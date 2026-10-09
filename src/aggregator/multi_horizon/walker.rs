//! Unified pixel walker for the continuous and categorical engines.
//!
//! Every valid pixel projects each sample position once, filters it by the
//! bounding box and mosaic tile ownership, and indexes it exactly at every
//! resolution. Consecutive contributions to the same cell accumulate in a
//! per-resolution run that is merged into the chunk map when the cell changes.
//! Single-band center sampling additionally batches contiguous pixels through
//! the engine's span kernels. Scratch storage is bounded by the sampling
//! pattern and resolution count, not by row or raster size.

use fxhash::FxBuildHasher;
use h3o::{LatLng, Resolution};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::ops::Range;

use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

pub use super::coordinates::{is_point_in_bbox, CoordinateTransformer, RAD_TO_DEG, WGS84_A};

/// Fast check if an entire row slice consists purely of NoData values using centralized NoDataRule
pub use crate::aggregator::nodata::is_slice_all_native_nodata;

/// Trait implemented by accumulator engines (continuous and categorical) to drive the generic scanline walker
pub trait ScanlineEngine<T, Acc> {
    type Sample: Copy;

    /// Opt in only when combining weights for the same pixel preserves semantics.
    fn combine_sample_weights(&self) -> bool {
        false
    }

    /// Allocate a fresh empty accumulator (or with quantiles if configured)
    fn new_acc(&self) -> Acc;

    /// Reset an accumulator to its zero/empty state
    fn clear_acc(&self, acc: &mut Acc);

    /// Check if accumulator contains valid observations
    fn has_samples(&self, acc: &Acc) -> bool;

    /// Merge the source accumulator into the destination
    fn merge_acc(&self, dest: &mut Acc, src: &Acc);

    /// Accumulate a span of pixels into a single accumulator
    fn accumulate_span(&self, acc: &mut Acc, slice: &[T]);

    /// Accumulate a span of pixels across multiple active resolution accumulators
    fn accumulate_span_multi(&self, run_accs: &mut [Acc], run_cells: &[u64], slice: &[T]);

    /// Extract a valid sample from a raw pixel, or None if nodata
    fn get_sample(&self, pixel: T) -> Option<Self::Sample>;

    /// Update an accumulator with a sample and weight
    fn update_sample(&self, acc: &mut Acc, sample: Self::Sample, weight: f64);
}

type CellMap<Acc> = HashMap<u64, Acc, FxBuildHasher>;

/// Borrowed inputs shared by every row of one chunk window.
#[derive(Clone, Copy)]
pub struct WalkContext<'a> {
    pub chunk: &'a RasterChunk,
    pub resolutions: &'a [Resolution],
    pub crs: &'a CrsTransformer,
    pub gt: &'a GeoTransform,
    pub sampling: &'a SamplingPattern,
    pub bbox: Option<[f64; 4]>,
    /// Physical row stride in pixels; 0 means the chunk width.
    pub stride: u32,
    /// Interleaved samples per pixel; 1 for single-band data.
    pub samples_per_pixel: u16,
    /// Keep only samples owned by this tile of an overlapping mosaic.
    pub owner: Option<(usize, &'a MosaicReader)>,
}

/// Walk a single-band window. Center sampling batches contiguous pixels
/// through the engine's span kernels.
pub fn walk_direct<T, Acc, E, N>(
    ctx: &WalkContext,
    slice: &[T],
    is_row_all_nodata: N,
    engine: &E,
    maps: &mut [CellMap<Acc>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    N: Fn(&[T]) -> bool,
{
    let batch = ctx.sampling.is_single_point();
    walk(
        ctx,
        slice,
        1,
        is_row_all_nodata,
        |px| engine.get_sample(px[0]),
        batch,
        engine,
        maps,
    );
}

/// Walk a window of `ctx.samples_per_pixel` interleaved values; `read` reduces
/// one pixel's values (a band selection or spectral index) to a sample.
pub fn walk_interleaved<T, Acc, E, R>(
    ctx: &WalkContext,
    slice: &[T],
    read: R,
    engine: &E,
    maps: &mut [CellMap<Acc>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    R: Fn(&[T]) -> Option<E::Sample>,
{
    walk(
        ctx,
        slice,
        ctx.samples_per_pixel as usize,
        |_| false,
        read,
        false,
        engine,
        maps,
    );
}

/// Single-band walk without mosaic ownership; retained for existing callers.
#[allow(clippy::too_many_arguments)]
pub fn scanline_walk<T, Acc, E, FNoData>(
    slice: &[T],
    chunk: &RasterChunk,
    resolutions: &[Resolution],
    crs_transformer: &CrsTransformer,
    gt: &GeoTransform,
    sampling: &SamplingPattern,
    bbox: Option<[f64; 4]>,
    chunk_stride: u32,
    is_row_all_nodata: FNoData,
    engine: &E,
    chunk_maps: &mut [CellMap<Acc>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    FNoData: Fn(&[T]) -> bool,
{
    let ctx = WalkContext {
        chunk,
        resolutions,
        crs: crs_transformer,
        gt,
        sampling,
        bbox,
        stride: chunk_stride,
        samples_per_pixel: 1,
        owner: None,
    };
    walk_direct(&ctx, slice, is_row_all_nodata, engine, chunk_maps);
}

#[allow(clippy::too_many_arguments)]
fn walk<T, Acc, E, N, R>(
    ctx: &WalkContext,
    slice: &[T],
    spp: usize,
    is_row_all_nodata: N,
    read: R,
    batch: bool,
    engine: &E,
    maps: &mut [CellMap<Acc>],
) where
    T: Copy,
    Acc: Clone,
    E: ScanlineEngine<T, Acc>,
    N: Fn(&[T]) -> bool,
    R: Fn(&[T]) -> Option<E::Sample>,
{
    let chunk = ctx.chunk;
    let width = chunk.width as usize;
    let spp = spp.max(1);
    if width == 0 || slice.is_empty() {
        return;
    }
    let stride = match ctx.stride as usize * spp {
        s if s > 0 && slice.len() >= s => s,
        _ => width * spp,
    };
    let num_res = ctx.resolutions.len();
    let combine = engine.combine_sample_weights();
    let pruner = RowPruner::new(ctx);
    let transformer = CoordinateTransformer::new(ctx.gt, ctx.crs);
    let mut runs = Runs::new(num_res, engine);
    let mut positions: Vec<(LatLng, f64)> = Vec::with_capacity(ctx.sampling.points.len());
    let mut assignments: Vec<(u64, f64)> = Vec::with_capacity(ctx.sampling.points.len());
    let mut cells = vec![0u64; num_res];

    for r in 0..chunk.height as usize {
        let start = r * stride;
        if start >= slice.len() {
            break;
        }
        let row_px = ((slice.len() - start) / spp).min(width);
        let row = &slice[start..start + row_px * spp];
        if is_row_all_nodata(row) {
            continue;
        }
        let row_idx = chunk.row_offset as usize + r;
        let cols = match &pruner {
            Some(p) => match p.columns(ctx, row_idx, row_px) {
                Some(cols) => cols,
                None => continue,
            },
            None => 0..row_px,
        };
        // Batch mode: pixels in `span_start..` all map to `runs.cells`.
        // Invalid pixels may sit inside a span; the span kernels skip them.
        let mut span_start: Option<usize> = None;

        for c in cols.clone() {
            let Some(sample) = read(&row[c * spp..(c + 1) * spp]) else {
                continue;
            };
            positions.clear();
            let col = chunk.col_offset as usize + c;
            for sp in &ctx.sampling.points {
                let Ok((lon, lat)) = transformer.subpixel_to_wgs84(col, row_idx, *sp) else {
                    continue;
                };
                if !(-90.0..=90.0).contains(&lat) || !is_point_in_bbox(lon, lat, ctx.bbox) {
                    continue;
                }
                if let Some((tile_idx, mosaic)) = ctx.owner {
                    if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                        continue;
                    }
                }
                if let Ok(ll) = LatLng::new(lat, lon) {
                    positions.push((ll, sp.weight));
                }
            }
            if positions.is_empty() {
                // A valid pixel outside the walk must not enter a span.
                if let Some(s) = span_start.take() {
                    runs.add_span(&row[s..c], engine);
                }
                continue;
            }

            if batch {
                let ll = positions[0].0;
                let mut changed = false;
                for (i, &res) in ctx.resolutions.iter().enumerate() {
                    cells[i] = u64::from(super::profile::index(ll, res));
                    changed |= cells[i] != runs.cells[i];
                }
                if changed {
                    if let Some(s) = span_start.take() {
                        runs.add_span(&row[s..c], engine);
                    }
                    for (i, map) in maps.iter_mut().enumerate() {
                        if cells[i] != runs.cells[i] {
                            runs.restart(i, cells[i], engine, map);
                        }
                    }
                }
                span_start.get_or_insert(c);
                continue;
            }

            for (i, &res) in ctx.resolutions.iter().enumerate() {
                assignments.clear();
                assignments.extend(
                    positions
                        .iter()
                        .map(|&(ll, w)| (u64::from(super::profile::index(ll, res)), w)),
                );
                let first = assignments[0].0;
                // A pixel wholly inside one cell contributes its retained
                // weight once; custom patterns need not sum to one.
                if assignments.len() == 1
                    || (combine && assignments.iter().all(|&(cell, _)| cell == first))
                {
                    let weight = assignments.iter().map(|a| a.1).sum::<f64>();
                    if weight.is_finite() {
                        runs.update(i, first, sample, weight, engine, &mut maps[i]);
                        continue;
                    }
                }
                // Preserve sample order for boundary pixels and quantile sketches.
                for &(cell, weight) in &assignments {
                    runs.update(i, cell, sample, weight, engine, &mut maps[i]);
                }
            }
        }
        if let Some(s) = span_start {
            runs.add_span(&row[s..cols.end], engine);
        }
    }
    runs.finish(engine, maps);
}

/// Current cell and partial accumulator per resolution.
struct Runs<Acc> {
    cells: Vec<u64>,
    accs: Vec<Acc>,
}

impl<Acc: Clone> Runs<Acc> {
    fn new<T, E: ScanlineEngine<T, Acc>>(num_res: usize, engine: &E) -> Self {
        Self {
            cells: vec![0; num_res],
            accs: (0..num_res).map(|_| engine.new_acc()).collect(),
        }
    }

    #[inline]
    fn restart<T, E: ScanlineEngine<T, Acc>>(
        &mut self,
        i: usize,
        cell: u64,
        engine: &E,
        map: &mut CellMap<Acc>,
    ) {
        flush_run(map, self.cells[i], &mut self.accs[i], engine);
        self.cells[i] = cell;
    }

    #[inline]
    fn update<T, E: ScanlineEngine<T, Acc>>(
        &mut self,
        i: usize,
        cell: u64,
        sample: E::Sample,
        weight: f64,
        engine: &E,
        map: &mut CellMap<Acc>,
    ) {
        if cell != self.cells[i] {
            self.restart(i, cell, engine, map);
        }
        engine.update_sample(&mut self.accs[i], sample, weight);
    }

    #[inline]
    fn add_span<T, E: ScanlineEngine<T, Acc>>(&mut self, values: &[T], engine: &E) {
        if let [acc] = self.accs.as_mut_slice() {
            engine.accumulate_span(acc, values);
        } else {
            engine.accumulate_span_multi(&mut self.accs, &self.cells, values);
        }
    }

    fn finish<T, E: ScanlineEngine<T, Acc>>(mut self, engine: &E, maps: &mut [CellMap<Acc>]) {
        for (i, map) in maps.iter_mut().enumerate() {
            flush_run(map, self.cells[i], &mut self.accs[i], engine);
        }
    }
}

/// Merge a finished run into the chunk map, moving rather than cloning new entries.
#[inline]
fn flush_run<T, Acc, E: ScanlineEngine<T, Acc>>(
    map: &mut CellMap<Acc>,
    cell: u64,
    acc: &mut Acc,
    engine: &E,
) {
    if cell == 0 || !engine.has_samples(acc) {
        return;
    }
    match map.entry(cell) {
        Entry::Occupied(mut entry) => {
            engine.merge_acc(entry.get_mut(), acc);
            engine.clear_acc(acc);
        }
        Entry::Vacant(entry) => {
            entry.insert(std::mem::replace(acc, engine.new_acc()));
        }
    }
}

/// Conservative row and column limits for bbox queries on north-up WGS84 and
/// Web Mercator grids, where latitude depends only on the row and longitude is
/// affine in the column. Every retained sample is still tested exactly.
struct RowPruner {
    bbox: [f64; 4],
    lon0: f64,
    lon_step: f64,
    dx_min: f64,
    dx_max: f64,
}

impl RowPruner {
    fn new(ctx: &WalkContext) -> Option<Self> {
        let bbox = ctx.bbox?;
        let gt = ctx.gt;
        if !(gt.b == 0.0 && gt.d == 0.0 && gt.a > 0.0 && gt.e < 0.0) {
            return None;
        }
        let (lon0, lon_step) = match ctx.crs {
            CrsTransformer::Wgs84Identity => (gt.c0, gt.a),
            CrsTransformer::WebMercatorFast => {
                (gt.c0 / WGS84_A * RAD_TO_DEG, gt.a / WGS84_A * RAD_TO_DEG)
            }
            _ => return None,
        };
        let points = &ctx.sampling.points;
        Some(Self {
            bbox,
            lon0,
            lon_step,
            dx_min: points.iter().map(|p| p.dx).fold(f64::INFINITY, f64::min),
            dx_max: points
                .iter()
                .map(|p| p.dx)
                .fold(f64::NEG_INFINITY, f64::max),
        })
    }

    /// Columns of this row that may hold a sample inside the bbox, or None
    /// when no sample of the row can be inside it.
    fn columns(&self, ctx: &WalkContext, row_idx: usize, row_px: usize) -> Option<Range<usize>> {
        let [min_lon, min_lat, max_lon, max_lat] = self.bbox;

        // Same expressions as sample projection, so the latitude test is exact.
        let (mut lat_lo, mut lat_hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for sp in &ctx.sampling.points {
            let (x, y) = ctx.gt.pixel_to_coord(0.0, row_idx as f64 + sp.dy);
            let Ok((_, lat)) = ctx.crs.transform_point(x, y) else {
                return Some(0..row_px);
            };
            lat_lo = lat_lo.min(lat);
            lat_hi = lat_hi.max(lat);
        }
        if lat_hi < min_lat || lat_lo > max_lat {
            return None;
        }

        // Longitude windows are only linear when neither the bbox nor the row
        // wraps; otherwise leave the exact per-sample test to decide.
        let col0 = ctx.chunk.col_offset as f64;
        let row_west = self.lon0 + col0 * self.lon_step;
        let row_east = self.lon0 + (col0 + row_px as f64) * self.lon_step;
        if min_lon > max_lon || row_west < -180.0 || row_east > 180.0 {
            return Some(0..row_px);
        }
        // Pad one column on each side to absorb rounding in the linear model.
        let first = ((min_lon - self.lon0) / self.lon_step - self.dx_max - col0).floor() - 1.0;
        let last = ((max_lon - self.lon0) / self.lon_step - self.dx_min - col0).floor() + 2.0;
        if !first.is_finite() || !last.is_finite() {
            return Some(0..row_px);
        }
        let clamp = |v: f64| v.clamp(0.0, row_px as f64) as usize;
        let cols = clamp(first)..clamp(last);
        (!cols.is_empty()).then_some(cols)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_point_in_bbox() {
        let bbox = Some([-122.5, 37.5, -122.0, 38.0]);
        assert!(is_point_in_bbox(-122.25, 37.75, bbox));
        assert!(is_point_in_bbox(-122.5, 37.5, bbox));
        assert!(is_point_in_bbox(-122.0, 38.0, bbox));
        assert!(!is_point_in_bbox(-122.6, 37.75, bbox));
        assert!(!is_point_in_bbox(-122.25, 38.1, bbox));
        assert!(is_point_in_bbox(0.0, 0.0, None));

        // Antimeridian-crossing bbox (e.g. Fiji [178.0, -20.0, -178.0, -15.0])
        let fiji_bbox = Some([178.0, -20.0, -178.0, -15.0]);
        assert!(is_point_in_bbox(179.0, -18.0, fiji_bbox));
        assert!(is_point_in_bbox(-179.0, -18.0, fiji_bbox));
        // Unnormalized longitude wrapping (181.0 -> -179.0)
        assert!(is_point_in_bbox(181.0, -18.0, fiji_bbox));
        // Outside longitude
        assert!(!is_point_in_bbox(175.0, -18.0, fiji_bbox));
        assert!(!is_point_in_bbox(-175.0, -18.0, fiji_bbox));
        // Outside latitude
        assert!(!is_point_in_bbox(179.0, -21.0, fiji_bbox));
    }

    fn pruned(c0: f64, bbox: [f64; 4], sampling: &SamplingPattern) -> Option<Range<usize>> {
        let gt = GeoTransform {
            c0,
            a: 0.1,
            b: 0.0,
            f0: -17.0,
            d: 0.0,
            e: -0.1,
        };
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 200,
            height: 1,
        };
        let ctx = WalkContext {
            chunk: &chunk,
            resolutions: &[],
            crs: &CrsTransformer::Wgs84Identity,
            gt: &gt,
            sampling,
            bbox: Some(bbox),
            stride: 200,
            samples_per_pixel: 1,
            owner: None,
        };
        RowPruner::new(&ctx).unwrap().columns(&ctx, 0, 200)
    }

    #[test]
    fn row_pruner_is_conservative_and_never_prunes_wrapped_rows() {
        let center = SamplingPattern::center();
        // Centers 160.05 + 0.1c inside [170, 171] -> c in 100..=109.
        let cols = pruned(160.0, [170.0, -20.0, 171.0, -15.0], &center).unwrap();
        assert!(cols.start <= 100 && cols.end >= 110 && cols.len() <= 14);
        // Antimeridian bboxes and rows past 180 keep every column.
        let fiji = [178.0, -20.0, -178.0, -15.0];
        assert_eq!(pruned(160.0, fiji, &center), Some(0..200));
        assert_eq!(
            pruned(170.0, [-179.0, -20.0, -178.0, -15.0], &center),
            Some(0..200)
        );
        // Latitude outside the bbox drops the row.
        assert_eq!(pruned(160.0, [170.0, 0.0, 171.0, 1.0], &center), None);
        // Supersampled pixels reach further than their centers.
        let nine = SamplingPattern::nine_point();
        let cols = pruned(160.0, [170.0, -20.0, 171.0, -15.0], &nine).unwrap();
        assert!(cols.start <= 99 && cols.end >= 111);
    }
}

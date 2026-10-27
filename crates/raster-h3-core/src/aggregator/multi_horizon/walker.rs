//! Unified pixel walker for the continuous and categorical engines.
//!
//! Every valid pixel projects each sample position once, filters it by the
//! bounding box and mosaic tile ownership, and indexes it exactly at every
//! resolution. Consecutive contributions to the same cell accumulate in a
//! per-resolution run that is merged into the chunk map when the cell changes.
//! Single-band center sampling additionally batches contiguous pixels through
//! the engine's span kernels. Scratch storage is bounded by the sampling
//! pattern and resolution count, not by row or raster size.

use crate::aggregator::cell_hash::H3BuildHasher;
use h3o::{LatLng, LatLngIndexer, Resolution};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::ops::Range;

use crate::aggregator::sampling::{SamplePoint, SamplingPattern};
use crate::crs::transformer::{web_mercator_lat, web_mercator_lon, AlbersInverse, CrsTransformer};
use crate::raster::geotransform::GeoTransform;
use crate::raster::metadata::BandScaling;
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

    /// Accumulate a span of pixels into every resolution's accumulator
    fn accumulate_span_multi(&self, run_accs: &mut [Acc], slice: &[T]);

    /// Extract a valid sample from a raw pixel, or None if nodata
    fn get_sample(&self, pixel: T) -> Option<Self::Sample>;

    /// Update an accumulator with a sample and weight
    fn update_sample(&self, acc: &mut Acc, sample: Self::Sample, weight: f64);
}

type CellMap<Acc> = HashMap<u64, Acc, H3BuildHasher>;

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
    /// Per-band stored-to-physical calibration, indexed by zero-based band;
    /// empty means stored values are used unchanged.
    pub band_scaling: &'a [BandScaling],
    /// Zero-based interleaved alpha sample; pixels with zero alpha are skipped.
    pub alpha_sample: Option<u16>,
}

impl WalkContext<'_> {
    /// Calibration for the 1-based `band`, or identity when uncalibrated.
    #[inline]
    pub fn scaling(&self, band: usize) -> BandScaling {
        self.band_scaling
            .get(band.saturating_sub(1))
            .copied()
            .unwrap_or(BandScaling::IDENTITY)
    }
}

/// Whether an interleaved pixel's alpha sample marks it fully transparent.
#[inline(always)]
pub fn is_transparent<T: Copy + Default + PartialEq>(ctx: &WalkContext<'_>, px: &[T]) -> bool {
    ctx.alpha_sample
        .and_then(|a| px.get(a as usize))
        .is_some_and(|&alpha| alpha == T::default())
}

/// Walk a single-band window. Center sampling batches contiguous pixels
/// through the engine's span kernels.
pub fn walk_direct<T, Acc, E, N>(
    ctx: &WalkContext<'_>,
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
    ctx: &WalkContext<'_>,
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
        band_scaling: &[],
        alpha_sample: None,
    };
    walk_direct(&ctx, slice, is_row_all_nodata, engine, chunk_maps);
}

#[allow(clippy::too_many_arguments)]
fn walk<T, Acc, E, N, R>(
    ctx: &WalkContext<'_>,
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
    let mut projector = SampleProjector::new(ctx);
    let mut runs = Runs::new(num_res, engine);
    let mut positions: Vec<(LatLng, f64)> = Vec::with_capacity(ctx.sampling.points.len());
    let mut assignments: Vec<(u64, f64)> = Vec::with_capacity(ctx.sampling.points.len());
    let mut position_cells: Vec<u64> = Vec::with_capacity(ctx.sampling.points.len() * num_res);

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
        projector.start_row(ctx, row_idx);
        // Batch mode: pixels in `span_start..` all map to `runs.cells`.
        // Invalid pixels may sit inside a span; the span kernels skip them.
        let mut span_start: Option<usize> = None;

        if batch && ctx.owner.is_none() && ctx.bbox.is_none() {
            let sp = ctx.sampling.points[0];
            let col_offset = chunk.col_offset as usize;
            for c in cols.clone() {
                if read(&row[c * spp..(c + 1) * spp]).is_none() {
                    continue;
                }
                let Some(ll) = projector
                    .project(col_offset + c, row_idx, 0, sp)
                    .filter(|&(_, lat)| (-90.0..=90.0).contains(&lat))
                    .and_then(|(lon, lat)| LatLng::new(lat, lon).ok())
                else {
                    if let Some(s) = span_start.take() {
                        runs.add_span(&row[s..c], engine);
                    }
                    continue;
                };
                runs.advance_cells(
                    ll,
                    ctx.resolutions,
                    &row[..c],
                    &mut span_start,
                    engine,
                    maps,
                );
            }
            if let Some(s) = span_start {
                runs.add_span(&row[s..cols.end], engine);
            }
            continue;
        }

        for c in cols.clone() {
            let Some(sample) = read(&row[c * spp..(c + 1) * spp]) else {
                continue;
            };
            positions.clear();
            let col = chunk.col_offset as usize + c;
            for (k, sp) in ctx.sampling.points.iter().enumerate() {
                let Some((lon, lat)) = projector.project(col, row_idx, k, *sp) else {
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
                runs.advance_cells(
                    positions[0].0,
                    ctx.resolutions,
                    &row[..c],
                    &mut span_start,
                    engine,
                    maps,
                );
                continue;
            }

            // Row-major by position: `position_cells[p * num_res + i]`.
            position_cells.resize(positions.len() * num_res, 0);
            for (&(ll, _), cells) in positions
                .iter()
                .zip(position_cells.chunks_exact_mut(num_res))
            {
                super::profile::index_all(&mut runs.indexer, ll, ctx.resolutions, cells);
            }
            for i in 0..num_res {
                assignments.clear();
                assignments.extend(
                    positions
                        .iter()
                        .zip(position_cells.chunks_exact(num_res))
                        .map(|(&(_, w), cells)| (cells[i], w)),
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

/// Row-dependent projection terms. Horizontal shear is allowed: only d affects y.
enum RowCache<'a> {
    None,
    Wgs84(Vec<f64>),
    Mercator(Vec<f64>),
    Albers(AlbersInverse<'a>, Option<Vec<(f64, f64)>>),
}

struct SampleProjector<'a> {
    transformer: CoordinateTransformer<'a>,
    gt: &'a GeoTransform,
    cache: RowCache<'a>,
}

impl<'a> SampleProjector<'a> {
    fn new(ctx: &WalkContext<'a>) -> Self {
        let row_invariant = ctx.gt.d == 0.0;
        let capacity = ctx.sampling.points.len();
        let cache = match ctx.crs {
            CrsTransformer::Wgs84Identity if row_invariant => {
                RowCache::Wgs84(Vec::with_capacity(capacity))
            }
            CrsTransformer::WebMercatorFast if row_invariant => {
                RowCache::Mercator(Vec::with_capacity(capacity))
            }
            CrsTransformer::AlbersConic(albers) => RowCache::Albers(
                albers.prepare_inverse(),
                row_invariant.then(|| Vec::with_capacity(capacity)),
            ),
            _ => RowCache::None,
        };
        Self {
            transformer: CoordinateTransformer::new(ctx.gt, ctx.crs),
            gt: ctx.gt,
            cache,
        }
    }

    fn start_row(&mut self, ctx: &WalkContext<'_>, row_idx: usize) {
        let row_y = |sp: &SamplePoint| self.gt.pixel_to_coord(0.0, row_idx as f64 + sp.dy).1;
        match &mut self.cache {
            RowCache::Wgs84(lats) => {
                lats.clear();
                lats.extend(ctx.sampling.points.iter().map(row_y));
            }
            RowCache::Mercator(lats) => {
                lats.clear();
                lats.extend(
                    ctx.sampling
                        .points
                        .iter()
                        .map(|sp| web_mercator_lat(row_y(sp))),
                );
            }
            RowCache::Albers(inverse, Some(yps)) => {
                yps.clear();
                yps.extend(
                    ctx.sampling
                        .points
                        .iter()
                        .map(|sp| inverse.row_yp(row_y(sp))),
                );
            }
            _ => {}
        }
    }

    /// Project sampling point k, skipping samples outside the projection's domain.
    #[inline(always)]
    fn project(&self, col: usize, row_idx: usize, k: usize, sp: SamplePoint) -> Option<(f64, f64)> {
        if matches!(self.cache, RowCache::None) {
            return self.transformer.subpixel_to_wgs84(col, row_idx, sp).ok();
        }
        super::profile::measure_transform(|| {
            let (x, y) = self
                .gt
                .pixel_to_coord(col as f64 + sp.dx, row_idx as f64 + sp.dy);
            let (lon, lat) = match &self.cache {
                RowCache::Wgs84(lats) => (x, lats[k]),
                RowCache::Mercator(lats) => (web_mercator_lon(x), lats[k]),
                RowCache::Albers(inverse, Some(yps)) => {
                    let (y_p, y_p_sq) = yps[k];
                    inverse.transform_with_yp(x, y_p, y_p_sq)
                }
                RowCache::Albers(inverse, None) => inverse.transform_point(x, y),
                RowCache::None => unreachable!(),
            };
            (lon.is_finite() && lat.is_finite()).then_some((lon, lat))
        })
    }
}

/// Current cell (None before the first) and partial accumulator per
/// resolution, and the indexer that remembers recent cells across samples.
struct Runs<Acc> {
    cells: Vec<Option<u64>>,
    next_cells: Vec<u64>,
    accs: Vec<Acc>,
    indexer: LatLngIndexer,
}

impl<Acc: Clone> Runs<Acc> {
    fn new<T, E: ScanlineEngine<T, Acc>>(num_res: usize, engine: &E) -> Self {
        Self {
            cells: vec![None; num_res],
            next_cells: vec![0; num_res],
            accs: (0..num_res).map(|_| engine.new_acc()).collect(),
            indexer: LatLngIndexer::new(),
        }
    }

    /// Index every resolution and flush the preceding span before restarting any run.
    #[inline]
    fn advance_cells<T, E: ScanlineEngine<T, Acc>>(
        &mut self,
        ll: LatLng,
        resolutions: &[Resolution],
        preceding: &[T],
        span_start: &mut Option<usize>,
        engine: &E,
        maps: &mut [CellMap<Acc>],
    ) {
        super::profile::index_all(&mut self.indexer, ll, resolutions, &mut self.next_cells);
        let changed = self
            .next_cells
            .iter()
            .zip(&self.cells)
            .any(|(&next, &cell)| Some(next) != cell);
        if changed {
            if let Some(s) = span_start.take() {
                self.add_span(&preceding[s..], engine);
            }
            for (i, map) in maps.iter_mut().enumerate() {
                let cell = self.next_cells[i];
                if Some(cell) != self.cells[i] {
                    self.restart(i, cell, engine, map);
                }
            }
        }
        span_start.get_or_insert(preceding.len());
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
        self.cells[i] = Some(cell);
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
        if Some(cell) != self.cells[i] {
            self.restart(i, cell, engine, map);
        }
        engine.update_sample(&mut self.accs[i], sample, weight);
    }

    /// Only called once every resolution has a current cell.
    #[inline]
    fn add_span<T, E: ScanlineEngine<T, Acc>>(&mut self, values: &[T], engine: &E) {
        if let [acc] = self.accs.as_mut_slice() {
            engine.accumulate_span(acc, values);
        } else {
            engine.accumulate_span_multi(&mut self.accs, values);
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
    cell: Option<u64>,
    acc: &mut Acc,
    engine: &E,
) {
    let Some(cell) = cell.filter(|_| engine.has_samples(acc)) else {
        return;
    };
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
    fn new(ctx: &WalkContext<'_>) -> Option<Self> {
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
    fn columns(
        &self,
        ctx: &WalkContext<'_>,
        row_idx: usize,
        row_px: usize,
    ) -> Option<Range<usize>> {
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
            band_scaling: &[],
            alpha_sample: None,
        };
        RowPruner::new(&ctx).unwrap().columns(&ctx, 0, 200)
    }

    #[test]
    fn mercator_row_latitudes_match_per_sample_projection_bitwise() {
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 1,
            height: 1,
        };
        for gt in [
            // North-up, origin north of the equator.
            GeoTransform {
                c0: -13_627_665.27,
                a: 30.0,
                b: 0.0,
                f0: 4_547_675.35,
                d: 0.0,
                e: -30.0,
            },
            // Southern hemisphere, origin at -0.0, south-up rows.
            GeoTransform {
                c0: 0.0,
                a: 152.874_057_3,
                b: 0.0,
                f0: -0.0,
                d: -0.0,
                e: 152.874_057_3,
            },
        ] {
            for sampling in [SamplingPattern::center(), SamplingPattern::sixteen_point()] {
                let ctx = WalkContext {
                    chunk: &chunk,
                    resolutions: &[],
                    crs: &CrsTransformer::WebMercatorFast,
                    gt: &gt,
                    sampling: &sampling,
                    bbox: None,
                    stride: 1,
                    samples_per_pixel: 1,
                    owner: None,
                    band_scaling: &[],
                    alpha_sample: None,
                };
                let mut projector = SampleProjector::new(&ctx);
                assert!(matches!(projector.cache, RowCache::Mercator(_)));
                let reference = CoordinateTransformer::new(&gt, ctx.crs);
                for row in [0usize, 1, 7, 4_095, 100_000] {
                    projector.start_row(&ctx, row);
                    for col in [0usize, 3, 65_535] {
                        for (k, sp) in sampling.points.iter().enumerate() {
                            let (lon, lat) = projector.project(col, row, k, *sp).unwrap();
                            let (rlon, rlat) = reference.subpixel_to_wgs84(col, row, *sp).unwrap();
                            assert_eq!(lon.to_bits(), rlon.to_bits());
                            assert_eq!(lat.to_bits(), rlat.to_bits());
                        }
                    }
                }
            }
        }
    }

    fn check_row_cache_shear(d: f64) {
        use crate::crs::transformer::AlbersConicFast;

        let chunk = RasterChunk {
            col_offset: 3,
            row_offset: 7,
            width: 4,
            height: 2,
        };
        for crs in [
            CrsTransformer::Wgs84Identity,
            CrsTransformer::WebMercatorFast,
            CrsTransformer::AlbersConic(AlbersConicFast::epsg_5070()),
            CrsTransformer::AlbersConic(AlbersConicFast::with_offsets(
                -18.0, -36.0, -25.0, 132.0, 12345.0, -9876.0,
            )),
        ] {
            let gt = GeoTransform {
                c0: 1.0,
                a: 0.1,
                b: 0.03,
                f0: 2.0,
                d,
                e: -0.1,
            };
            for sampling in [SamplingPattern::center(), SamplingPattern::rgss()] {
                let ctx = WalkContext {
                    chunk: &chunk,
                    resolutions: &[],
                    crs: &crs,
                    gt: &gt,
                    sampling: &sampling,
                    bbox: None,
                    stride: 4,
                    samples_per_pixel: 1,
                    owner: None,
                    band_scaling: &[],
                    alpha_sample: None,
                };
                let mut projector = SampleProjector::new(&ctx);
                match (&crs, &projector.cache) {
                    (CrsTransformer::Wgs84Identity, RowCache::Wgs84(_))
                    | (CrsTransformer::WebMercatorFast, RowCache::Mercator(_))
                    | (CrsTransformer::AlbersConic(_), RowCache::Albers(_, Some(_))) => {
                        assert_eq!(d, 0.0)
                    }
                    (_, RowCache::None | RowCache::Albers(_, None)) => assert_ne!(d, 0.0),
                    _ => panic!("incorrect row-cache mode"),
                }
                #[cfg(feature = "stream-profile")]
                let scope = super::super::profile::WorkerScope::new();
                for row in [7, 8] {
                    projector.start_row(&ctx, row);
                    for col in [3, 4, 6] {
                        for (k, &sp) in sampling.points.iter().enumerate() {
                            let (x, y) = gt.pixel_to_coord(col as f64 + sp.dx, row as f64 + sp.dy);
                            assert_eq!(
                                projector.project(col, row, k, sp),
                                Some(crs.transform_point(x, y).unwrap())
                            );
                        }
                    }
                }
                #[cfg(feature = "stream-profile")]
                assert_eq!(
                    scope.snapshot().transform_calls,
                    (6 * sampling.points.len()) as u64
                );
            }
        }
    }

    #[test]
    fn horizontal_shear_keeps_row_hoisting_and_exact_coordinates() {
        check_row_cache_shear(0.0);
    }

    #[test]
    fn vertical_shear_disables_row_hoisting_and_keeps_exact_coordinates() {
        check_row_cache_shear(0.02);
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

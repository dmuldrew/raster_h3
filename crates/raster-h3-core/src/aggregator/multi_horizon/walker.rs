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
use h3o::{GeographicRow, LatLng, LatLngIndexer, Resolution, RowPath, SpanBounds};
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::ops::Range;

use crate::aggregator::sampling::{SamplePoint, SamplingPattern};
use crate::crs::transformer::{
    mercator_line_bounds, web_mercator_lat, web_mercator_lon, wgs84_line_bounds, AlbersInverse,
    CrsTransformer, RowPathBounds,
};
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
    let mut row_indexer = RowIndexer::take(num_res, ctx.sampling.points.len());
    let total_weight = ctx.sampling.points.iter().map(|sp| sp.weight).sum::<f64>();
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

        if !batch && ctx.owner.is_none() && ctx.bbox.is_none() {
            let col_offset = chunk.col_offset as usize;
            if row_indexer.index_samples(&projector, ctx, row_idx, col_offset, &cols) {
                row_indexer.replay_samples(
                    cols.clone(),
                    row,
                    spp,
                    &read,
                    ctx,
                    (combine, total_weight),
                    &mut runs,
                    engine,
                    maps,
                );
                continue;
            }
        }

        if batch && ctx.owner.is_none() && ctx.bbox.is_none() {
            let sp = ctx.sampling.points[0];
            let col_offset = chunk.col_offset as usize;
            if let Some(mut path) = projector.grid_row(row_idx, col_offset, 0, sp, &cols) {
                row_indexer.walk(
                    &mut path,
                    cols.clone(),
                    row,
                    spp,
                    &read,
                    ctx.resolutions,
                    &mut runs,
                    engine,
                    maps,
                );
                continue;
            }
            for c in cols.clone() {
                if read(&row[c * spp..(c + 1) * spp]).is_none() {
                    continue;
                }
                let Some(ll) = projector
                    .project(col_offset + c, row_idx, 0, sp)
                    .filter(|&(_, lat)| (-90.0..=90.0).contains(&lat))
                    .and_then(|(lon, lat)| LatLng::new(lat, lon).ok())
                else {
                    super::profile::transform_failed();
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
                let Some((lon, lat)) = projector
                    .project(col, row_idx, k, *sp)
                    .filter(|&(_, lat)| (-90.0..=90.0).contains(&lat))
                else {
                    super::profile::transform_failed();
                    continue;
                };
                if !is_point_in_bbox(lon, lat, ctx.bbox) {
                    continue;
                }
                if let Some((tile_idx, mosaic)) = ctx.owner {
                    if !mosaic.is_point_owned_by(tile_idx, lon, lat) {
                        continue;
                    }
                }
                match LatLng::new(lat, lon) {
                    Ok(ll) => positions.push((ll, sp.weight)),
                    Err(_) => super::profile::transform_failed(),
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
    row_indexer.release();
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
    crs: &'a CrsTransformer,
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
            crs: ctx.crs,
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

    /// Sample point `k` of a raster row, as an exact `RowPath`: `point(c)`
    /// reproduces `project(col_offset + c, row_idx, k, sp)` bit for bit.
    ///
    /// North-up WGS84 and Web Mercator rows are parallels (constant latitude,
    /// longitude affine in the column); north-up Albers rows have constant
    /// projected y, with bounds derived from the analytic inverse. Rotated or
    /// sheared rows (`gt.d != 0`) are straight lines of projected
    /// coordinates, bounded per span. `None` keeps the per-pixel path (other
    /// CRSs, rows that may reach a pole or the Albers cone apex).
    fn grid_row(
        &self,
        row_idx: usize,
        col_offset: usize,
        k: usize,
        sp: SamplePoint,
        cols: &Range<usize>,
    ) -> Option<GridRow<'_>> {
        if !exact_rows_enabled() {
            return None;
        }
        let gt = self.gt;
        if ![gt.c0, gt.a, gt.b, gt.f0, gt.d, gt.e, sp.dx, sp.dy]
            .iter()
            .all(|v| v.is_finite())
        {
            return None;
        }
        let row = row_idx as f64 + sp.dy;
        let mut path = GridRow {
            gt,
            row,
            col_offset,
            dx: sp.dx,
            kind: RowKind::Wgs84Line,
            bounds: RowPathBounds {
                speed: 0.0,
                accel: 0.0,
                noise: 0.0,
            },
        };
        match (&self.cache, self.crs) {
            (RowCache::Wgs84(lats) | RowCache::Mercator(lats), _) => {
                let lat = *lats.get(k)?;
                if !(-90.0..=90.0).contains(&lat) {
                    return None;
                }
                let mercator = matches!(self.cache, RowCache::Mercator(_));
                // Longitude step per column, in degrees.
                let step = if mercator {
                    web_mercator_lon(gt.a)
                } else {
                    gt.a
                };
                let bounds = GeographicRow::new(lat, 0.0, step);
                path.kind = RowKind::Geographic { lat, mercator };
                path.bounds = RowPathBounds {
                    speed: bounds.speed_bound(),
                    accel: bounds.accel_bound(),
                    noise: 0.0,
                };
            }
            (RowCache::Albers(inverse, Some(yps)), _) => {
                let (y_p, y_p_sq) = *yps.get(k)?;
                // `index_row` samples `cols.start..=cols.end`.
                let (x0, x1) = (path.coord(cols.start).0, path.coord(cols.end).0);
                path.bounds = inverse.row_path_bounds(x0.min(x1), x0.max(x1), y_p, gt.a)?;
                path.kind = RowKind::Albers {
                    inverse,
                    y_p,
                    y_p_sq,
                };
            }
            (RowCache::Albers(inverse, None), _) => {
                path.kind = RowKind::AlbersLine(inverse);
                path.bounds = path.line_bounds(cols.start, cols.end)?;
            }
            (RowCache::None, CrsTransformer::Wgs84Identity) => {
                path.bounds = path.line_bounds(cols.start, cols.end)?;
            }
            (RowCache::None, CrsTransformer::WebMercatorFast) => {
                path.kind = RowKind::MercatorLine;
                path.bounds = path.line_bounds(cols.start, cols.end)?;
            }
            _ => return None,
        }
        Some(path)
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

/// Whether rows of constant latitude use `LatLngIndexer::index_row` (on unless
/// `RASTER_H3_EXACT_ROWS=0`, for A/B comparisons).
fn exact_rows_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RASTER_H3_EXACT_ROWS").map_or(true, |v| v != "0"))
}

/// How a [`GridRow`] turns projected coordinates into a location.
enum RowKind<'a> {
    /// North-up: constant latitude; longitude is x (WGS84) or
    /// `web_mercator_lon(x)`.
    Geographic {
        lat: f64,
        mercator: bool,
    },
    /// North-up Albers with the row's hoisted `y_p` terms.
    Albers {
        inverse: &'a AlbersInverse<'a>,
        y_p: f64,
        y_p_sq: f64,
    },
    /// Rotated or sheared rows, where x and y both change along the row:
    /// `CrsTransformer::transform_point(x, y)` for each CRS.
    Wgs84Line,
    MercatorLine,
    AlbersLine(&'a AlbersInverse<'a>),
}

/// See [`SampleProjector::grid_row`].
struct GridRow<'a> {
    gt: &'a GeoTransform,
    row: f64,
    col_offset: usize,
    dx: f64,
    kind: RowKind<'a>,
    /// Bounds for the whole call.
    bounds: RowPathBounds,
}

impl GridRow<'_> {
    /// Projected coordinates of column `c`, exactly as `SampleProjector`
    /// computes them.
    #[inline(always)]
    fn coord(&self, c: usize) -> (f64, f64) {
        self.gt
            .pixel_to_coord((self.col_offset + c) as f64 + self.dx, self.row)
    }

    /// Bounds for the locations `first..=last` of a rotated or sheared row
    /// (the `*Line` kinds): a straight line of projected coordinates.
    fn line_bounds(&self, first: usize, last: usize) -> Option<RowPathBounds> {
        let gt = self.gt;
        let (start, end) = (self.coord(first), self.coord(last));
        let step = (gt.a, gt.d);
        let col = |c: usize| ((self.col_offset + c) as f64 + self.dx).abs();
        let col_max = col(first).max(col(last));
        // Magnitude of the terms `pixel_to_coord` sums, which sizes the
        // rounding of x and y.
        let coord_mag = gt.c0.abs()
            + (col_max * gt.a).abs()
            + (self.row * gt.b).abs()
            + gt.f0.abs()
            + (col_max * gt.d).abs()
            + (self.row * gt.e).abs();
        match self.kind {
            RowKind::Wgs84Line => wgs84_line_bounds(start, end, step, coord_mag),
            RowKind::MercatorLine => mercator_line_bounds(start, end, step, coord_mag),
            RowKind::AlbersLine(inverse) => inverse.line_path_bounds(start, end, step, coord_mag),
            RowKind::Geographic { .. } | RowKind::Albers { .. } => None,
        }
    }
}

impl RowPath for GridRow<'_> {
    #[inline]
    fn point(&mut self, c: usize) -> LatLng {
        let (x, y) = self.coord(c);
        let (lon, lat) = match self.kind {
            RowKind::Geographic { lat, mercator } => {
                (if mercator { web_mercator_lon(x) } else { x }, lat)
            }
            RowKind::Albers {
                inverse,
                y_p,
                y_p_sq,
            } => inverse.transform_with_yp(x, y_p, y_p_sq),
            RowKind::Wgs84Line => (x, y),
            RowKind::MercatorLine => (web_mercator_lon(x), web_mercator_lat(y)),
            RowKind::AlbersLine(inverse) => inverse.transform_point(x, y),
        };
        LatLng::new(lat, lon).expect("finite coordinate")
    }

    fn speed_bound(&self) -> f64 {
        self.bounds.speed
    }

    fn accel_bound(&self) -> f64 {
        self.bounds.accel
    }

    fn noise_bound(&self) -> f64 {
        self.bounds.noise
    }

    /// Rotated rows report bounds per span: near a pole (WGS84, Web
    /// Mercator) or the cone apex (Albers), the path-wide bounds can be much
    /// looser than a short span's.
    fn span_bounds(&mut self, first: usize, last: usize) -> Option<SpanBounds> {
        let bounds = match self.kind {
            RowKind::Geographic { .. } | RowKind::Albers { .. } => self.bounds,
            RowKind::Wgs84Line | RowKind::MercatorLine | RowKind::AlbersLine(_) => {
                self.line_bounds(first, last)?
            }
        };
        Some(SpanBounds {
            speed: bounds.speed,
            accel: bounds.accel,
            noise: bounds.noise,
        })
    }
}

/// Indexes whole rows with `LatLngIndexer::index_row` and replays the cell
/// changes through `Runs` exactly as the per-pixel loops would.
#[derive(Default)]
struct RowIndexer {
    indexer: LatLngIndexer,
    /// (first column, cell) of each run of the current row, per resolution
    /// and sample point: `row_runs[i * num_samples + k]`.
    row_runs: Vec<Vec<(usize, u64)>>,
    cursor: Vec<usize>,
    num_samples: usize,
    /// Each sample point's cell in the current segment, laid out like `row_runs`.
    segment_cells: Vec<u64>,
}

thread_local! {
    /// Per-thread `RowIndexer` reused across chunks, so a stream of
    /// small chunks doesn't reallocate (and regrow) its row buffers each time.
    static ROW_INDEXER: RefCell<RowIndexer> = RefCell::default();
}

impl RowIndexer {
    /// This thread's indexer, sized for `num_res * num_samples` row paths.
    /// Hand it back with [`Self::release`]; if the walk panics it is dropped
    /// and the next walk starts from empty buffers.
    fn take(num_res: usize, num_samples: usize) -> Self {
        let mut this = ROW_INDEXER.with(|cell| std::mem::take(&mut *cell.borrow_mut()));
        let len = num_res * num_samples;
        this.row_runs.resize_with(len, Vec::new);
        this.cursor.resize(len, 0);
        this.segment_cells.resize(len, 0);
        this.num_samples = num_samples;
        this
    }

    /// Return the buffers to this thread for the next walk.
    fn release(self) {
        ROW_INDEXER.with(|cell| *cell.borrow_mut() = self);
    }

    /// Index every sample point of a row. False keeps the per-pixel path, for
    /// rows where some sample point has no `GridRow`.
    fn index_samples(
        &mut self,
        projector: &SampleProjector<'_>,
        ctx: &WalkContext<'_>,
        row_idx: usize,
        col_offset: usize,
        cols: &Range<usize>,
    ) -> bool {
        if cols.is_empty() {
            return true;
        }
        for runs in &mut self.row_runs {
            runs.clear();
        }
        let num_samples = self.num_samples;
        for (k, &sp) in ctx.sampling.points.iter().enumerate() {
            let Some(mut path) = projector.grid_row(row_idx, col_offset, k, sp, cols) else {
                return false;
            };
            let row_runs = &mut self.row_runs;
            self.indexer
                .index_row(&mut path, cols.clone(), ctx.resolutions, |i, span, cell| {
                    row_runs[i * num_samples + k].push((span.start, u64::from(cell)));
                });
        }
        true
    }

    /// Feed the row indexed by `index_samples` through `Runs`, with the same
    /// updates in the same order as the per-pixel weighted loop. Between two
    /// cell changes every valid pixel has the same assignments, so only `read`
    /// and `Runs::update` remain per pixel.
    #[allow(clippy::too_many_arguments)]
    fn replay_samples<T, Acc, E, R>(
        &mut self,
        cols: Range<usize>,
        row: &[T],
        spp: usize,
        read: &R,
        ctx: &WalkContext<'_>,
        (combine, total_weight): (bool, f64),
        runs: &mut Runs<Acc>,
        engine: &E,
        maps: &mut [CellMap<Acc>],
    ) where
        T: Copy,
        Acc: Clone,
        E: ScanlineEngine<T, Acc>,
        R: Fn(&[T]) -> Option<E::Sample>,
    {
        if cols.is_empty() {
            return;
        }
        let num_samples = self.num_samples;
        let single_weight = total_weight.is_finite() && (combine || num_samples == 1);
        self.cursor.fill(0);
        let mut start = cols.start;
        loop {
            let mut end = cols.end;
            for (j, runs_j) in self.row_runs.iter().enumerate() {
                let cursor = self.cursor[j];
                self.segment_cells[j] = runs_j[cursor].1;
                if let Some(&(next, _)) = runs_j.get(cursor + 1) {
                    end = end.min(next);
                }
            }
            for c in start..end {
                let Some(sample) = read(&row[c * spp..(c + 1) * spp]) else {
                    continue;
                };
                for (i, cells) in self.segment_cells.chunks_exact(num_samples).enumerate() {
                    let first = cells[0];
                    if single_weight && cells.iter().all(|&cell| cell == first) {
                        runs.update(i, first, sample, total_weight, engine, &mut maps[i]);
                        continue;
                    }
                    for (&cell, sp) in cells.iter().zip(&ctx.sampling.points) {
                        runs.update(i, cell, sample, sp.weight, engine, &mut maps[i]);
                    }
                }
            }
            if end >= cols.end {
                break;
            }
            for (j, runs_j) in self.row_runs.iter().enumerate() {
                if runs_j.get(self.cursor[j] + 1).map(|r| r.0) == Some(end) {
                    self.cursor[j] += 1;
                }
            }
            start = end;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk<T, Acc, E, R>(
        &mut self,
        path: &mut GridRow<'_>,
        cols: Range<usize>,
        row: &[T],
        spp: usize,
        read: &R,
        resolutions: &[Resolution],
        runs: &mut Runs<Acc>,
        engine: &E,
        maps: &mut [CellMap<Acc>],
    ) where
        T: Copy,
        Acc: Clone,
        E: ScanlineEngine<T, Acc>,
        R: Fn(&[T]) -> Option<E::Sample>,
    {
        if cols.is_empty() {
            return;
        }
        for runs in &mut self.row_runs {
            runs.clear();
        }
        let row_runs = &mut self.row_runs;
        self.indexer
            .index_row(path, cols.clone(), resolutions, |i, span, cell| {
                row_runs[i].push((span.start, u64::from(cell)));
            });
        self.cursor.fill(0);

        // Segments where no resolution changes cell. In each, the first valid
        // pixel is where the per-pixel loop would notice a change; the other
        // valid pixels would leave `Runs` untouched.
        let mut span_start: Option<usize> = None;
        let mut start = cols.start;
        loop {
            let mut end = cols.end;
            for (i, runs_i) in self.row_runs.iter().enumerate() {
                let k = self.cursor[i];
                runs.next_cells[i] = runs_i[k].1;
                if let Some(&(next, _)) = runs_i.get(k + 1) {
                    end = end.min(next);
                }
            }
            if let Some(c) = (start..end).find(|&c| read(&row[c * spp..(c + 1) * spp]).is_some()) {
                runs.apply_next_cells(&row[..c], &mut span_start, engine, maps);
            }
            if end >= cols.end {
                break;
            }
            for (i, runs_i) in self.row_runs.iter().enumerate() {
                if runs_i.get(self.cursor[i] + 1).map(|r| r.0) == Some(end) {
                    self.cursor[i] += 1;
                }
            }
            start = end;
        }
        if let Some(s) = span_start {
            runs.add_span(&row[s..cols.end], engine);
        }
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
            indexer: LatLngIndexer::default(),
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
        self.apply_next_cells(preceding, span_start, engine, maps);
    }

    /// Flush and restart the runs whose cell differs from `next_cells`.
    #[inline]
    fn apply_next_cells<T, E: ScanlineEngine<T, Acc>>(
        &mut self,
        preceding: &[T],
        span_start: &mut Option<usize>,
        engine: &E,
        maps: &mut [CellMap<Acc>],
    ) {
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

    /// A rotated `GeoTransform` with `size` units per pixel, turned by
    /// `angle` radians, and a little extra shear.
    fn rotated_gt(c0: f64, f0: f64, size: f64, angle: f64) -> GeoTransform {
        let (sin, cos) = angle.sin_cos();
        GeoTransform {
            c0,
            a: size * cos,
            b: size * (sin + 0.05),
            f0,
            d: size * sin,
            e: -size * cos,
        }
    }

    /// Rotated and sheared rows go through `index_row` (with per-span
    /// bounds), and every pixel gets the cell the per-pixel projection gives.
    #[test]
    fn rotated_rows_index_like_per_pixel_projection() {
        use crate::crs::transformer::AlbersConicFast;
        use h3o::CellIndex;

        if !exact_rows_enabled() {
            return;
        }
        let resolutions = [
            Resolution::Five,
            Resolution::Eight,
            Resolution::Ten,
            Resolution::Twelve,
        ];
        let width = 3000;
        let chunk = RasterChunk {
            col_offset: 1_000,
            row_offset: 0,
            width,
            height: 1,
        };
        let cases = [
            // (CRS, origin x, origin y, pixel size)
            (CrsTransformer::Wgs84Identity, -105.2, 40.1, 2.7e-4),
            (CrsTransformer::Wgs84Identity, 12.0, -71.5, 1e-3),
            (CrsTransformer::Wgs84Identity, 179.5, 3.0, 8e-5),
            (
                CrsTransformer::WebMercatorFast,
                -13_627_665.0,
                4_547_675.0,
                30.0,
            ),
            (
                CrsTransformer::WebMercatorFast,
                2_000_000.0,
                -9_000_000.0,
                75.0,
            ),
            (
                CrsTransformer::AlbersConic(AlbersConicFast::epsg_5070()),
                -1_500_000.0,
                2_100_000.0,
                30.0,
            ),
            (
                CrsTransformer::AlbersConic(AlbersConicFast::with_offsets(
                    -18.0, -36.0, -25.0, 132.0, 12345.0, -9876.0,
                )),
                -400_000.0,
                -1_200_000.0,
                90.0,
            ),
        ];
        let (mut rows, mut decisions) = (0, 0);
        for (n, (crs, x0, y0, size)) in cases.iter().enumerate() {
            for angle in [0.01_f64, -0.3, 0.7, 1.2] {
                let gt = rotated_gt(*x0, *y0, *size, angle);
                for sampling in [SamplingPattern::center(), SamplingPattern::rgss()] {
                    let ctx = WalkContext {
                        chunk: &chunk,
                        resolutions: &resolutions,
                        crs,
                        gt: &gt,
                        sampling: &sampling,
                        bbox: None,
                        stride: width,
                        samples_per_pixel: 1,
                        owner: None,
                        band_scaling: &[],
                        alpha_sample: None,
                    };
                    let mut projector = SampleProjector::new(&ctx);
                    let col_offset = chunk.col_offset as usize;
                    let cols = 0..width as usize;
                    for row_idx in [0, 1_777] {
                        projector.start_row(&ctx, row_idx);
                        for (k, &sp) in sampling.points.iter().enumerate() {
                            let mut path = projector
                                .grid_row(row_idx, col_offset, k, sp, &cols)
                                .unwrap_or_else(|| panic!("case {n} angle {angle}: no row path"));
                            let mut cells = vec![vec![None::<CellIndex>; cols.len()]; 4];
                            let stats = LatLngIndexer::new().index_row(
                                &mut path,
                                cols.clone(),
                                &resolutions,
                                |i, span, cell| cells[i][span].fill(Some(cell)),
                            );
                            rows += 1;
                            decisions += stats.decisions;
                            for c in cols.clone() {
                                let (lon, lat) =
                                    projector.project(col_offset + c, row_idx, k, sp).unwrap();
                                assert!((-90.0..=90.0).contains(&lat));
                                let ll = LatLng::new(lat, lon).unwrap();
                                assert_eq!(path.point(c), ll, "case {n} col {c}");
                                for (i, &res) in resolutions.iter().enumerate() {
                                    assert_eq!(
                                        cells[i][c],
                                        Some(ll.to_cell(res)),
                                        "case {n} angle {angle} row {row_idx} col {c} {res:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(rows, cases.len() * 4 * 2 * (1 + 4));
        // Most pixels are decided by interpolation, not `to_cell`.
        assert!(decisions > rows * 4 * 100, "only {decisions} decisions");
    }

    /// Rotated rows that reach a pole (WGS84) or leave Web Mercator's range
    /// keep the per-pixel path.
    #[test]
    fn rotated_rows_near_poles_keep_per_pixel_path() {
        if !exact_rows_enabled() {
            return;
        }
        let chunk = RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 100,
            height: 1,
        };
        let sampling = SamplingPattern::center();
        for (crs, gt) in [
            // Latitude passes 90 (WGS84) or 89 (Web Mercator, y = 4e7 is 89.8°).
            (
                CrsTransformer::Wgs84Identity,
                rotated_gt(10.0, 89.9, 0.01, 1.0),
            ),
            (
                CrsTransformer::WebMercatorFast,
                rotated_gt(0.0, 4.0e7, 1_000.0, 0.5),
            ),
        ] {
            let ctx = WalkContext {
                chunk: &chunk,
                resolutions: &[Resolution::Nine],
                crs: &crs,
                gt: &gt,
                sampling: &sampling,
                bbox: None,
                stride: 100,
                samples_per_pixel: 1,
                owner: None,
                band_scaling: &[],
                alpha_sample: None,
            };
            let mut projector = SampleProjector::new(&ctx);
            projector.start_row(&ctx, 0);
            let sp = sampling.points[0];
            assert!(projector.grid_row(0, 0, 0, sp, &(0..100)).is_none());
        }
    }
}

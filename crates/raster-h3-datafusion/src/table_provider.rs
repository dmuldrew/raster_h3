//! DataFusion `TableProvider`, `ExecutionPlan`, and `TableFunctionImpl` implementations
//! for continuous and categorical GeoTIFF/COG to H3 aggregation.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use raster_h3_core::aggregator::multi_horizon::{
    MultiResolutionConfig, QuantileTarget, SpectralFormula,
};
use raster_h3_core::aggregator::remap::CategoryRemapper;
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::columnar::{
    ArrowArrayStream, CategoricalBatchReader, CategoricalFormat, ColumnSchema,
    ContinuousBatchReader, RecordBatch,
};
use raster_h3_core::error::Result;
use raster_h3_core::raster::mosaic::{resolve_raster_sources, OverlapRule};
use raster_h3_core::scalar_ops::estimate_raster_cardinality;

/// Cardinality statistics exposed to query planners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableStatistics {
    pub num_rows: u64,
    pub is_exact: bool,
}

/// Filter predicates pushable into the raster H3 scanline engine.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterPredicate {
    MinCount(f64),
    MinMean(f64),
    MaxMean(f64),
    MinMajorityFraction(f64),
    Bbox([f64; 4]),
    Resolution(u8),
    Other(String),
}

/// Filter pushdown support classification (`Exact`, `Inexact`, `Unsupported`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPushdownStatus {
    Exact,
    Inexact,
    Unsupported,
}

/// Physical execution plan producing columnar [`RecordBatch`] streams or [`ArrowArrayStream`].
pub trait ExecutionPlan: Send + Sync {
    /// Output schema of this physical plan (after projection pushdown).
    fn schema(&self) -> Arc<ColumnSchema>;

    /// Execute the plan and return a streaming iterator of [`RecordBatch`]es.
    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>>;

    /// Execute the plan and export an ABI-stable [`ArrowArrayStream`].
    fn execute_arrow_c_stream(&self) -> Result<ArrowArrayStream> {
        let schema = self.schema();
        let iter = self.execute()?;
        Ok(ArrowArrayStream::from_iterator(schema, iter))
    }
}

/// DataFusion-compatible `TableProvider` trait for scanning GeoTIFF/COG rasters as H3 tables.
pub trait TableProvider: Send + Sync {
    /// Table schema prior to projection.
    fn schema(&self) -> Arc<ColumnSchema>;

    /// Estimated table cardinality for query optimization.
    fn statistics(&self) -> TableStatistics;

    /// Classify which filter predicates can be pushed down into the scanline aggregator.
    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus>;

    /// Create a physical [`ExecutionPlan`] with projection, filter, and limit pushdown.
    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>>;
}

/// Apply a filter common to both providers (`MinCount`, `Bbox`, `Resolution`) by narrowing
/// `config`. Returns `false` when the filter excludes every row. Filters only ever narrow the
/// table: a `Resolution` outside the configured set or a `Bbox` disjoint from the configured
/// one yields an empty scan rather than widening it.
///
/// Core's bbox clips individual samples, so a cell straddling its edge would be aggregated
/// from only part of its pixels. A pushed `Bbox` is therefore widened by [`cell_margin_km`]
/// at the coarsest resolution first: every cell that touches the requested box then lies
/// wholly inside the widened one and keeps its full aggregate. The partial cells near the
/// widened edge do not touch the requested box, so the caller's re-applied predicate drops
/// them (the pushdown is `Inexact`). With compaction on, a cell's output depends on all its
/// children, so the box is not pushed at all.
fn apply_shared_filter(config: &mut MultiResolutionConfig, filter: &FilterPredicate) -> bool {
    match *filter {
        FilterPredicate::MinCount(v) => {
            config.min_count = Some(config.min_count.map_or(v, |c| c.max(v)));
            true
        }
        FilterPredicate::Resolution(r) => {
            if config.resolutions.contains(&r) {
                config.resolutions = vec![r];
                true
            } else {
                false
            }
        }
        FilterPredicate::Bbox(bbox) => {
            if config.compact || config.compact_h3_children {
                return true;
            }
            let Some(widened) = config
                .resolutions
                .iter()
                .min()
                .and_then(|&r| cell_margin_km(r))
                .and_then(|margin| expand_bbox(bbox, margin))
            else {
                return true;
            };
            match config.bbox {
                None => {
                    config.bbox = Some(widened);
                    true
                }
                Some(current) => match intersect_bbox(current, widened) {
                    Some(narrowed) => {
                        config.bbox = Some(narrowed);
                        true
                    }
                    None => false,
                },
            }
        }
        _ => true,
    }
}

/// Kilometres per degree of latitude, rounded down so degree margins come out larger.
const KM_PER_DEG_LAT: f64 = 110.0;
/// Kilometres per degree of longitude at the equator, rounded down likewise.
const KM_PER_DEG_LON_EQUATOR: f64 = 111.0;
/// Beyond this latitude the widened box spans every longitude.
const POLAR_LAT: f64 = 89.0;

/// A distance larger than the diameter of any H3 cell at `resolution`, including the
/// samples a cell owns along its geodesic edges.
///
/// The largest cells have edges about 1.4x the average and a diameter about 2x their edge,
/// so roughly 2.8x the average edge; 4x leaves headroom.
fn cell_margin_km(resolution: u8) -> Option<f64> {
    h3o::Resolution::try_from(resolution)
        .ok()
        .map(|r| 4.0 * r.edge_length_km())
}

/// Widen a `[min_lon, min_lat, max_lon, max_lat]` box by `margin_km` on every side. The
/// result crosses the antimeridian (`min_lon > max_lon`) when the margin wraps, and spans
/// every longitude near a pole or once wider than the globe. `None` for coordinates out of
/// range, in which case the box is not pushed.
fn expand_bbox(bbox: [f64; 4], margin_km: f64) -> Option<[f64; 4]> {
    let [min_lon, min_lat, max_lon, max_lat] = bbox;
    let in_range =
        |lon: f64, lat: f64| (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat);
    if !(in_range(min_lon, min_lat) && in_range(max_lon, max_lat)) || min_lat > max_lat {
        return None;
    }
    let lat_margin = margin_km / KM_PER_DEG_LAT;
    let lo_lat = (min_lat - lat_margin).max(-90.0);
    let hi_lat = (max_lat + lat_margin).min(90.0);
    let widest = lo_lat.abs().max(hi_lat.abs());
    if widest >= POLAR_LAT {
        return Some([-180.0, lo_lat, 180.0, hi_lat]);
    }
    let lon_margin = margin_km / (KM_PER_DEG_LON_EQUATOR * widest.to_radians().cos());
    let span = if min_lon > max_lon {
        max_lon + 360.0 - min_lon
    } else {
        max_lon - min_lon
    };
    if span + 2.0 * lon_margin >= 360.0 {
        return Some([-180.0, lo_lat, 180.0, hi_lat]);
    }
    let mut lo_lon = min_lon - lon_margin;
    let mut hi_lon = max_lon + lon_margin;
    if lo_lon < -180.0 {
        lo_lon += 360.0;
    }
    if hi_lon > 180.0 {
        hi_lon -= 360.0;
    }
    Some([lo_lon, lo_lat, hi_lon, hi_lat])
}

/// Intersect two `[min_lon, min_lat, max_lon, max_lat]` boxes; `None` when disjoint.
/// Antimeridian-crossing boxes (`min_lon > max_lon`) keep `current`: `Bbox` pushdown is
/// `Inexact`, so the caller still re-applies the predicate.
fn intersect_bbox(current: [f64; 4], other: [f64; 4]) -> Option<[f64; 4]> {
    if current[0] > current[2] || other[0] > other[2] {
        return Some(current);
    }
    let out = [
        current[0].max(other[0]),
        current[1].max(other[1]),
        current[2].min(other[2]),
        current[3].min(other[3]),
    ];
    (out[0] <= out[2] && out[1] <= out[3]).then_some(out)
}

/// The row limit a scan may hand its batch reader. A pushed `Bbox` is `Inexact`: the scan
/// also emits cells near the widened edge that the caller's re-applied predicate drops, so
/// a reader-level limit could be spent on them before every matching cell is produced.
fn reader_limit(filters: &[FilterPredicate], limit: Option<usize>) -> Option<usize> {
    if filters
        .iter()
        .any(|f| matches!(f, FilterPredicate::Bbox(_)))
    {
        None
    } else {
        limit
    }
}

#[cfg(test)]
mod bbox_tests {
    use super::*;
    use h3o::CellIndex;

    #[test]
    fn expand_wraps_and_saturates() {
        let m = 110.0; // one degree of latitude
        assert_eq!(
            expand_bbox([0.0, 0.0, 1.0, 1.0], 0.0),
            Some([0.0, 0.0, 1.0, 1.0])
        );
        let [lo, s, hi, n] = expand_bbox([10.0, 10.0, 11.0, 11.0], m).unwrap();
        assert_eq!((s, n), (9.0, 12.0));
        assert!(lo < 9.0 && hi > 12.0);
        // Widening across the antimeridian yields min_lon > max_lon.
        let [lo, _, hi, _] = expand_bbox([179.5, 0.0, 179.9, 1.0], m).unwrap();
        assert!(lo > hi && lo < 179.5 && hi > -180.0);
        // A box already crossing it stays crossing, and grows on both sides.
        let [lo, _, hi, _] = expand_bbox([178.0, 0.0, -178.0, 1.0], m).unwrap();
        assert!(lo < 178.0 && hi > -178.0 && lo > hi);
        // Near a pole, or wider than the globe, every longitude is kept.
        assert_eq!(expand_bbox([0.0, 88.5, 1.0, 88.6], m).unwrap()[0], -180.0);
        assert_eq!(expand_bbox([-179.5, 0.0, 179.5, 1.0], m).unwrap()[2], 180.0);
        assert_eq!(expand_bbox([190.0, 0.0, 191.0, 1.0], m), None);
    }

    #[test]
    fn margin_exceeds_every_cell_diameter() {
        for res in 0..=3u8 {
            let margin = cell_margin_km(res).unwrap();
            let r = h3o::Resolution::try_from(res).unwrap();
            let widest = CellIndex::base_cells()
                .flat_map(|b| b.children(r))
                .map(|cell| {
                    let b: Vec<_> = cell.boundary().iter().copied().collect();
                    b.iter()
                        .flat_map(|p| b.iter().map(move |q| p.distance_km(*q)))
                        .fold(0.0, f64::max)
                })
                .fold(0.0, f64::max);
            assert!(
                widest * 1.25 < margin,
                "res {res}: widest {widest} km, margin {margin} km"
            );
        }
    }

    #[test]
    fn compaction_skips_bbox_pushdown() {
        let mut config = MultiResolutionConfig::new(vec![8]);
        config.compact = true;
        assert!(apply_shared_filter(
            &mut config,
            &FilterPredicate::Bbox([0.0, 0.0, 1.0, 1.0])
        ));
        assert_eq!(config.bbox, None);
    }
}

/// Physical execution plan for continuous H3 raster aggregation.
pub struct ContinuousRasterExec {
    paths: Vec<PathBuf>,
    config: MultiResolutionConfig,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    limit: Option<usize>,
    /// Pushed-down filters exclude every row (e.g. an unconfigured resolution).
    empty: bool,
}

impl ExecutionPlan for ContinuousRasterExec {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>> {
        if self.empty {
            return Ok(Box::new(std::iter::empty()));
        }
        let reader = ContinuousBatchReader::open(
            &self.paths,
            &self.config,
            self.projection.clone(),
            self.batch_size,
            self.limit,
        )?;
        Ok(Box::new(reader))
    }
}

/// `TableProvider` for `h3_raster_continuous_aggregate`.
#[derive(Clone)]
pub struct ContinuousRasterTableProvider {
    pub paths: Vec<PathBuf>,
    pub config: MultiResolutionConfig,
    pub batch_size: usize,
}

impl ContinuousRasterTableProvider {
    pub fn try_new(source: &str, resolutions: Vec<u8>) -> Result<Self> {
        let paths = resolve_raster_sources(source)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        Ok(Self {
            paths,
            config: MultiResolutionConfig::new(res),
            batch_size: 2048,
        })
    }

    pub fn with_crs(mut self, crs: impl Into<String>) -> Self {
        self.config.custom_crs = Some(crs.into());
        self
    }

    pub fn with_nodata(mut self, nodata: f64) -> Self {
        self.config.custom_nodata = Some(nodata);
        self
    }

    /// Calibrate stored values with GDAL band scale/offset (default `true`).
    pub fn with_scale_offset(mut self, apply: bool) -> Self {
        self.config.apply_scale_offset = apply;
        self
    }

    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.config.bbox = Some(bbox);
        self
    }

    pub fn with_sampling(mut self, sampling: SamplingPattern) -> Self {
        self.config.sampling = sampling;
        self
    }

    pub fn with_band(mut self, band: usize) -> Self {
        self.config.band = band.max(1);
        self
    }

    pub fn with_spectral_formula(mut self, formula: SpectralFormula) -> Self {
        self.config.spectral_formula = Some(formula);
        self
    }

    pub fn with_quantiles(mut self, quantiles: Vec<QuantileTarget>) -> Self {
        self.config.quantiles = quantiles;
        self
    }

    pub fn with_min_count(mut self, min_count: f64) -> Self {
        self.config.min_count = Some(min_count);
        self
    }

    pub fn with_min_mean(mut self, min_mean: f64) -> Self {
        self.config.min_mean = Some(min_mean);
        self
    }

    pub fn with_max_mean(mut self, max_mean: f64) -> Self {
        self.config.max_mean = Some(max_mean);
        self
    }

    pub fn with_compact(mut self, compact: bool) -> Self {
        self.config.compact = compact;
        self.config.compact_h3_children = compact;
        self
    }

    pub fn with_overlap_rule(mut self, rule: OverlapRule) -> Self {
        self.config.overlap_rule = rule;
        self
    }

    /// Fail the scan when a valid sample point cannot be transformed to WGS84,
    /// instead of skipping it (default `false`).
    pub fn with_strict_transforms(mut self, strict: bool) -> Self {
        self.config.strict_transforms = strict;
        self
    }

    /// Add the number of sample points dropped by failed coordinate transforms
    /// to `counter` as plans built from this provider execute.
    pub fn with_transform_failure_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.config.transform_failure_counter = Some(counter);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }
}

impl TableProvider for ContinuousRasterTableProvider {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::new(ColumnSchema::continuous(&self.config.quantiles))
    }

    fn statistics(&self) -> TableStatistics {
        let est = estimate_raster_cardinality(&self.paths, &self.config.resolutions);
        TableStatistics {
            num_rows: est,
            is_exact: false,
        }
    }

    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus> {
        filters
            .iter()
            .map(|f| match f {
                FilterPredicate::MinCount(_)
                | FilterPredicate::MinMean(_)
                | FilterPredicate::MaxMean(_)
                | FilterPredicate::Resolution(_) => FilterPushdownStatus::Exact,
                FilterPredicate::Bbox(_) => FilterPushdownStatus::Inexact,
                _ => FilterPushdownStatus::Unsupported,
            })
            .collect()
    }

    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>> {
        let mut config = self.config.clone();
        let mut empty = false;
        for f in filters {
            match *f {
                FilterPredicate::MinMean(v) => {
                    config.min_mean = Some(config.min_mean.map_or(v, |c| c.max(v)));
                }
                FilterPredicate::MaxMean(v) => {
                    config.max_mean = Some(config.max_mean.map_or(v, |c| c.min(v)));
                }
                _ => empty |= !apply_shared_filter(&mut config, f),
            }
        }

        let base_schema = self.schema();
        let schema = match projection {
            Some(indices) => Arc::new(base_schema.project(indices)?),
            None => base_schema,
        };

        Ok(Box::new(ContinuousRasterExec {
            paths: self.paths.clone(),
            config,
            projection: projection.map(|p| p.to_vec()),
            schema,
            batch_size: self.batch_size,
            limit: reader_limit(filters, limit),
            empty,
        }))
    }
}

/// Physical execution plan for categorical H3 raster aggregation.
pub struct CategoricalRasterExec {
    paths: Vec<PathBuf>,
    config: MultiResolutionConfig,
    format: CategoricalFormat,
    projection: Option<Vec<usize>>,
    schema: Arc<ColumnSchema>,
    batch_size: usize,
    limit: Option<usize>,
    /// Pushed-down filters exclude every row (e.g. an unconfigured resolution).
    empty: bool,
}

impl ExecutionPlan for CategoricalRasterExec {
    fn schema(&self) -> Arc<ColumnSchema> {
        Arc::clone(&self.schema)
    }

    fn execute(&self) -> Result<Box<dyn Iterator<Item = Result<RecordBatch>> + Send>> {
        if self.empty {
            return Ok(Box::new(std::iter::empty()));
        }
        let reader = CategoricalBatchReader::open(
            &self.paths,
            &self.config,
            self.format,
            self.projection.clone(),
            self.batch_size,
            self.limit,
        )?;
        Ok(Box::new(reader))
    }
}

/// `TableProvider` for `h3_raster_categorical_aggregate`.
#[derive(Clone)]
pub struct CategoricalRasterTableProvider {
    pub paths: Vec<PathBuf>,
    pub config: MultiResolutionConfig,
    pub format: CategoricalFormat,
    pub batch_size: usize,
}

impl CategoricalRasterTableProvider {
    pub fn try_new(source: &str, resolutions: Vec<u8>) -> Result<Self> {
        let paths = resolve_raster_sources(source)?;
        let res = if resolutions.is_empty() {
            vec![8]
        } else {
            resolutions
        };
        Ok(Self {
            paths,
            config: MultiResolutionConfig::new(res),
            format: CategoricalFormat::Wide,
            batch_size: 2048,
        })
    }

    pub fn with_format(mut self, format: CategoricalFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_crs(mut self, crs: impl Into<String>) -> Self {
        self.config.custom_crs = Some(crs.into());
        self
    }

    pub fn with_nodata(mut self, nodata: f64) -> Self {
        self.config.custom_nodata = Some(nodata);
        self
    }

    pub fn with_bbox(mut self, bbox: [f64; 4]) -> Self {
        self.config.bbox = Some(bbox);
        self
    }

    pub fn with_remapper(mut self, remapper: Arc<CategoryRemapper>) -> Self {
        self.config.remapper = Some(remapper);
        self
    }

    pub fn with_min_count(mut self, min_count: f64) -> Self {
        self.config.min_count = Some(min_count);
        self
    }

    pub fn with_min_majority_fraction(mut self, frac: f64) -> Self {
        self.config.min_majority_fraction = Some(frac);
        self
    }

    /// Fail the scan when a valid sample point cannot be transformed to WGS84,
    /// instead of skipping it (default `false`).
    pub fn with_strict_transforms(mut self, strict: bool) -> Self {
        self.config.strict_transforms = strict;
        self
    }

    /// Add the number of sample points dropped by failed coordinate transforms
    /// to `counter` as plans built from this provider execute.
    pub fn with_transform_failure_counter(mut self, counter: Arc<AtomicU64>) -> Self {
        self.config.transform_failure_counter = Some(counter);
        self
    }

    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }
}

impl TableProvider for CategoricalRasterTableProvider {
    fn schema(&self) -> Arc<ColumnSchema> {
        match self.format {
            CategoricalFormat::Wide => Arc::new(ColumnSchema::categorical_wide()),
            CategoricalFormat::Long => Arc::new(ColumnSchema::categorical_long()),
        }
    }

    fn statistics(&self) -> TableStatistics {
        let est = estimate_raster_cardinality(&self.paths, &self.config.resolutions);
        TableStatistics {
            num_rows: est,
            is_exact: false,
        }
    }

    fn supports_filters_pushdown(&self, filters: &[FilterPredicate]) -> Vec<FilterPushdownStatus> {
        filters
            .iter()
            .map(|f| match f {
                FilterPredicate::MinCount(_)
                | FilterPredicate::MinMajorityFraction(_)
                | FilterPredicate::Resolution(_) => FilterPushdownStatus::Exact,
                FilterPredicate::Bbox(_) => FilterPushdownStatus::Inexact,
                _ => FilterPushdownStatus::Unsupported,
            })
            .collect()
    }

    fn scan(
        &self,
        projection: Option<&[usize]>,
        filters: &[FilterPredicate],
        limit: Option<usize>,
    ) -> Result<Box<dyn ExecutionPlan>> {
        let mut config = self.config.clone();
        let mut empty = false;
        for f in filters {
            match *f {
                FilterPredicate::MinMajorityFraction(v) => {
                    config.min_majority_fraction =
                        Some(config.min_majority_fraction.map_or(v, |c| c.max(v)));
                }
                _ => empty |= !apply_shared_filter(&mut config, f),
            }
        }

        let base_schema = self.schema();
        let schema = match projection {
            Some(indices) => Arc::new(base_schema.project(indices)?),
            None => base_schema,
        };

        Ok(Box::new(CategoricalRasterExec {
            paths: self.paths.clone(),
            config,
            format: self.format,
            projection: projection.map(|p| p.to_vec()),
            schema,
            batch_size: self.batch_size,
            limit: reader_limit(filters, limit),
            empty,
        }))
    }
}

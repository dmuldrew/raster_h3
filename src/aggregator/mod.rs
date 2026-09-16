//! Core H3 hexagonal aggregation engines for raster data.
//!
//! This module implements the core H3 hexagonal aggregation engines. It uses a
//! "southernmost scan-line horizon eviction" architecture that maintains only
//! the active scan front in memory (~15 MB), enabling constant-memory streaming
//! of multi-gigabyte rasters.
//!
//! # Architecture & Key Submodules
//!
//! - [`accumulator`](crate::aggregator::accumulator): Welford online statistics accumulation (mean, variance, min, max, count, sum).
//! - [`categorical`](crate::aggregator::categorical): Class frequency tracking and mode computation for discrete rasters.
//! - [`h3_scanline`](crate::aggregator::h3_scanline): Scanline lookahead buffering and H3 neighbor caching.
//! - [`horizon_streamer`](crate::aggregator::horizon_streamer): Southernmost scan-line horizon eviction engine for single-resolution streaming.
//! - [`multi_horizon`](crate::aggregator::multi_horizon): Single-pass multi-resolution fusion and concurrent horizon streaming.
//! - [`nodata`](crate::aggregator::nodata): NoData detection, value casting, and dispatching.
//! - [`quantiles`](crate::aggregator::quantiles): Streaming percentile sketches for approximate quantiles.
//! - [`remap`](crate::aggregator::remap): Category remapping and value reclassification rules.
//! - [`sampling`](crate::aggregator::sampling): Sub-pixel super-sampling patterns (center, 4-point, jittered).
//! - [`simd`](crate::aggregator::simd): Vectorized span accumulation using SIMD operations.

/// Online statistical accumulators using Welford's algorithm.
pub mod accumulator;
/// Frequency tracking and summary metrics for categorical raster data.
pub mod categorical;
/// Scanline lookahead buffering and H3 spatial neighbor traversal caching.
pub mod h3_scanline;
/// Southernmost scan-line horizon eviction engine for single-resolution streaming.
pub mod horizon_streamer;
/// Single-pass multi-resolution fusion and horizon streaming engines.
pub mod multi_horizon;
/// NoData value detection, type casting, and chunk skipping utilities.
pub mod nodata;
/// Streaming quantile and percentile sketches.
pub mod quantiles;
/// Category remapping rules and value reclassification.
pub mod remap;
/// Sub-pixel super-sampling point patterns and offsets.
pub mod sampling;
/// Vectorized contiguous span accumulation using SIMD.
pub mod simd;

/// Online continuous accumulator maintaining Welford summary statistics for an H3 cell.
pub use accumulator::H3Accumulator;
/// Categorical accumulator tracking class frequencies and uniformity metrics for an H3 cell.
pub use categorical::{CategoricalAccumulator, CategoricalUniformity};
/// Scanline lookahead buffer and spatial neighbor caching for H3 indexing.
pub use h3_scanline::H3ScanlineLookahead;
/// Horizon eviction tracking structures and spatial bounding box helpers.
pub use horizon_streamer::{chunk_intersects_bbox, compute_cell_south_lat, HexEvictionEntry};
/// Multi-resolution horizon streamers, output records, and configuration.
pub use multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
/// NoData detection, chunk skipping checks, and sentinel value casting.
pub use nodata::{is_chunk_all_nodata, is_decoding_result_all_nodata, NodataCast};
/// Memory-efficient streaming sketch for calculating approximate percentiles.
pub use quantiles::QuantileSketch;
/// Category remapping engine, rules, and unmapped value handling policies.
pub use remap::{CategoryRemapper, RemapRule, UnmappedAction};
/// Sub-pixel sample points and sampling pattern configurations.
pub use sampling::{SamplePoint, SamplingPattern};
/// SIMD-accelerated horizontal span accumulator.
pub use simd::SimdSpanAccumulate;

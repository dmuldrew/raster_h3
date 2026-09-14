//! Multi-Resolution Direct Ground-Truth Horizon Streaming
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! while preserving 100% true pixel-in-polygon containment at each resolution.
//!
//! Employs multi-core chunk-row parallelism via Rayon to process chunks across all CPU cores
//! lock-free while strictly bounding RAM to the active scanline horizon.

pub mod categorical;
pub mod categorical_streamer;
pub mod config;
pub mod continuous;
pub mod continuous_streamer;
pub mod controller;
pub mod sharded_map;
pub mod walker;

pub use categorical::{process_categorical_chunk_payload_into, MultiCategoricalRecord};
pub use categorical_streamer::MultiCategoricalHorizonStreamer;
pub use config::{MultiResolutionConfig, QuantileTarget, SpectralFormula};
pub use continuous::{process_continuous_chunk_payload_into, MultiContinuousRecord};
pub use continuous_streamer::MultiScanHorizonStreamer;
pub use controller::{HorizonStreamKernel, MultiHorizonStreamer};
pub use sharded_map::{get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS};
pub use walker::{
    is_slice_all_native_nodata, scanline_walk, walk_overlap_pixel_cells, RowCoordinates,
    RowGeometryContext, ScanlineEngine, RAD_TO_DEG, WGS84_A,
};

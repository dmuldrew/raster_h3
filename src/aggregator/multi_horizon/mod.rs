//! Multi-resolution raster aggregation with certified center assignments,
//! conservative horizon eviction, and budgeted external aggregation.
//! The aggregation budget does not limit TIFF decoder or whole-process memory.

pub mod borrowed;
pub mod categorical;
pub mod categorical_streamer;
pub mod compaction;
pub mod config;
pub mod continuous;
pub mod continuous_streamer;
pub mod controller;
pub mod coordinates;
pub mod lifecycle;
pub mod profile;
pub mod sharded_map;
pub mod spectral;
pub mod spill;
pub mod walker;

pub use categorical::{process_categorical_chunk_payload_into, MultiCategoricalRecord};
pub use categorical_streamer::MultiCategoricalHorizonStreamer;
pub use compaction::HierarchicalCompactor;
pub use config::{MultiResolutionConfig, QuantileTarget, SpectralFormula};
pub use continuous::{
    process_continuous_chunk_payload_into, ContinuousOptions, MultiContinuousRecord,
};
pub use continuous_streamer::MultiScanHorizonStreamer;
pub use controller::{HorizonStreamKernel, MultiHorizonStreamer, RecordStreamer};
pub use coordinates::{is_point_in_bbox, CoordinateTransformer, RAD_TO_DEG, WGS84_A};
pub use lifecycle::{OutputBuffer, StreamLifecycle, StreamState};
pub use sharded_map::{get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS};
pub use walker::{
    is_slice_all_native_nodata, scanline_walk, walk_direct, walk_interleaved, ScanlineEngine,
    WalkContext,
};

//! Multi-resolution raster aggregation with certified center assignments,
//! conservative horizon eviction, and budgeted external aggregation.
//! The aggregation budget does not limit TIFF decoder or whole-process memory.

pub mod categorical;
pub mod categorical_streamer;
pub mod compaction;
pub mod config;
pub mod continuous;
pub mod continuous_streamer;
pub mod controller;
pub mod coordinates;
pub mod lifecycle;
pub mod overlap_walker;
pub mod sharded_map;
pub mod span;
pub mod spectral;
pub mod spill;
pub mod walker;

pub use categorical::{process_categorical_chunk_payload_into, MultiCategoricalRecord};
pub use categorical_streamer::MultiCategoricalHorizonStreamer;
pub use compaction::HierarchicalCompactor;
pub use config::{MultiResolutionConfig, QuantileTarget, SpectralFormula};
pub use continuous::{process_continuous_chunk_payload_into, MultiContinuousRecord};
pub use continuous_streamer::MultiScanHorizonStreamer;
pub use controller::{HorizonStreamKernel, MultiHorizonStreamer, RecordStreamer};
pub use coordinates::{
    is_point_in_bbox, resolve_subpixel_cell, CoordinateTransformer, RowCoordinates,
    RowGeometryContext, RAD_TO_DEG, WGS84_A,
};
pub use lifecycle::{OutputBuffer, StreamLifecycle, StreamState};
pub use sharded_map::{get_shard, AccumulatorMerge, ShardedResolutionMap, NUM_SHARDS};
pub use span::H3SpanOptimizer;
pub use walker::{
    is_slice_all_native_nodata, scanline_walk, walk_overlap_pixel_cells, ScanlineEngine,
};

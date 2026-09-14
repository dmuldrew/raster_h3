pub mod accumulator;
pub mod categorical;
pub mod h3_scanline;
pub mod horizon_streamer;
pub mod multi_horizon;
pub mod quantiles;
pub mod remap;
pub mod sampling;
pub mod simd;

pub use accumulator::H3Accumulator;
pub use categorical::{CategoricalAccumulator, CategoricalUniformity};
pub use h3_scanline::{can_use_neighbor_cache, H3NeighborDiskCache, H3ScanlineLookahead};
pub use horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, is_chunk_all_nodata, HexEvictionEntry,
};
pub use multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
pub use quantiles::QuantileSketch;
pub use remap::{CategoryRemapper, RemapRule, UnmappedAction};
pub use sampling::{SamplePoint, SamplingPattern};
pub use simd::SimdSpanAccumulate;

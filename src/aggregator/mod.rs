pub mod accumulator;
pub mod categorical;
pub mod h3_map;
pub mod h3_scanline;
pub mod horizon_streamer;
pub mod multi_horizon;
pub mod quantiles;
pub mod remap;
pub mod sampling;
pub mod simd;

pub use accumulator::H3Accumulator;
pub use quantiles::QuantileSketch;
pub use remap::{CategoryRemapper, RemapRule, UnmappedAction};
pub use categorical::{CategoricalAccumulator, CategoricalHorizonStreamer};
pub use h3_map::{aggregate_raster_stream, H3HashMap};
pub use h3_scanline::H3ScanlineLookahead;
pub use horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, is_chunk_all_nodata, AggregationConfig,
    HexEvictionEntry, ScanHorizonStreamer,
};
pub use multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget,
};
pub use sampling::{SamplePoint, SamplingPattern};
pub use simd::SimdSpanAccumulate;

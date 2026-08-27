pub mod accumulator;
pub mod categorical;
pub mod coherence;
pub mod h3_map;
pub mod horizon_streamer;
pub mod sampling;

pub use accumulator::H3Accumulator;
pub use categorical::{CategoricalAccumulator, CategoricalHorizonStreamer};
pub use coherence::SpatialCoherenceCache;
pub use h3_map::{aggregate_raster_stream, H3HashMap};
pub use horizon_streamer::{
    chunk_intersects_bbox, compute_cell_south_lat, is_chunk_all_nodata, AggregationConfig,
    HexEvictionEntry, ScanHorizonStreamer,
};
pub use sampling::{SamplePoint, SamplingPattern};

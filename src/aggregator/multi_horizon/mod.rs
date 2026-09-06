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

pub use categorical::{process_categorical_chunk_payload_into, MultiCategoricalRecord};
pub use categorical_streamer::MultiCategoricalHorizonStreamer;
pub use config::{MultiResolutionConfig, QuantileTarget, SpectralFormula};
pub use continuous::{
    is_slice_all_native_nodata, process_continuous_chunk_payload_into, MultiContinuousRecord,
    RAD_TO_DEG, WGS84_A,
};
pub use continuous_streamer::MultiScanHorizonStreamer;

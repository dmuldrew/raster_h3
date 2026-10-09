//! Multi-Resolution Scanline Horizon Streamer for Continuous Data
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! for continuous/floating-point raster bands and spectral indices.

use fxhash::FxBuildHasher;
use h3o::Resolution;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use tiff::decoder::DecodingResult;

use crate::aggregator::accumulator::H3Accumulator;
use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;

use super::config::{MultiResolutionConfig, SpectralFormula};
use super::continuous::{
    process_continuous_chunk_payload_into, ContinuousOptions, MultiContinuousRecord,
};
use super::controller::{HorizonStreamKernel, MultiHorizonStreamer, RecordStreamer};
use super::walker::WalkContext;

/// Kernel implementation for continuous (floating point / spectral) raster aggregation
#[derive(Clone)]
pub struct ContinuousKernel {
    pub band: usize,
    pub spectral_formula: Option<SpectralFormula>,
    pub min_count: Option<f64>,
    pub min_mean: Option<f64>,
    pub max_mean: Option<f64>,
    pub track_quantiles: bool,
}

impl ContinuousKernel {
    fn options(&self) -> ContinuousOptions {
        ContinuousOptions {
            band: self.band,
            spectral_formula: self.spectral_formula,
            track_quantiles: self.track_quantiles,
        }
    }
}

impl HorizonStreamKernel for ContinuousKernel {
    type Accumulator = H3Accumulator;
    type Record = MultiContinuousRecord;

    #[inline(always)]
    fn new_parent_accumulator(&self) -> Self::Accumulator {
        if self.track_quantiles {
            H3Accumulator::with_quantiles()
        } else {
            H3Accumulator::default()
        }
    }

    #[inline(always)]
    fn passes_filter(&self, acc: &Self::Accumulator) -> bool {
        if let Some(min_c) = self.min_count {
            if acc.count < min_c {
                return false;
            }
        }
        if let Some(min_m) = self.min_mean {
            if acc.mean() < min_m {
                return false;
            }
        }
        if let Some(max_m) = self.max_mean {
            if acc.mean() > max_m {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    fn make_record(&self, resolution: u8, cell_u64: u64, acc: Self::Accumulator) -> Self::Record {
        MultiContinuousRecord {
            resolution,
            h3_index: cell_u64,
            accumulator: acc,
        }
    }

    #[inline(always)]
    fn process_chunk(
        &self,
        window: &WalkContext,
        decoding_result: &mut DecodingResult,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool {
        process_continuous_chunk_payload_into(
            window,
            decoding_result,
            nodata,
            self.options(),
            local_maps,
        )
    }

    #[inline(always)]
    fn process_window(
        &self,
        window: &WalkContext,
        samples: super::borrowed::BorrowedSamples<'_>,
        nodata: Option<f64>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool {
        super::continuous::process_continuous_borrowed_into(
            window,
            samples,
            nodata,
            self.options(),
            local_maps,
        )
    }
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Continuous Data)
pub struct MultiScanHorizonStreamer {
    inner: MultiHorizonStreamer<ContinuousKernel>,
    track_quantiles: bool,
}

impl MultiScanHorizonStreamer {
    /// Whether records carry streaming quantile sketches
    pub fn track_quantiles(&self) -> bool {
        self.track_quantiles
    }

    /// Initialize a new MultiScanHorizonStreamer from a single GeoTIFF reader
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        let kernel = ContinuousKernel {
            band: config.band,
            spectral_formula: config.spectral_formula,
            min_count: config.min_count,
            min_mean: config.min_mean,
            max_mean: config.max_mean,
            track_quantiles: config.track_quantiles(),
        };
        let track_quantiles = kernel.track_quantiles;
        let inner = MultiHorizonStreamer::new(reader, config, kernel)?;
        Ok(Self {
            inner,
            track_quantiles,
        })
    }

    /// Initialize a new MultiScanHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(mosaic: Arc<MosaicReader>, config: &MultiResolutionConfig) -> Result<Self> {
        let kernel = ContinuousKernel {
            band: config.band,
            spectral_formula: config.spectral_formula,
            min_count: config.min_count,
            min_mean: config.min_mean,
            max_mean: config.max_mean,
            track_quantiles: config.track_quantiles(),
        };
        let track_quantiles = kernel.track_quantiles;
        let inner = MultiHorizonStreamer::new_mosaic(mosaic, config, kernel)?;
        Ok(Self {
            inner,
            track_quantiles,
        })
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    #[inline(always)]
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Result<Vec<MultiContinuousRecord>> {
        self.inner.fetch_next_batch(max_rows)
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    #[inline(always)]
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, MultiContinuousRecord),
    {
        self.inner.drain_completed_into(max_rows, consumer)
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    #[inline(always)]
    pub fn advance_until_completed(&mut self, min_rows: usize) -> Result<()> {
        self.inner.advance_until_completed(min_rows)
    }

    /// Return current southernmost latitude reached by scanline horizon
    #[inline(always)]
    pub fn current_lat_horizon(&self) -> f64 {
        self.inner.current_lat_horizon()
    }

    /// Target H3 resolutions
    #[inline(always)]
    pub fn resolutions(&self) -> &[Resolution] {
        self.inner.resolutions()
    }

    /// Target H3 resolution integer levels
    #[inline(always)]
    pub fn resolution_u8s(&self) -> &[u8] {
        self.inner.resolution_u8s()
    }

    /// Number of partial aggregation runs persisted under memory pressure.
    pub fn spill_run_count(&self) -> u64 {
        self.inner.spill_run_count()
    }

    /// Peak active-map allocation estimate (not whole-process memory).
    pub fn peak_active_bytes(&self) -> usize {
        self.inner.peak_active_bytes()
    }

    /// Return total active in-flight cells across all resolutions
    #[inline(always)]
    pub fn active_cell_count(&self) -> usize {
        self.inner.active_cell_count()
    }

    /// Check if stream is fully drained and finished
    #[inline(always)]
    pub fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }
}

impl Deref for MultiScanHorizonStreamer {
    type Target = MultiHorizonStreamer<ContinuousKernel>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MultiScanHorizonStreamer {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl RecordStreamer for MultiScanHorizonStreamer {
    type Record = MultiContinuousRecord;

    #[inline(always)]
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> Result<usize>
    where
        F: FnMut(usize, Self::Record),
    {
        self.inner.drain_completed_into(max_rows, consumer)
    }

    #[inline(always)]
    fn current_lat_horizon(&self) -> f64 {
        self.inner.current_lat_horizon()
    }

    #[inline(always)]
    fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    #[inline(always)]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        Some(self.inner.mosaic().mosaic_bounds_wgs84)
    }

    #[inline(always)]
    fn resolution_u8s(&self) -> &[u8] {
        self.inner.resolution_u8s()
    }
}

//! Multi-Resolution Scanline Horizon Streamer for Categorical Data
//!
//! Provides single-pass streaming aggregation across multiple H3 resolution levels simultaneously
//! for discrete integer / land cover categories and classification maps.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use fxhash::FxBuildHasher;
use h3o::Resolution;
use tiff::decoder::DecodingResult;

use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::remap::CategoryRemapper;
use crate::aggregator::sampling::SamplingPattern;
use crate::crs::transformer::CrsTransformer;
use crate::error::Result;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::geotransform::GeoTransform;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;

use super::categorical::{process_categorical_chunk_payload_into, MultiCategoricalRecord};
use super::config::MultiResolutionConfig;
use super::controller::{HorizonStreamKernel, MultiHorizonStreamer};

pub use super::sharded_map::{get_shard as pub_get_shard, NUM_SHARDS as PUB_NUM_SHARDS};

/// Kernel implementation for categorical/classified raster aggregation
#[derive(Clone)]
pub struct CategoricalKernel {
    pub band: usize,
    pub min_count: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub remapper: Option<Arc<CategoryRemapper>>,
}

impl HorizonStreamKernel for CategoricalKernel {
    type Accumulator = CategoricalAccumulator;
    type Record = MultiCategoricalRecord;

    #[inline(always)]
    fn new_parent_accumulator(&self) -> Self::Accumulator {
        CategoricalAccumulator::default()
    }

    #[inline(always)]
    fn passes_filter(&self, acc: &Self::Accumulator) -> bool {
        if let Some(min_c) = self.min_count {
            if acc.total_count < min_c {
                return false;
            }
        }
        if let Some(min_frac) = self.min_majority_fraction {
            let (_, _, frac) = acc.majority();
            if frac < min_frac {
                return false;
            }
        }
        true
    }

    #[inline(always)]
    fn make_record(&self, resolution: u8, cell_u64: u64, acc: Self::Accumulator) -> Self::Record {
        MultiCategoricalRecord {
            resolution,
            h3_index: cell_u64,
            accumulator: acc,
        }
    }

    #[inline(always)]
    fn process_chunk(
        &self,
        chunk_bounds: &RasterChunk,
        decoding_result: &mut DecodingResult,
        resolutions: &[Resolution],
        crs_transformer: &CrsTransformer,
        gt: &GeoTransform,
        sampling: &SamplingPattern,
        bbox: Option<[f64; 4]>,
        chunk_stride: u32,
        nodata: Option<f64>,
        samples_per_pixel: u16,
        overlap_ctx: Option<(usize, &MosaicReader)>,
        local_maps: &mut [HashMap<u64, Self::Accumulator, FxBuildHasher>],
    ) -> bool {
        process_categorical_chunk_payload_into(
            chunk_bounds,
            decoding_result,
            resolutions,
            crs_transformer,
            gt,
            sampling,
            bbox,
            chunk_stride,
            nodata,
            samples_per_pixel,
            self.band,
            overlap_ctx,
            self.remapper.as_deref(),
            local_maps,
        )
    }
}

/// Single-pass streaming aggregator across multiple H3 resolutions (Categorical Data)
pub struct MultiCategoricalHorizonStreamer {
    inner: MultiHorizonStreamer<CategoricalKernel>,
    pub remapper: Option<Arc<CategoryRemapper>>,
}

impl MultiCategoricalHorizonStreamer {
    /// Initialize a new MultiCategoricalHorizonStreamer from a single GeoTIFF reader
    pub fn new(reader: GeoTiffStreamReader, config: &MultiResolutionConfig) -> Result<Self> {
        let kernel = CategoricalKernel {
            band: config.band,
            min_count: config.min_count,
            min_majority_fraction: config.min_majority_fraction,
            remapper: config.remapper.clone(),
        };
        let remapper = kernel.remapper.clone();
        let inner = MultiHorizonStreamer::new(reader, config, kernel)?;
        Ok(Self { inner, remapper })
    }

    /// Initialize a new MultiCategoricalHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(mosaic: Arc<MosaicReader>, config: &MultiResolutionConfig) -> Result<Self> {
        let kernel = CategoricalKernel {
            band: config.band,
            min_count: config.min_count,
            min_majority_fraction: config.min_majority_fraction,
            remapper: config.remapper.clone(),
        };
        let remapper = kernel.remapper.clone();
        let inner = MultiHorizonStreamer::new_mosaic(mosaic, config, kernel)?;
        Ok(Self { inner, remapper })
    }

    /// Pull up to `max_rows` completed multi-resolution records using multi-core chunk-row parallelism
    #[inline(always)]
    pub fn fetch_next_batch(&mut self, max_rows: usize) -> Vec<MultiCategoricalRecord> {
        self.inner.fetch_next_batch(max_rows)
    }

    /// Drain up to `max_rows` completed records directly into a closure with zero heap allocation
    #[inline(always)]
    pub fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> usize
    where
        F: FnMut(usize, MultiCategoricalRecord),
    {
        self.inner.drain_completed_into(max_rows, consumer)
    }

    /// Advance scanline horizon until at least `min_rows` completed records are available or finished
    #[inline(always)]
    pub fn advance_until_completed(&mut self, min_rows: usize) {
        self.inner.advance_until_completed(min_rows);
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

impl Deref for MultiCategoricalHorizonStreamer {
    type Target = MultiHorizonStreamer<CategoricalKernel>;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for MultiCategoricalHorizonStreamer {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

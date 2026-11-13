//! Construction of a [`MultiHorizonStreamer`] over a mosaic.

use std::collections::HashMap;
use std::sync::Arc;

use h3o::Resolution;

use super::super::compaction::HierarchicalCompactor;
use super::super::config::MultiResolutionConfig;
use super::super::coordinates::eviction_north_bound;
use super::super::eviction_space::EvictionSpace;
use super::super::lifecycle::{OutputBuffer, StreamLifecycle};
use super::super::partition::{restrict_mosaic, BoundaryClassifier};
use super::super::sharded_map::ShardedResolutionMap;
use super::super::spill::SpillRuns;
use super::pipeline::{shared_aggregation_pool, worker_map_fixed_bytes};
use super::{HorizonStreamKernel, MultiHorizonStreamer, PartitionOutput};
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::error::{RasterH3Error, Result};
use crate::grid::GridKind;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;
use crate::raster::prefetch::PrefetchedMosaicReader;

/// Refuse a `band` (or spectral formula band) that some mosaic tile does not have. The
/// sample kernels index pixels by band without a bounds check that fails the scan, so an
/// out-of-range band would otherwise read another band's values.
fn check_bands_exist(mosaic: &MosaicReader, config: &MultiResolutionConfig) -> Result<()> {
    let bands = match &config.spectral_formula {
        Some(formula) => formula.bands(),
        None => vec![config.band],
    };
    for tile in &mosaic.tiles {
        let available = usize::from(tile.reader.metadata.samples_per_pixel.max(1));
        if let Some(&band) = bands.iter().find(|&&b| b == 0 || b > available) {
            let what = if config.spectral_formula.is_some() {
                "spectral formula band"
            } else {
                "band"
            };
            return Err(RasterH3Error::InvalidParameter(format!(
                "{what} {band} does not exist: the raster has {available} band(s), numbered from 1"
            )));
        }
    }
    Ok(())
}

impl<K: HorizonStreamKernel> MultiHorizonStreamer<K> {
    /// Initialize a new MultiHorizonStreamer from a single GeoTIFF reader
    pub fn new(
        reader: GeoTiffStreamReader,
        config: &MultiResolutionConfig,
        kernel: K,
    ) -> Result<Self> {
        let mosaic = Arc::new(MosaicReader::from_single_reader(
            reader,
            config.bbox,
            config.custom_crs.as_deref(),
        )?);
        Self::new_mosaic(mosaic, config, kernel)
    }

    /// Initialize a new MultiHorizonStreamer from a multi-file MosaicReader
    pub fn new_mosaic(
        mosaic: Arc<MosaicReader>,
        config: &MultiResolutionConfig,
        kernel: K,
    ) -> Result<Self> {
        config.validate()?;
        check_bands_exist(&mosaic, config)?;
        let should_compact = config.should_compact_h3_children();
        // A partition streams only its own chunks; cells that chunks outside it may also
        // reach are classified against the full mosaic's chunk list.
        let (mosaic, partition) = match &config.partition {
            Some(spec) => {
                let restricted = Arc::new(restrict_mosaic(&mosaic, spec)?);
                let output = PartitionOutput {
                    classifier: BoundaryClassifier::with_grid(&mosaic, spec, config.grid),
                    partials: OutputBuffer::with_capacity(16),
                };
                (restricted, Some(output))
            }
            None => (mosaic, None),
        };

        let mut resolutions = Vec::with_capacity(if config.grid == GridKind::H3 {
            config.resolutions.len()
        } else {
            0
        });
        let mut resolution_u8s = Vec::with_capacity(config.resolutions.len());
        for &res_u8 in &config.resolutions {
            if config.grid == GridKind::H3 {
                let res = Resolution::try_from(res_u8).map_err(|_| {
                    RasterH3Error::InvalidParameter(format!("Invalid H3 resolution: {}", res_u8))
                })?;
                resolutions.push(res);
            }
            resolution_u8s.push(res_u8);
        }

        let prefetcher = PrefetchedMosaicReader::spawn_with_budget(
            Arc::clone(&mosaic),
            config.prefetch_chunks,
            config.decode_workers,
            config.fetch_workers,
            config.decode_budget_bytes,
        );
        let num_res = resolution_u8s.len();
        let mut resolution_shards: Vec<_> =
            (0..num_res).map(|_| ShardedResolutionMap::new()).collect();
        // Past an opt-in limit, pending sibling groups are decomposed before completion.
        let compactor = HierarchicalCompactor::with_grid_and_byte_limit(
            config.grid,
            should_compact,
            config.compaction_budget_bytes.unwrap_or(usize::MAX),
        );

        // Bounds cover whole chunks and all subpixel offsets. Unknown projected
        // chunks get +infinity; once they are processed a later suffix can still
        // become certifiable.
        let n_chunks = mosaic.chunk_refs.len();
        let mut suffix_max_north_lat = vec![f64::NEG_INFINITY; n_chunks + 1];
        for k in (0..n_chunks).rev() {
            let reference = mosaic.chunk_refs[k];
            let tile = &mosaic.tiles[reference.tile_idx];
            let chunk = tile.reader.chunk_layout.get_chunk_bounds(
                reference.chunk_idx,
                tile.reader.metadata.width,
                tile.reader.metadata.height,
            );
            let north = eviction_north_bound(
                &chunk,
                &tile.reader.metadata.geotransform,
                &tile.crs_transformer,
            );
            suffix_max_north_lat[k] = suffix_max_north_lat[k + 1].max(north);
        }
        // Cells are evicted by `eviction_space`; latitude bounds still guard
        // compaction and the horizon published to tilers.
        let eviction_space = EvictionSpace::for_mosaic_with_grid(&mosaic, config.grid);
        let suffix_eviction_bound = eviction_space.is_rows().then(|| {
            let mut suffix = vec![f64::NEG_INFINITY; n_chunks + 1];
            for k in (0..n_chunks).rev() {
                let reference = mosaic.chunk_refs[k];
                let tile = &mosaic.tiles[reference.tile_idx];
                let chunk = tile.reader.chunk_layout.get_chunk_bounds(
                    reference.chunk_idx,
                    tile.reader.metadata.width,
                    tile.reader.metadata.height,
                );
                let bound = eviction_space.chunk_bound(
                    &chunk,
                    &tile.reader.metadata.geotransform,
                    &tile.crs_transformer,
                );
                suffix[k] = suffix[k + 1].max(bound);
            }
            suffix
        });
        let can_evict_early = suffix_eviction_bound
            .as_ref()
            .unwrap_or(&suffix_max_north_lat)
            .iter()
            .any(|v| v.is_finite());
        for map in &mut resolution_shards {
            map.set_eviction_enabled(can_evict_early);
            map.set_lat_tracking(eviction_space.is_rows());
        }
        // Leave room for sorted-run construction, merge heads, and bounded output.
        let map_budget = config.aggregation_budget_bytes / num_res / 4;
        let accumulator_limit = config.aggregation_budget_bytes / num_res / 8;
        let spill_runs = (0..num_res)
            .map(|_| SpillRuns::new(accumulator_limit, config.spill_directory.as_deref()))
            .collect();
        // Reserve worker storage by sample cardinality, not TIFF chunk count:
        // each sample can add at most one cell per resolution.
        let per_sample = kernel.worker_bytes_per_sample();
        let per_pixel = per_sample
            .saturating_mul(num_res)
            .saturating_mul(config.sampling.points.len());
        let worker_budget = config.aggregation_budget_bytes / 4;
        if per_pixel > worker_budget {
            return Err(RasterH3Error::InvalidParameter(
                "sampling pattern exceeds worker allowance; increase aggregation_budget_bytes"
                    .into(),
            ));
        }
        let pool = config
            .aggregation_workers
            .map(shared_aggregation_pool)
            .transpose()?;
        let worker_tasks = match config.aggregation_workers {
            Some(n) => n.clamp(1, 64),
            None => rayon::current_num_threads().clamp(1, 8),
        }
        .min(worker_budget / per_pixel);
        let batch_size = (worker_tasks * 2).min((n_chunks / 4).max(1));
        // Two map sets per worker keep workers busy while earlier windows
        // wait to be merged in order; all sets share the worker allowance.
        let map_sets = (2 * worker_tasks).min(worker_budget / per_pixel).max(1);
        let worker_tasks = worker_tasks.min(map_sets);
        let map_fixed = worker_map_fixed_bytes::<K::Accumulator>().saturating_mul(num_res);
        let worker_pixels =
            ((worker_budget / map_sets).saturating_sub(map_fixed) / per_pixel).clamp(1, 16384);
        // Horizon maps hold every cell of a batch until it is merged, so a
        // batch also stops at about one window per map set (see `next_batch_len`).
        let batch_pixels = map_sets * worker_pixels;
        let output_byte_limit = config.aggregation_budget_bytes / 4;

        Ok(Self {
            kernel,
            prefetcher: Some(prefetcher),
            mosaic,
            grid: config.grid,
            resolutions,
            resolution_u8s,
            nodata: config.custom_nodata,
            apply_scale_offset: config.apply_scale_offset,
            transform_failure_counter: config.transform_failure_counter.clone(),
            strict_transforms: config.strict_transforms,
            bbox: config.bbox,
            sampling: config.sampling.clone(),
            resolution_shards,
            compactor,
            output_buffer: OutputBuffer::with_capacity(16),
            lifecycle: StreamLifecycle::new(),
            current_lat_horizon: f64::INFINITY,
            can_evict_early,
            suffix_max_north_lat,
            eviction_space,
            suffix_eviction_bound,
            lat_horizon: f64::INFINITY,
            processed_chunk_count: 0,
            spill_runs,
            final_reader: None,
            eviction_horizon: f64::INFINITY,
            final_resolution: 0,
            finishing: false,
            compaction_parent: None,
            map_budget,
            accumulator_limit,
            worker_pixels,
            worker_tasks,
            pool,
            worker_budget,
            batch_size,
            batch_pixels,
            output_byte_limit,
            peak_active_bytes: 0,
            finished_sets: Vec::new(),
            new_cell_buffer: Vec::new(),
            key_chunk_buffer: Vec::new(),
            worker_maps: (0..map_sets)
                .map(|_| {
                    (0..num_res)
                        .map(|_| HashMap::with_hasher(H3BuildHasher))
                        .collect()
                })
                .collect(),
            metrics: super::super::profile::StreamProfile {
                worker_map_sets: map_sets as u64,
                ..Default::default()
            },
            partition,
        })
    }
}

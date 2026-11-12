use super::super::eviction_space::CellKeys;
use super::super::sharded_map::ShardedResolutionMap;
use super::super::spill::SpillRuns;
use super::pipeline::{run_pipelined, CellMaps, MergeTarget, WindowJobs};
use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::cell_hash::H3BuildHasher;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::mosaic::MosaicReader;
use crate::raster::RasterChunk;
use h3o::Resolution;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tiff::decoder::DecodingResult;

fn test_mosaic() -> (tempfile::NamedTempFile, MosaicReader) {
    use tiff::encoder::{colortype::Gray8, TiffEncoder};
    use tiff::tags::Tag;

    let file = tempfile::NamedTempFile::new().unwrap();
    {
        let mut encoder = TiffEncoder::new(file.reopen().unwrap()).unwrap();
        let mut image = encoder.new_image::<Gray8>(1, 1).unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.01, 0.01, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0, 0.0, 0.0, -100.0, 40.0, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
        image.write_data(&[1]).unwrap();
    }
    let reader = GeoTiffStreamReader::open(file.path()).unwrap();
    (
        file,
        MosaicReader::from_single_reader(reader, None, None).unwrap(),
    )
}

#[test]
fn large_tile_windows_are_lazy_and_cover_each_pixel_once() {
    let (_file, mosaic) = test_mosaic();
    let items = vec![Ok((
        0,
        0,
        RasterChunk {
            col_offset: 7,
            row_offset: 11,
            width: 4096,
            height: 4096,
        },
        DecodingResult::U8(vec![1; 4096 * 4096]),
        true,
    ))];
    let mut decoded_bytes = 0;
    let mut jobs = WindowJobs::new(&items, &mosaic, 33, &mut decoded_bytes).unwrap();
    let count = 4096usize.div_ceil(33) * 4096;
    assert_eq!(jobs.len(), count);
    assert_eq!(decoded_bytes, 4096 * 4096);
    let mut pixels = 0;
    let mut windows = 0;
    while let Some((tile, chunk, samples, stride, overlap)) = jobs.next() {
        assert_eq!(tile, 0);
        assert_eq!(chunk.col_offset as usize - 7, pixels % 4096);
        assert_eq!(chunk.row_offset as usize - 11, pixels / 4096);
        assert_eq!(chunk.height, 1);
        assert!(chunk.width <= 33);
        assert_eq!(samples.bytes(), chunk.width as usize);
        assert_eq!(stride, 4096);
        assert!(overlap);
        pixels += chunk.width as usize;
        windows += 1;
        assert_eq!(jobs.len(), count - windows);
    }
    assert_eq!(windows, count);
    assert_eq!(pixels, 4096 * 4096);
    assert!(jobs.next().is_none());
}

#[test]
fn windows_combine_rows_and_preserve_interleaved_sample_offsets() {
    let (_file, mut mosaic) = test_mosaic();
    mosaic.tiles[0].reader.metadata.samples_per_pixel = 3;
    let items = vec![
        Ok((
            0,
            0,
            RasterChunk {
                col_offset: 0,
                row_offset: 0,
                width: 0,
                height: 3,
            },
            DecodingResult::U8(Vec::new()),
            false,
        )),
        Ok((
            0,
            0,
            RasterChunk {
                col_offset: 4,
                row_offset: 6,
                width: 3,
                height: 5,
            },
            DecodingResult::U8((0..45).collect()),
            false,
        )),
        Ok((
            0,
            0,
            RasterChunk {
                col_offset: 8,
                row_offset: 9,
                width: 3,
                height: 1,
            },
            DecodingResult::U8((45..54).collect()),
            false,
        )),
    ];
    let mut decoded_bytes = 0;
    let jobs = WindowJobs::new(&items, &mosaic, 7, &mut decoded_bytes).unwrap();
    assert_eq!(jobs.len(), 4);
    let windows: Vec<_> = jobs
        .map(|(_, chunk, samples, stride, _)| {
            let super::super::borrowed::BorrowedSamples::U8(samples) = samples else {
                panic!("wrong sample type")
            };
            assert_eq!(stride, 3);
            (chunk.row_offset, chunk.height, samples.to_vec())
        })
        .collect();
    assert_eq!(
        windows,
        vec![
            (6, 2, (0..18).collect()),
            (8, 2, (18..36).collect()),
            (10, 1, (36..45).collect()),
            (9, 1, (45..54).collect()),
        ]
    );
    assert_eq!(decoded_bytes, 54);
}

#[test]
fn malformed_chunks_fail_before_dispatch() {
    let (_file, mosaic) = test_mosaic();
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 2,
        height: 2,
    };
    let mut items = vec![
        Ok((0, 0, chunk, DecodingResult::U8(vec![0; 4]), false)),
        Ok((0, 0, chunk, DecodingResult::U8(vec![0; 3]), false)),
    ];
    let mut decoded_bytes = 0;
    assert!(matches!(
        WindowJobs::new(&items, &mosaic, 1, &mut decoded_bytes),
        Err(RasterH3Error::InvalidMetadata(_))
    ));
    items[1] = Err(RasterH3Error::StreamFailed("decode failed".into()));
    assert!(matches!(
        WindowJobs::new(&items, &mosaic, 1, &mut decoded_bytes),
        Err(RasterH3Error::StreamFailed(_))
    ));
}

#[test]
fn scheduling_storage_is_bounded_for_half_a_million_windows() {
    struct Job<'a>(&'a AtomicUsize);
    impl Drop for Job<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let live = AtomicUsize::new(0);
    let peak_live = AtomicUsize::new(0);
    let jobs = (0..512_000).map(|_| {
        peak_live.fetch_max(live.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        Job(&live)
    });
    let run_window = |_: Job<'_>, _: &mut CellMaps<H3Accumulator>| {
        (super::super::profile::WorkerProfile::default(), 0)
    };
    let compute_keys = |_: &[(usize, u64)]| Vec::new();
    let mut shards = vec![ShardedResolutionMap::new()];
    let mut spill_runs = vec![SpillRuns::new(usize::MAX, None)];
    let mut metrics = super::super::profile::StreamProfile::default();
    let mut peak = 0;
    let mut free = (0..8)
        .map(|_| vec![HashMap::with_hasher(H3BuildHasher)])
        .collect();
    let mut finished = Vec::new();
    let mut target = MergeTarget {
        shards: &mut shards,
        spill_runs: &mut spill_runs,
        metrics: &mut metrics,
        peak_active_bytes: &mut peak,
        failure_counter: None,
        compactor_bytes: 0,
        accumulator_limit: usize::MAX,
        map_budget: usize::MAX,
        set_budget: usize::MAX,
        map_sets: 8,
        new_cells: Vec::new(),
        key_chunks: Vec::new(),
    };
    pool.in_place_scope(|scope| {
        run_pipelined(
            scope,
            4,
            jobs,
            &mut free,
            &run_window,
            &compute_keys,
            &mut target,
            &mut finished,
        )
    })
    .unwrap();
    assert_eq!(metrics.jobs, 512_000);
    assert_eq!(live.load(Ordering::Relaxed), 0);
    assert!(peak_live.load(Ordering::Relaxed) <= 8);
    assert_eq!(finished.len(), 8);
    assert_eq!(finished.capacity(), 8);
    assert!(finished.iter().all(Option::is_none));
    assert_eq!(free.len(), 8);
}

fn run(jobs: usize, panic_at: Option<usize>) -> std::thread::Result<Result<Vec<usize>>> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let mut shards: Vec<ShardedResolutionMap<H3Accumulator>> = vec![ShardedResolutionMap::new()];
    let mut spill_runs = vec![SpillRuns::new(usize::MAX, None)];
    let mut metrics = super::super::profile::StreamProfile::default();
    let mut peak = 0;
    let mut free: Vec<CellMaps<H3Accumulator>> = (0..8)
        .map(|_| vec![HashMap::with_hasher(H3BuildHasher)])
        .collect();
    let order = Mutex::new(Vec::new());
    // Window i adds one cell whose index records i, with a delay that
    // makes later windows finish first.
    let run_window = |index: usize, maps: &mut CellMaps<H3Accumulator>| {
        if Some(index) == panic_at {
            panic!("window {index} failed");
        }
        std::thread::sleep(std::time::Duration::from_micros(
            ((jobs - index) % 5) as u64 * 200,
        ));
        let cell = u64::from(
            h3o::LatLng::new(10.0 + index as f64 * 0.01, 20.0)
                .unwrap()
                .to_cell(Resolution::Nine),
        );
        maps[0].insert(cell, H3Accumulator::new(index as f64));
        (super::super::profile::WorkerProfile::default(), 0)
    };
    let compute_keys = |cells: &[(usize, u64)]| -> Vec<CellKeys> {
        cells
            .iter()
            .map(|&(_, c)| {
                order.lock().unwrap().push(c);
                CellKeys {
                    key: 0.0,
                    south_lat_upper: 0.0,
                }
            })
            .collect()
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut target = MergeTarget {
            shards: &mut shards,
            spill_runs: &mut spill_runs,
            metrics: &mut metrics,
            peak_active_bytes: &mut peak,
            failure_counter: None,
            compactor_bytes: 0,
            accumulator_limit: usize::MAX,
            map_budget: usize::MAX,
            set_budget: usize::MAX,
            map_sets: 8,
            new_cells: Vec::new(),
            key_chunks: Vec::new(),
        };
        let result = pool.in_place_scope(|scope| {
            run_pipelined(
                scope,
                4,
                0..jobs,
                &mut free,
                &run_window,
                &compute_keys,
                &mut target,
                &mut Vec::new(),
            )
        });
        result.map(|()| {
            let mut chunks = std::mem::take(&mut target.key_chunks);
            chunks.sort_unstable_by_key(|c| c.0);
            // Merge order: the window each new cell came from.
            chunks
                .into_iter()
                .flat_map(|(_, cells, _)| cells)
                .map(|(_, cell)| target.shards[0].get(cell).map(|a| a.sum as usize).unwrap())
                .collect()
        })
    }))
}

#[test]
fn merges_in_window_order_when_later_windows_finish_first() {
    let order = run(600, None).unwrap().unwrap();
    assert_eq!(order, (0..600).collect::<Vec<_>>());
}

#[test]
fn a_worker_panic_reaches_the_caller() {
    let outcome = run(200, Some(37));
    assert!(outcome.is_err(), "the panic was swallowed");
}

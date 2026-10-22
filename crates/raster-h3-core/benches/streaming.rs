//! End-to-end streaming baselines; fixture generation is outside the measured loop.

#[path = "../tests/it/helpers.rs"]
mod helpers;

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use raster_h3_core::aggregator::categorical::CategoricalAccumulator;
use raster_h3_core::aggregator::cell_hash::H3BuildHasher;
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3_core::aggregator::quantiles::QuantileSketch;
use raster_h3_core::aggregator::sampling::SamplingPattern;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

fn streaming(c: &mut Criterion) {
    let (_file, path) = helpers::TestGeoTiffBuilder::new(256, 256)
        .create_f32_tempfile(|col, row| ((col + row) % 8) as f32);
    let mut group = c.benchmark_group("streaming");
    group.throughput(Throughput::Elements(256 * 256));
    for (name, config) in [
        ("continuous", MultiResolutionConfig::single(8)),
        (
            "multiresolution",
            MultiResolutionConfig::new(vec![6, 7, 8, 9]),
        ),
        ("supersampling", {
            let mut config = MultiResolutionConfig::single(8);
            config.sampling = SamplingPattern::rgss();
            config
        }),
    ] {
        group.bench_with_input(BenchmarkId::new(name, "256x256"), &config, |b, config| {
            b.iter(|| {
                let reader = GeoTiffStreamReader::open(&path).unwrap();
                let mut stream = MultiScanHorizonStreamer::new(reader, config).unwrap();
                loop {
                    let batch = stream.fetch_next_batch(32).unwrap();
                    if batch.is_empty() {
                        break;
                    }
                    black_box(batch);
                }
            });
        });
    }
    group.bench_function("categorical/256x256", |b| {
        b.iter(|| {
            let reader = GeoTiffStreamReader::open(&path).unwrap();
            let mut stream =
                MultiCategoricalHorizonStreamer::new(reader, &MultiResolutionConfig::single(8))
                    .unwrap();
            loop {
                let batch = stream.fetch_next_batch(32).unwrap();
                if batch.is_empty() {
                    break;
                }
                black_box(batch);
            }
        });
    });
    group.finish();
}

fn hash_sensitive(c: &mut Criterion) {
    let categories: Vec<_> = (0..65536).map(|i| i64::from(i % 64)).collect();
    let values: Vec<_> = (0..65536).map(|i| f64::from(i % 4096 + 1)).collect();
    let mut group = c.benchmark_group("hash_sensitive");
    group.throughput(Throughput::Elements(65536));
    for classes in [8, 64] {
        let inputs: Vec<_> = categories
            .iter()
            .map(|category| category % classes)
            .collect();
        group.bench_with_input(
            BenchmarkId::new("categorical", classes),
            &classes,
            |b, _| {
                b.iter(|| {
                    let mut accumulator = CategoricalAccumulator::new();
                    for &category in &inputs {
                        accumulator.update(black_box(category));
                    }
                    black_box(accumulator);
                });
            },
        );
    }
    group.bench_function("quantile_sketch", |b| {
        b.iter(|| {
            let mut sketch = QuantileSketch::new();
            for &value in &values {
                sketch.insert(black_box(value));
            }
            black_box(sketch);
        });
    });
    group.finish();
}

fn h3_map(c: &mut Criterion) {
    let cells: Vec<_> = h3o::CellIndex::try_from(0x8a2a1072b59ffffu64)
        .unwrap()
        .parent(h3o::Resolution::Three)
        .unwrap()
        .children(h3o::Resolution::Ten)
        .map(u64::from)
        .collect();
    let mut group = c.benchmark_group("h3_map");
    group.throughput(Throughput::Elements((cells.len() * 2) as u64));
    group.bench_function("res10_insert_remove", |b| {
        b.iter(|| {
            let mut map: HashMap<u64, u64, H3BuildHasher> = HashMap::default();
            for &cell in &cells {
                map.insert(cell, cell);
            }
            for cell in &cells {
                black_box(map.remove(cell));
            }
            black_box(map);
        });
    });
    group.finish();
}

criterion_group!(benches, streaming, hash_sensitive, h3_map, pruning);
criterion_main!(benches);

/// Local uncompressed synthetic tiles. Includes open/planning/decode/aggregation;
/// fixtures are created outside timing. One worker makes comparisons repeatable.
fn pruning(c: &mut Criterion) {
    use raster_h3_core::crs::transformer::CrsTransformer;
    use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};
    use std::sync::Arc;
    let mut group = c.benchmark_group("pruning");
    for (epsg, x, y) in [
        (3413, 1_000_000.0, -2_000_000.0),
        (3031, 1_000_000.0, 2_000_000.0),
        (2154, 700_000.0, 6_600_000.0),
        (3035, 4_000_000.0, 3_000_000.0),
    ] {
        let fixtures: Vec<_> = (0..16)
            .map(|i| {
                helpers::TestGeoTiffBuilder::new(128, 128)
                    .epsg(epsg)
                    .origin(
                        x + (i % 4) as f64 * 250_000.0,
                        y + (i / 4) as f64 * 250_000.0,
                    )
                    .pixel_size(100.0)
                    .create_constant_tempfile(1.0f32)
            })
            .collect();
        let paths: Vec<_> = fixtures.iter().map(|(_, p)| p.clone()).collect();
        let tf = CrsTransformer::from_epsg_code(epsg as u32).unwrap();
        let (lon, lat) = tf.transform_point(x + 6400.0, y - 6400.0).unwrap();
        for (name, rule, bbox) in [
            (
                "small_bbox",
                OverlapRule::First,
                Some([lon - 0.01, lat - 0.01, lon + 0.01, lat + 0.01]),
            ),
            ("disjoint_first", OverlapRule::First, None),
            ("disjoint_average", OverlapRule::Average, None),
        ] {
            let mut config = MultiResolutionConfig::single(7);
            config.aggregation_workers = Some(1);
            config.decode_workers = 1;
            config.bbox = bbox;
            let plan = MosaicReader::open(&paths, bbox, None, rule).unwrap();
            eprintln!(
                "pruning/{name}/{epsg}: tiles={} chunks={} overlaps={}",
                plan.tiles.len(),
                plan.chunk_refs.len(),
                plan.chunk_refs.iter().filter(|c| c.has_overlap).count()
            );
            group.bench_function(BenchmarkId::new(name, epsg), |b| {
                b.iter(|| {
                    let mosaic = Arc::new(MosaicReader::open(&paths, bbox, None, rule).unwrap());
                    let mut stream = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();
                    loop {
                        let batch = stream.fetch_next_batch(4096).unwrap();
                        if batch.is_empty() {
                            break;
                        }
                        black_box(batch);
                    }
                })
            });
        }
    }
    group.finish();
}

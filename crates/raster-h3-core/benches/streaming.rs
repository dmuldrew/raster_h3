//! End-to-end streaming baselines; fixture generation is outside the measured loop.

#[path = "../tests/it/helpers.rs"]
mod helpers;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use raster_h3_core::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
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

criterion_group!(benches, streaming);
criterion_main!(benches);

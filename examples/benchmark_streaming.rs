//! Reproducible streaming benchmark. See docs/streaming-performance.md.
use raster_h3::{
    aggregator::{MultiResolutionConfig, MultiScanHorizonStreamer, SamplingPattern},
    raster::geotiff::GeoTiffStreamReader,
};
use std::{collections::BTreeMap, fs::File, time::Instant};
use tiff::{
    encoder::{colortype::Gray32Float, TiffEncoder},
    tags::Tag,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let size: u32 = args.get(1).map(|s| s.parse()).transpose()?.unwrap_or(512);
    let repeats: usize = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(3);
    assert!(size > 0 && repeats > 0);
    // Drive streaming outside Rayon: nesting in pool.install selects its serial
    // fallback. Set RAYON_NUM_THREADS before starting this process instead.
    for epsg in [4326u16, 3857, 32610] {
        let file = tempfile::NamedTempFile::new()?;
        let mut encoder = TiffEncoder::new(File::create(file.path())?)?;
        let mut image = encoder.new_image::<Gray32Float>(size, size)?;
        image.rows_per_strip(64)?;
        let (x, y, step) = match epsg {
            4326 => (-122.5, 37.85, 0.0001),
            3857 => (-13636637.6, 4558128.0, 10.0),
            _ => (544000.0, 4189000.0, 10.0),
        };
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[step, step, 0.0][..])?;
        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[0., 0., 0., x, y, 0.][..])?;
        let keys = if epsg == 4326 {
            [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, epsg]
        } else {
            [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, epsg]
        };
        image
            .encoder()
            .write_tag(Tag::GeoKeyDirectoryTag, &keys[..])?;
        let values: Vec<_> = (0..size as usize * size as usize)
            .map(|i| (i % 31) as f32)
            .collect();
        let expected_sum: f64 = values.iter().map(|&v| v as f64).sum();
        image.write_data(&values)?;
        drop(encoder);
        drop(values);
        // Separate, warm-cache, synchronous decode baseline. Prefetch wait in
        // the streaming run is not decoder CPU time and is reported separately.
        let reader = GeoTiffStreamReader::open(file.path())?;
        let start = Instant::now();
        for i in 0..reader.chunk_layout.total_chunks {
            std::hint::black_box(reader.read_chunk(i)?);
        }
        let decode_ns = start.elapsed().as_nanos();
        for resolution in [7, 10] {
            for (sampling_name, sampling) in [
                ("center", SamplingPattern::center()),
                ("rgss", SamplingPattern::rgss()),
            ] {
                let mut reference = None;
                for budget in [256 * 1024, 64 * 1024 * 1024] {
                    for repeat in 0..repeats {
                        let mut config = MultiResolutionConfig::single(resolution);
                        config.sampling = sampling.clone();
                        config.aggregation_budget_bytes = budget;
                        let reader = GeoTiffStreamReader::open(file.path())?;
                        let start = Instant::now();
                        let mut stream = MultiScanHorizonStreamer::new(reader, &config)?;
                        let mut records = BTreeMap::new();
                        loop {
                            let batch = stream.fetch_next_batch(2048)?;
                            if batch.is_empty() {
                                break;
                            }
                            for record in batch {
                                assert!(records
                                    .insert(
                                        record.h3_index,
                                        (record.accumulator.count, record.accumulator.sum)
                                    )
                                    .is_none());
                            }
                        }
                        let seconds = start.elapsed().as_secs_f64();
                        assert_eq!(
                            records.values().map(|v| v.0).sum::<f64>(),
                            size as f64 * size as f64
                        );
                        assert_eq!(records.values().map(|v| v.1).sum::<f64>(), expected_sum);
                        if let Some(ref expected) = reference {
                            assert_eq!(&records, expected);
                        } else {
                            reference = Some(records.clone());
                        }
                        println!(
                            "{}",
                            serde_json::json!({
                                "epsg": epsg, "size":size, "resolution":resolution,
                                "sampling":sampling_name, "budget":budget, "repeat":repeat,
                                "threads":rayon::current_num_threads(),
                                "instrumented":cfg!(feature="stream-profile"),
                                "seconds":seconds, "mpx_per_second":(size as f64 * size as f64)/seconds/1e6,
                                "cells":records.len(), "decode_baseline_ns":decode_ns.to_string(),
                                "spill_runs":stream.spill_run_count(), "spill_bytes":stream.spill_bytes_written(),
                                "peak_active_bytes":stream.peak_active_bytes(), "profile":stream.metrics,
                            })
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

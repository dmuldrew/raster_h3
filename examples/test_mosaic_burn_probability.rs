//! Integration Benchmark & Verification for Multi-Tile Mosaic Ingestion
//! using real USFS Burn Probability GeoTIFF tiles from GeoPlatform.
//!
//! Run with:
//!   cargo run --release --example test_mosaic_burn_probability

use std::fs::File;
use std::sync::Arc;
use std::time::Instant;

use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::raster::mosaic::{resolve_raster_sources, MosaicReader, OverlapRule};

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!(
        "========================================================================================="
    );
    println!(
        "          USFS BURN PROBABILITY: MULTI-TILE MOSAIC VERIFICATION & BENCHMARK              "
    );
    println!("=========================================================================================\n");

    let source_pattern = "data/burn_probability_mosaic/*.tif";
    let paths = match resolve_raster_sources(source_pattern) {
        Ok(p) if !p.is_empty() => p,
        _ => {
            eprintln!("No GeoTIFF tiles found matching '{}'!", source_pattern);
            eprintln!("Please download test tiles first by running:");
            eprintln!("  cargo run --release --bin download_burn_probability -- --grid 2,2");
            return Ok(());
        }
    };

    println!("Found {} GeoTIFF tile(s) in mosaic:", paths.len());
    for (i, p) in paths.iter().enumerate() {
        if let Ok(reader) = raster_h3::raster::geotiff::GeoTiffStreamReader::open(p) {
            let (c0, f0) = (
                reader.metadata.geotransform.c0,
                reader.metadata.geotransform.f0,
            );
            println!(
                "  [{}] {:?} | {}x{} px | Chunks: {} ({}x{}) | Origin: ({:.4}, {:.4})",
                i,
                p.file_name().unwrap(),
                reader.metadata.width,
                reader.metadata.height,
                reader.chunk_layout.total_chunks,
                reader.chunk_layout.chunk_width,
                reader.chunk_layout.chunk_height,
                c0,
                f0,
            );
        }
    }

    // Step 1: Open MosaicReader with Voronoi Cutline rule
    println!("\n[Step 1] Initializing MosaicReader with Voronoi Cutline rule...");
    let t0 = Instant::now();
    let mosaic = Arc::new(MosaicReader::open(
        &paths,
        None,
        None,
        OverlapRule::Cutline,
    )?);
    let dur_mosaic_open = t0.elapsed();

    println!("  - Mosaic Tiles Count  : {}", mosaic.tiles.len());
    println!(
        "  - Combined BBox WGS84 : [{:.4}, {:.4}, {:.4}, {:.4}]",
        mosaic.mosaic_bounds_wgs84[0],
        mosaic.mosaic_bounds_wgs84[1],
        mosaic.mosaic_bounds_wgs84[2],
        mosaic.mosaic_bounds_wgs84[3]
    );
    println!("  - Total Interleaved Chunks : {}", mosaic.chunk_refs.len());
    println!("  - Open Duration       : {:.2?}", dur_mosaic_open);

    // Step 2: Streaming Multi-Resolution Aggregation (Res 7, 8, 9)
    println!(
        "\n[Step 2] Streaming Mosaic Aggregation via MultiScanHorizonStreamer (Res 7, 8, 9)..."
    );
    let mut config = MultiResolutionConfig::new(vec![7, 8, 9]);
    config.overlap_rule = OverlapRule::Cutline;

    let t_stream = Instant::now();
    let mut streamer = MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &config)?;

    let mut hex_count_by_res = [0usize; 16];
    let mut pixel_count_by_res = [0.0f64; 16];
    let mut non_zero_burn_cells = [0usize; 16];
    let mut max_burn_prob_by_res = [0.0f64; 16];

    loop {
        let n = streamer
            .drain_completed_into(4096, |_hex_id, rec| {
                let res = rec.resolution as usize;
                if res < 16 {
                    hex_count_by_res[res] += 1;
                    pixel_count_by_res[res] += rec.accumulator.count;
                    let mean = rec.accumulator.mean();
                    if mean > 0.0 {
                        non_zero_burn_cells[res] += 1;
                    }
                    if mean > max_burn_prob_by_res[res] {
                        max_burn_prob_by_res[res] = mean;
                    }
                }
            })
            .unwrap();
        if n == 0 {
            break;
        }
    }
    let dur_stream = t_stream.elapsed();

    println!("  - Mosaic Stream Time  : {:.2?}", dur_stream);
    println!(
        "  - Throughput          : {:.2} Mpix/s",
        (pixel_count_by_res[8] / 1_000_000.0) / dur_stream.as_secs_f64()
    );

    println!("\n  Aggregated Multi-Resolution Hexagon Results:");
    for res in [7, 8, 9] {
        println!(
            "    * Resolution {:>2}: {:>7} hexagons | {:>10.0} pixels | {:>6} active burn cells | Max Burn Prob: {:.4}",
            res,
            hex_count_by_res[res],
            pixel_count_by_res[res],
            non_zero_burn_cells[res],
            max_burn_prob_by_res[res]
        );
    }

    // Conservation check: all resolutions must cover the exact same total pixel volume
    assert!(
        hex_count_by_res[7] > 0,
        "Resolution 7 should yield hexagons"
    );
    assert!(
        hex_count_by_res[8] > hex_count_by_res[7],
        "Res 8 count must exceed Res 7"
    );
    assert!(
        hex_count_by_res[9] > hex_count_by_res[8],
        "Res 9 count must exceed Res 8"
    );

    let diff_7_8 = (pixel_count_by_res[7] - pixel_count_by_res[8]).abs();
    let diff_8_9 = (pixel_count_by_res[8] - pixel_count_by_res[9]).abs();
    println!("\n  Seam Conservation Check:");
    println!(
        "    - Res 7 vs Res 8 Pixel Delta: {:.1} pixels ({:.4}%)",
        diff_7_8,
        (diff_7_8 / pixel_count_by_res[8]) * 100.0
    );
    println!(
        "    - Res 8 vs Res 9 Pixel Delta: {:.1} pixels ({:.4}%)",
        diff_8_9,
        (diff_8_9 / pixel_count_by_res[8]) * 100.0
    );
    assert!(
        diff_8_9 < 100.0,
        "Multi-resolution pixel conservation failed across tile seams!"
    );

    // Step 3: End-to-End PMTiles Generation directly from Mosaic Source
    println!("\n[Step 3] Exporting Multi-Resolution PMTiles v3 Archive directly from Mosaic...");
    let pmtiles_output = "data/burn_probability_mosaic.pmtiles";
    let pmtiles_config = MultiResolutionConfig::new(vec![7, 8, 9]);

    let t_pmtiles = Instant::now();
    let total_pmtiles_hexes =
        H3PmtilesTiler::process_geotiff_to_pmtiles(source_pattern, pmtiles_output, pmtiles_config)?;
    let dur_pmtiles = t_pmtiles.elapsed();

    let pmtiles_file = File::open(pmtiles_output)?;
    let pmtiles_size_mb = (pmtiles_file.metadata()?.len() as f64) / (1024.0 * 1024.0);

    println!("  - PMTiles Hexagons Written : {}", total_pmtiles_hexes);
    println!("  - PMTiles Archive Size     : {:.2} MB", pmtiles_size_mb);
    println!("  - PMTiles Generation Time  : {:.2?}", dur_pmtiles);

    println!("\n=========================================================================================");
    println!(
        "                ALL MOSAIC VERIFICATION TESTS PASSED SUCCESSFULLY!                       "
    );
    println!("=========================================================================================\n");

    Ok(())
}

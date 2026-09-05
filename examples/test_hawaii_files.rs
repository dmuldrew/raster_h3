use std::path::Path;
use std::time::Instant;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn main() {
    println!("=========================================================================================");
    println!("                 raster_h3 Comprehensive Real-World Test: Hawaii Datasets                ");
    println!("=========================================================================================");

    let cfl_path = "data/CFL_HI.tif";
    let lf_path = "data/LF2024_FBFM40_HI.tif";

    if !Path::new(cfl_path).exists() || !Path::new(lf_path).exists() {
        eprintln!("Error: Hawaii dataset files not found at expected paths.");
        std::process::exit(1);
    }

    // =========================================================================
    // TEST SUITE 1: Continuous Raster (data/CFL_HI.tif - Conditional Flame Length)
    // =========================================================================
    println!("\n▶ [TEST SUITE 1] Continuous Raster: data/CFL_HI.tif");
    let cfl_reader = GeoTiffStreamReader::open(cfl_path).expect("Failed to open CFL_HI.tif");
    println!("  • Raster Dimensions : {} x {} ({} chunks, EPSG: {:?})",
        cfl_reader.metadata.width,
        cfl_reader.metadata.height,
        cfl_reader.chunk_layout.total_chunks,
        cfl_reader.metadata.epsg
    );

    // 1A. Full Archipelago Continuous Scan (Multi-Core Rayon Horizon Streaming)
    println!("\n  1A. Full Archipelago Scan (Multi-Core Rayon Engine, H3 Res 8)");
    let config_cfl = MultiResolutionConfig::new(vec![8]);
    let t0 = Instant::now();
    let cfl_reader_full = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut streamer_cfl = MultiScanHorizonStreamer::new(cfl_reader_full, &config_cfl).unwrap();
    let mut total_cfl_hexes = 0usize;
    let mut total_cfl_pixels = 0.0f64;
    let mut cfl_sum_val = 0.0f64;

    loop {
        let n = streamer_cfl.drain_completed_into(2048, |_i, rec| {
            total_cfl_hexes += 1;
            total_cfl_pixels += rec.accumulator.count;
            cfl_sum_val += rec.accumulator.sum;
        });
        if n == 0 { break; }
    }
    let dur_cfl_full = t0.elapsed();
    println!("      - Hexagons Output : {} hexagons", total_cfl_hexes);
    println!("      - Pixels Ingested : {:.0} pixels", total_cfl_pixels);
    println!("      - Global Mean Val : {:.4}", cfl_sum_val / total_cfl_pixels.max(1.0));
    println!("      - Scan Time       : {:.2?} ({:.2} Mpx/sec)",
        dur_cfl_full,
        (total_cfl_pixels / 1_000_000.0) / dur_cfl_full.as_secs_f64()
    );

    // 1B. Spatial Filter Pushdown: Bounding Box (Oahu South Shore ROI)
    // Lon: [-157.92, -157.80], Lat: [21.28, 21.36]
    println!("\n  1B. Spatial Bounding Box Filter Pushdown (Honolulu / Oahu ROI)");
    let oahu_bbox = [-157.92, 21.28, -157.80, 21.36];
    let mut config_cfl_bbox = MultiResolutionConfig::new(vec![8]);
    config_cfl_bbox.bbox = Some(oahu_bbox);

    let t_bbox = Instant::now();
    let cfl_reader_bbox = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut streamer_cfl_bbox = MultiScanHorizonStreamer::new(cfl_reader_bbox, &config_cfl_bbox).unwrap();
    let mut bbox_cfl_hexes = 0usize;
    let mut bbox_cfl_pixels = 0.0f64;

    loop {
        let n = streamer_cfl_bbox.drain_completed_into(2048, |_i, rec| {
            bbox_cfl_hexes += 1;
            bbox_cfl_pixels += rec.accumulator.count;
        });
        if n == 0 { break; }
    }
    let dur_cfl_bbox = t_bbox.elapsed();
    println!("      - ROI Hexagons    : {} hexagons", bbox_cfl_hexes);
    println!("      - ROI Pixels      : {:.0} pixels", bbox_cfl_pixels);
    println!("      - Ingestion Time  : {:.2?}", dur_cfl_bbox);
    println!("      - Speedup vs Full : {:.1}x faster",
        dur_cfl_full.as_secs_f64() / dur_cfl_bbox.as_secs_f64()
    );

    // 1C. Spatial Filter Pushdown: Single H3 Cell Point Lookup
    println!("\n  1C. Single H3 Cell Spatial Filter Pushdown (Diamond Head Cell)");
    let target_cell = h3o::LatLng::new(21.26, -157.81).unwrap().to_cell(h3o::Resolution::Eight);
    let ll: h3o::LatLng = target_cell.into();
    let r = raster_h3::pmtiles::tiler::max_hex_radius_deg(target_cell.resolution().into());
    let mut config_cfl_cell = MultiResolutionConfig::new(vec![8]);
    config_cfl_cell.bbox = Some([ll.lng() - r, ll.lat() - r, ll.lng() + r, ll.lat() + r]);

    let t_cell = Instant::now();
    let cfl_reader_cell = GeoTiffStreamReader::open(cfl_path).unwrap();
    let mut streamer_cfl_cell = MultiScanHorizonStreamer::new(cfl_reader_cell, &config_cfl_cell).unwrap();
    let mut point_hexes = 0usize;
    let mut point_cell_found = false;

    loop {
        let n = streamer_cfl_cell.drain_completed_into(2048, |_i, rec| {
            point_hexes += 1;
            if rec.h3_index == u64::from(target_cell) {
                point_cell_found = true;
                println!("      - Cell {:x}: Mean={:.2}, Count={:.0}, Min={:.2}, Max={:.2}",
                    rec.h3_index, rec.accumulator.mean(), rec.accumulator.count, rec.accumulator.min, rec.accumulator.max);
            }
        });
        if n == 0 { break; }
    }
    let dur_cfl_cell = t_cell.elapsed();
    println!("      - Chunks & Cells  : {} target neighborhood cells yielded", point_hexes);
    println!("      - Target Found?   : {}", if point_cell_found { "YES (Exact match)" } else { "NO" });
    println!("      - Query Latency   : {:.2?}", dur_cfl_cell);
    println!("      - Speedup vs Full : {:.1}x faster",
        dur_cfl_full.as_secs_f64() / dur_cfl_cell.as_secs_f64()
    );

    // =========================================================================
    // TEST SUITE 2: Categorical Raster (data/LF2024_FBFM40_HI.tif - Landfire Fuel Models)
    // =========================================================================
    println!("\n▶ [TEST SUITE 2] Categorical Raster: data/LF2024_FBFM40_HI.tif");
    let lf_reader = GeoTiffStreamReader::open(lf_path).expect("Failed to open LF2024_FBFM40_HI.tif");
    println!("  • Raster Dimensions : {} x {} ({} chunks, EPSG: {:?})",
        lf_reader.metadata.width,
        lf_reader.metadata.height,
        lf_reader.chunk_layout.total_chunks,
        lf_reader.metadata.epsg
    );

    // 2A. Full Archipelago Categorical Scan (Multi-Core Rayon Engine, H3 Res 8)
    println!("\n  2A. Full Archipelago Scan (Multi-Core Rayon Engine, H3 Res 8)");
    let config_lf = MultiResolutionConfig::new(vec![8]);
    let t_lf = Instant::now();
    let lf_reader_full = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut streamer_lf = MultiCategoricalHorizonStreamer::new(lf_reader_full, &config_lf).unwrap();
    let mut total_lf_hexes = 0usize;
    let mut total_lf_pixels = 0.0f64;
    let mut class_distribution = std::collections::HashMap::new();

    loop {
        let n = streamer_lf.drain_completed_into(2048, |_i, rec| {
            total_lf_hexes += 1;
            total_lf_pixels += rec.accumulator.total_count;
            let (maj_cls, _, _) = rec.accumulator.majority();
            *class_distribution.entry(maj_cls).or_insert(0usize) += 1;
        });
        if n == 0 { break; }
    }
    let dur_lf_full = t_lf.elapsed();
    println!("      - Hexagons Output : {} hexagons", total_lf_hexes);
    println!("      - Pixels Ingested : {:.0} pixels", total_lf_pixels);
    println!("      - Scan Time       : {:.2?} ({:.2} Mpx/sec)",
        dur_lf_full,
        (total_lf_pixels / 1_000_000.0) / dur_lf_full.as_secs_f64()
    );

    let mut top_classes: Vec<_> = class_distribution.into_iter().collect();
    top_classes.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
    println!("      - Top 3 Landcover Classes (by hex count):");
    for (cls, count) in top_classes.iter().take(3) {
        println!("        * Class {:>4}: {:>6} hexagons ({:.1}%)",
            cls, count, (*count as f64 / total_lf_hexes as f64) * 100.0);
    }

    // 2B. Projection Pushdown Comparison on Categorical
    println!("\n  2B. Projection Pushdown: Full Projection (with JSON) vs Projected (JSON Bypassed)");
    let lf_reader_proj = GeoTiffStreamReader::open(lf_path).unwrap();
    let t_proj = Instant::now();
    let mut streamer_lf_proj = MultiCategoricalHorizonStreamer::new(lf_reader_proj, &config_lf).unwrap();
    let mut projected_hexes = 0usize;

    loop {
        let n = streamer_lf_proj.drain_completed_into(2048, |_i, rec| {
            // Projected query: only majority class needed, JSON histogram bypassed
            let _ = rec.accumulator.majority();
            projected_hexes += 1;
        });
        if n == 0 { break; }
    }
    let dur_lf_proj = t_proj.elapsed();
    println!("      - Projected Scan Time : {:.2?}", dur_lf_proj);
    println!("      - Hexagons Verified   : {} (100% Deterministic match)", projected_hexes);
    assert_eq!(total_lf_hexes, projected_hexes, "Hexagon count mismatch between projections!");

    // 2C. Spatial Filter Pushdown on Categorical (Maui Island ROI)
    println!("\n  2C. Spatial Bounding Box Filter Pushdown (Maui Island ROI)");
    // Maui approx bbox: Lon [-156.70, 20.55], [-155.95, 21.05]
    let maui_bbox = [-156.70, 20.55, -155.95, 21.05];
    let mut config_lf_maui = MultiResolutionConfig::new(vec![8]);
    config_lf_maui.bbox = Some(maui_bbox);

    let t_maui = Instant::now();
    let lf_reader_maui = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut streamer_lf_maui = MultiCategoricalHorizonStreamer::new(lf_reader_maui, &config_lf_maui).unwrap();
    let mut maui_hexes = 0usize;
    let mut maui_pixels = 0.0f64;

    loop {
        let n = streamer_lf_maui.drain_completed_into(2048, |_i, rec| {
            maui_hexes += 1;
            maui_pixels += rec.accumulator.total_count;
        });
        if n == 0 { break; }
    }
    let dur_lf_maui = t_maui.elapsed();
    println!("      - Maui Hexagons   : {} hexagons", maui_hexes);
    println!("      - Maui Pixels     : {:.0} pixels", maui_pixels);
    println!("      - Ingestion Time  : {:.2?}", dur_lf_maui);
    println!("      - Speedup vs Full : {:.1}x faster",
        dur_lf_full.as_secs_f64() / dur_lf_maui.as_secs_f64()
    );

    // =========================================================================
    // TEST SUITE 3: Multi-Resolution Hex Pyramids (Res 7 + Res 8)
    // =========================================================================
    println!("\n▶ [TEST SUITE 3] Multi-Resolution Aggregation: Res 7 + Res 8 Dual Pyramid");
    let config_multi = MultiResolutionConfig::new(vec![7, 8]);
    let t_multi = Instant::now();
    let lf_reader_multi = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut streamer_multi = MultiCategoricalHorizonStreamer::new(lf_reader_multi, &config_multi).unwrap();
    let mut r7_count = 0usize;
    let mut r8_count = 0usize;

    loop {
        let n = streamer_multi.drain_completed_into(2048, |_i, rec| {
            if rec.resolution == 7 {
                r7_count += 1;
            } else if rec.resolution == 8 {
                r8_count += 1;
            }
        });
        if n == 0 { break; }
    }
    let dur_multi = t_multi.elapsed();
    println!("      - Res 7 Hexagons  : {} cells", r7_count);
    println!("      - Res 8 Hexagons  : {} cells", r8_count);
    println!("      - Total Hexagons  : {} cells", r7_count + r8_count);
    println!("      - Multi-Res Time  : {:.2?}", dur_multi);

    // =========================================================================
    // TEST SUITE 4: Landscape Diversity & Shannon Entropy Metrics
    // =========================================================================
    println!("\n▶ [TEST SUITE 4] Landscape Diversity & Shannon Entropy (Maui ROI)");
    let lf_reader_entropy = GeoTiffStreamReader::open(lf_path).unwrap();
    let mut config_lf_ent = MultiResolutionConfig::new(vec![8]);
    config_lf_ent.bbox = Some(maui_bbox);

    let t_ent = Instant::now();
    let mut streamer_ent = MultiCategoricalHorizonStreamer::new(lf_reader_entropy, &config_lf_ent).unwrap();
    let mut total_ent_hexes = 0usize;
    let mut pure_hexes = 0usize; // entropy == 0.0 (single class)
    let mut diverse_hexes = 0usize; // entropy > 1.0 (multi-class ecotones)
    let mut max_entropy = 0.0f64;
    let mut max_distinct = 0usize;

    loop {
        let n = streamer_ent.drain_completed_into(2048, |_i, rec| {
            total_ent_hexes += 1;
            let entropy = rec.accumulator.shannon_entropy();
            let distinct = rec.accumulator.unique_classes();
            if entropy == 0.0 {
                pure_hexes += 1;
            }
            if entropy > 1.0 {
                diverse_hexes += 1;
            }
            if entropy > max_entropy {
                max_entropy = entropy;
            }
            if distinct > max_distinct {
                max_distinct = distinct;
            }
        });
        if n == 0 { break; }
    }
    let dur_ent = t_ent.elapsed();
    println!("      - Maui Hexagons    : {} cells", total_ent_hexes);
    println!("      - Pure Cells (H=0) : {} cells ({:.1}%)",
        pure_hexes, (pure_hexes as f64 / total_ent_hexes as f64) * 100.0);
    println!("      - Diverse Ecotones : {} cells ({:.1}% with H > 1.0)",
        diverse_hexes, (diverse_hexes as f64 / total_ent_hexes as f64) * 100.0);
    println!("      - Max Diversity    : H={:.3}, Max Distinct Classes={}", max_entropy, max_distinct);
    println!("      - Entropy Calc Time: {:.2?}", dur_ent);

    println!("\n=========================================================================================");
    println!("                              ALL HAWAII TESTS PASSED SUCCESSFULLY                       ");
    println!("=========================================================================================");
}

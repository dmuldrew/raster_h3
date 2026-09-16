//! raster_h3 GeoTIFF / Cloud Optimized GeoTIFF (COG) Inspector
//!
//! Inspects local GeoTIFF files or remote HTTP/S3 COG URLs, displaying
//! raster dimensions, chunk layouts, spatial reference, geotransform,
//! projected/WGS84 bounding boxes, and H3 cell mappings.
//!
//! Usage:
//!   cargo run --bin inspect_tif -- \<path_or_url\> \[options\]

use h3o::{LatLng, Resolution};
use serde_json::json;
use std::fs;
use std::path::Path;

use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::raster::RasterSource;

fn print_help() {
    println!("raster_h3 GeoTIFF / COG Inspector");
    println!("Usage:");
    println!("  inspect_tif <PATH_OR_URL> [OPTIONS]");
    println!();
    println!("Options:");
    println!("  --point <lat,lon>     Query a specific WGS84 coordinate inside the raster");
    println!("  --h3-res <r>          H3 resolution for cell mapping (0 to 15, default: 8)");
    println!("  --json                Output metadata in structured JSON format");
    println!("  -h, --help            Show this help menu");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        if args.len() < 2 {
            std::process::exit(1);
        }
        return;
    }

    let mut target = String::new();
    let mut point_query: Option<(f64, f64)> = None;
    let mut h3_res_u8 = 8u8;
    let mut json_output = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--point" => {
                if i + 1 < args.len() {
                    let parts: Vec<f64> = args[i + 1]
                        .split(',')
                        .filter_map(|s| s.trim().parse::<f64>().ok())
                        .collect();
                    if parts.len() == 2 {
                        point_query = Some((parts[0], parts[1]));
                    } else {
                        eprintln!(
                            "Error: --point requires lat,lon (e.g. --point 21.3069,-157.8583)"
                        );
                        std::process::exit(1);
                    }
                    i += 1;
                }
            }
            "--h3-res" | "-r" => {
                if i + 1 < args.len() {
                    if let Ok(r) = args[i + 1].trim().parse::<u8>() {
                        if r <= 15 {
                            h3_res_u8 = r;
                        } else {
                            eprintln!("Error: H3 resolution must be between 0 and 15");
                            std::process::exit(1);
                        }
                    }
                    i += 1;
                }
            }
            "--json" => {
                json_output = true;
            }
            arg if !arg.starts_with('-') && target.is_empty() => {
                target = arg.to_string();
            }
            _ => {}
        }
        i += 1;
    }

    if target.is_empty() {
        eprintln!("Error: Target GeoTIFF path or URL must be specified.");
        std::process::exit(1);
    }

    let reader = match GeoTiffStreamReader::open(&target) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: Failed to open GeoTIFF '{}': {}", target, e);
            std::process::exit(1);
        }
    };

    let meta = &reader.metadata;
    let chunk = &reader.chunk_layout;
    let gt = &meta.geotransform;

    // Projected extent
    let (tl_x, tl_y) = gt.pixel_to_coord(0.0, 0.0);
    let (tr_x, tr_y) = gt.pixel_to_coord(meta.width as f64, 0.0);
    let (br_x, br_y) = gt.pixel_to_coord(meta.width as f64, meta.height as f64);
    let (bl_x, bl_y) = gt.pixel_to_coord(0.0, meta.height as f64);
    let (c_proj_x, c_proj_y) = gt.pixel_to_coord(meta.width as f64 * 0.5, meta.height as f64 * 0.5);

    let proj_min_x = tl_x.min(tr_x).min(br_x).min(bl_x);
    let proj_max_x = tl_x.max(tr_x).max(br_x).max(bl_x);
    let proj_min_y = tl_y.min(tr_y).min(br_y).min(bl_y);
    let proj_max_y = tl_y.max(tr_y).max(br_y).max(bl_y);

    // Coordinate transformation to WGS84
    let transformer_res = CrsTransformer::from_crs_or_epsg(meta.epsg, meta.proj_string.as_deref());
    let mut wgs84_bounds = None;
    let mut center_wgs84 = None;

    if let Ok(ref transformer) = transformer_res {
        let p_tl = transformer.transform_point(tl_x, tl_y);
        let p_tr = transformer.transform_point(tr_x, tr_y);
        let p_br = transformer.transform_point(br_x, br_y);
        let p_bl = transformer.transform_point(bl_x, bl_y);
        let p_c = transformer.transform_point(c_proj_x, c_proj_y);

        if let (Ok(tl), Ok(tr), Ok(br), Ok(bl), Ok(c)) = (p_tl, p_tr, p_br, p_bl, p_c) {
            let min_lon = tl.0.min(tr.0).min(br.0).min(bl.0);
            let max_lon = tl.0.max(tr.0).max(br.0).max(bl.0);
            let min_lat = tl.1.min(tr.1).min(br.1).min(bl.1);
            let max_lat = tl.1.max(tr.1).max(br.1).max(bl.1);
            wgs84_bounds = Some([min_lon, min_lat, max_lon, max_lat]);
            center_wgs84 = Some((c.1, c.0)); // (lat, lon)
        }
    }

    let h3_res_enum = Resolution::try_from(h3_res_u8).unwrap_or(Resolution::Eight);

    // H3 Cell at center
    let center_h3 = center_wgs84.and_then(|(lat, lon)| {
        LatLng::new(lat, lon).ok().map(|ll| {
            let cell = ll.to_cell(h3_res_enum);
            let cell_u64: u64 = cell.into();
            let cell_hex = format!("{:x}", cell_u64);
            let center: LatLng = cell.into();
            (cell_hex, cell_u64, center.lat(), center.lng())
        })
    });

    // Optional Point Query
    let point_result = point_query.map(|(lat, lon)| {
        let h3_cell = LatLng::new(lat, lon).ok().map(|ll| {
            let cell = ll.to_cell(h3_res_enum);
            let cell_u64: u64 = cell.into();
            (format!("{:x}", cell_u64), cell_u64)
        });
        h3_cell
    });

    let source_type_str = match reader.source {
        RasterSource::Local(_) => "Local File",
        RasterSource::Remote(_) => "Remote HTTP(S) COG",
    };

    let file_size_mb = if Path::new(&target).exists() {
        fs::metadata(&target)
            .ok()
            .map(|m| m.len() as f64 / (1024.0 * 1024.0))
    } else {
        None
    };

    if json_output {
        let json_data = json!({
            "target": target,
            "source_type": source_type_str,
            "file_size_mb": file_size_mb,
            "dimensions": {
                "width": meta.width,
                "height": meta.height,
                "bands": meta.samples_per_pixel,
            },
            "chunk_layout": {
                "chunk_width": chunk.chunk_width,
                "chunk_height": chunk.chunk_height,
                "chunks_across": chunk.chunks_across,
                "chunks_down": chunk.chunks_down,
                "total_chunks": chunk.total_chunks,
            },
            "nodata": meta.nodata,
            "spatial_reference": {
                "epsg": meta.epsg,
                "proj_string": meta.proj_string,
                "geotransform": [gt.c0, gt.a, gt.b, gt.f0, gt.d, gt.e],
                "pixel_size": [gt.a.abs(), gt.e.abs()]
            },
            "projected_extent": {
                "min_x": proj_min_x,
                "min_y": proj_min_y,
                "max_x": proj_max_x,
                "max_y": proj_max_y
            },
            "wgs84_bounds": wgs84_bounds,
            "center_wgs84": center_wgs84,
            "center_h3": center_h3.map(|(hex, u, lat, lon)| {
                json!({
                    "resolution": h3_res_u8,
                    "hex": hex,
                    "index": u,
                    "center_lat": lat,
                    "center_lon": lon
                })
            }),
            "point_query": point_query.map(|(lat, lon)| {
                json!({
                    "lat": lat,
                    "lon": lon,
                    "h3": point_result.flatten().map(|(hex, u)| json!({ "hex": hex, "index": u }))
                })
            })
        });
        println!("{}", serde_json::to_string_pretty(&json_data).unwrap());
    } else {
        println!(
            "================================================================================"
        );
        println!("raster_h3 GeoTIFF / COG Inspector");
        println!(
            "================================================================================"
        );
        if let Some(mb) = file_size_mb {
            println!(
                "Source:        {} ({}, {:.2} MB)",
                target, source_type_str, mb
            );
        } else {
            println!("Source:        {} ({})", target, source_type_str);
        }
        println!(
            "Dimensions:    {} x {} pixels ({} band{})",
            meta.width,
            meta.height,
            meta.samples_per_pixel,
            if meta.samples_per_pixel > 1 { "s" } else { "" }
        );
        println!(
            "Chunk Layout:  {} x {} px ({} x {} = {} total chunks)",
            chunk.chunk_width,
            chunk.chunk_height,
            chunk.chunks_across,
            chunk.chunks_down,
            chunk.total_chunks
        );
        if let Some(nd) = meta.nodata {
            println!("NoData Value:  {}", nd);
        } else {
            println!("NoData Value:  None");
        }
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("Spatial Reference:");
        println!("  EPSG:        {:?}", meta.epsg);
        if let Some(ref proj) = meta.proj_string {
            println!("  Projection:  {}", proj);
        }
        if let Err(ref e) = transformer_res {
            println!("  Warning:     {}", e);
        }
        println!("  Origin:      ({:.6}, {:.6})", gt.c0, gt.f0);
        println!("  Pixel Size:  {:.6} x {:.6}", gt.a.abs(), gt.e.abs());
        println!(
            "--------------------------------------------------------------------------------"
        );
        println!("Extents:");
        println!(
            "  Projected:   X: [{:.3}, {:.3}], Y: [{:.3}, {:.3}]",
            proj_min_x, proj_max_x, proj_min_y, proj_max_y
        );
        if let Some([min_lon, min_lat, max_lon, max_lat]) = wgs84_bounds {
            println!(
                "  WGS84 Bounds: [{:.6}, {:.6}, {:.6}, {:.6}]",
                min_lon, min_lat, max_lon, max_lat
            );
        }
        if let Some((c_lat, c_lon)) = center_wgs84 {
            println!("  Center:      (lat: {:.6}°, lon: {:.6}°)", c_lat, c_lon);
        }
        if let Some((hex, u, c_lat, c_lon)) = center_h3 {
            println!(
                "--------------------------------------------------------------------------------"
            );
            println!("H3 Cell at Center (Resolution {}):", h3_res_u8);
            println!("  Cell Hex:    {}", hex);
            println!("  Cell Index:  {}", u);
            println!("  Cell Center: (lat: {:.6}°, lon: {:.6}°)", c_lat, c_lon);
        }
        if let Some((q_lat, q_lon)) = point_query {
            println!(
                "--------------------------------------------------------------------------------"
            );
            println!("Point Query (lat: {:.6}°, lon: {:.6}°):", q_lat, q_lon);
            if let Some(Some((hex, u))) = point_result {
                println!("  H3 Res {}:    {} ({})", h3_res_u8, hex, u);
            }
        }
        println!(
            "================================================================================"
        );
    }
}

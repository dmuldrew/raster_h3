//! Integration tests verifying scanline horizon bounds and eviction soundness
//! in conic and transverse projections.
//!
//! Validates:
//! 1. Exact central meridian latitude capture on Albers strips (preventing the 2.5–3.6° underestimation defect).
//! 2. Bounding box intersection along the interior parallel arc of conic strips.
//! 3. Zero duplicate cell emission and complete metric conservation in multi-horizon streaming.
//! 4. Lower-bound headroom in `max_hex_radius_deg` across high latitudes.

use std::collections::HashSet;
use std::fs::File;
use tempfile::TempDir;

use raster_h3::aggregator::horizon_streamer::{chunk_intersects_bbox, compute_cell_south_lat};
use raster_h3::aggregator::multi_horizon::{MultiResolutionConfig, MultiScanHorizonStreamer};
use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::pmtiles::pyramid::max_hex_radius_deg;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::raster::geotransform::GeoTransform;
use raster_h3::raster::mosaic::{MosaicReader, OverlapRule};
use raster_h3::raster::RasterChunk;
use tiff::encoder::colortype;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

/// Helper to encode a GeoTIFF with custom projection tags and tiepoints
fn create_albers_conus_geotiff(
    path: &std::path::Path,
    width: u32,
    height: u32,
    origin_x: f64,
    origin_y: f64,
    pixel_size_x: f64,
    pixel_size_y: f64,
    fill_value: f32,
) {
    let file = File::create(path).expect("create file");
    let mut encoder = TiffEncoder::new(file).expect("create encoder");

    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
        .expect("new image");

    let tiepoint = [0.0, 0.0, 0.0, origin_x, origin_y, 0.0];
    let pixel_scale = [pixel_size_x, pixel_size_y, 0.0];

    // EPSG:5070 Projected CRS GeoKeys
    // KeyDirectoryVersion: 1, Revision: 1, MinorRevision: 0, NumberOfKeys: 2
    // GTModelTypeGeoKey (1024): ModelTypeProjected (1)
    // ProjectedCSTypeGeoKey (3072): EPSG:5070
    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 5070];

    image
        .encoder()
        .write_tag(Tag::ModelTiepointTag, &tiepoint[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &pixel_scale[..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .unwrap();

    let data = vec![fill_value; (width * height) as usize];
    image.write_data(&data).expect("write data");
}

#[test]
fn test_albers_strip_north_lat_captures_central_meridian() {
    let temp_dir = TempDir::new().unwrap();
    let tif_path = temp_dir.path().join("albers_strip.tif");

    // Create a 2-strip Albers GeoTIFF: width 10,000, height 2 (each strip is 10,000 x 1)
    // Straddling the central meridian (x = 0): x from -2,000,000 to +2,000,000
    // Pixel size: 400m
    create_albers_conus_geotiff(
        &tif_path,
        10000,
        2,
        -2000000.0,
        3200000.0,
        400.0,
        400.0,
        1.0,
    );

    let reader = GeoTiffStreamReader::open(&tif_path).unwrap();
    let mosaic = MosaicReader::from_single_reader(reader, None, None).unwrap();

    assert!(!mosaic.chunk_refs.is_empty());

    let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
    let (_, lat_west) = tf.transform_point(-2000000.0, 3200000.0).unwrap();
    let (_, lat_east) = tf.transform_point(2000000.0, 3200000.0).unwrap();
    let (_, lat_cm) = tf.transform_point(0.0, 3200000.0).unwrap();

    let corner_max = lat_west.max(lat_east);
    assert!(lat_cm > corner_max + 2.0, "Central meridian must be > 2° higher than corners");

    // Chunk 0 is the top strip. Its north_lat must match the central meridian latitude:
    let chunk0 = &mosaic.chunk_refs[0];
    assert!(
        (chunk0.north_lat - lat_cm).abs() < 1e-4,
        "Chunk north_lat ({}) must match central meridian lat ({})",
        chunk0.north_lat,
        lat_cm
    );
    assert!(
        chunk0.north_lat > corner_max + 2.0,
        "Chunk north_lat ({}) must exceed corner max ({})",
        chunk0.north_lat,
        corner_max
    );
}

#[test]
fn test_conic_chunk_intersects_bbox_interior_arc() {
    let tf = CrsTransformer::from_crs_or_epsg(Some(5070), None).unwrap();
    let gt = GeoTransform {
        c0: -2000000.0,
        a: 400.0,
        b: 0.0,
        f0: 3200000.0,
        d: 0.0,
        e: -400.0,
    };

    // Strip covers x: [-2,000,000, 2,000,000], y: [3,199,600, 3,200,000]
    let chunk = RasterChunk {
        col_offset: 0,
        row_offset: 0,
        width: 10000,
        height: 1,
    };

    let (_, lat_west) = tf.transform_point(-2000000.0, 3200000.0).unwrap();
    let (_, lat_east) = tf.transform_point(2000000.0, 3200000.0).unwrap();
    let (_, lat_cm) = tf.transform_point(0.0, 3200000.0).unwrap();
    let corner_max = lat_west.max(lat_east);

    // Place a bbox strictly in the interior arc:
    // Latitude is between corner_max + 0.5° and lat_cm + 0.1°
    // Longitude is near central meridian [-97.0, -95.0]
    let bbox_interior = [-97.0, corner_max + 0.5, -95.0, lat_cm + 0.1];

    // The corners of the chunk DO NOT intersect this bbox (their lat is < corner_max < corner_max + 0.5).
    // But the interior arc of the strip DOES pass through this bbox.
    let intersects = chunk_intersects_bbox(&chunk, &gt, &tf, &bbox_interior);
    assert!(
        intersects,
        "chunk_intersects_bbox must detect interior-arc intersection"
    );

    // Conversely, a bbox placed far north of lat_cm should not intersect:
    let bbox_too_far_north = [-97.0, lat_cm + 1.0, -95.0, lat_cm + 2.0];
    assert!(
        !chunk_intersects_bbox(&chunk, &gt, &tf, &bbox_too_far_north),
        "chunk_intersects_bbox should not intersect bbox far north of arc"
    );
}

#[test]
fn test_albers_multi_horizon_streaming_no_duplicate_cells() {
    let temp_dir = TempDir::new().unwrap();
    let tif_path = temp_dir.path().join("albers_multi_strip.tif");

    // Create a 4-strip Albers raster (1000 x 4), width 1000 pixels at 1km = 1000 km wide
    // Centered at central meridian: x from -500,000 to +500,000, y from 2,500,000 to 2,496,000
    create_albers_conus_geotiff(
        &tif_path,
        1000,
        4,
        -500000.0,
        2500000.0,
        1000.0,
        1000.0,
        10.0,
    );

    let reader = GeoTiffStreamReader::open(&tif_path).unwrap();
    let mut config = MultiResolutionConfig::new(vec![5]);
    config.overlap_rule = OverlapRule::Cutline;

    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();

    let mut emitted_cells: Vec<u64> = Vec::new();
    let mut total_pixel_count = 0.0f64;

    while !streamer.is_finished() {
        let batch = streamer.fetch_next_batch(1024).unwrap();
        if batch.is_empty() && streamer.is_finished() {
            break;
        }
        for record in batch {
            emitted_cells.push(record.h3_index);
            total_pixel_count += record.accumulator.count;
        }
    }

    assert!(!emitted_cells.is_empty(), "Should emit H3 cells");

    // Invariant: ZERO DUPLICATES across batch and strip boundaries!
    let mut unique_set = HashSet::new();
    for cell in &emitted_cells {
        assert!(
            unique_set.insert(*cell),
            "Duplicate H3 cell emitted across strip boundaries: {:x}",
            cell
        );
    }

    // Total pixel count must equal 1000 * 4 = 4000
    assert!(
        (total_pixel_count - 4000.0).abs() < 1e-3,
        "All pixels must be conserved without double counting: got {}",
        total_pixel_count
    );
}

#[test]
fn test_max_hex_radius_headroom_all_resolutions() {
    // Verify that max_hex_radius_deg provides generous headroom over theoretical circumradius
    // and geodesic bulge even at 70° latitude:
    for res in 0..=15 {
        let r_deg = max_hex_radius_deg(res);
        assert!(r_deg > 0.0);
        if res < 15 {
            assert!(
                r_deg > max_hex_radius_deg(res + 1),
                "Radius must decrease with resolution"
            );
        }
    }

    // Res 0-5 should have ample headroom
    assert!(max_hex_radius_deg(0) >= 13.5);
    assert!(max_hex_radius_deg(1) >= 5.2);
    assert!(max_hex_radius_deg(2) >= 1.95);
    assert!(max_hex_radius_deg(3) >= 0.74);
    assert!(max_hex_radius_deg(4) >= 0.28);
    assert!(max_hex_radius_deg(5) >= 0.106);

    // Cell south latitude exact boundary check
    let cell_res0 = h3o::LatLng::new(60.0, 0.0)
        .unwrap()
        .to_cell(h3o::Resolution::Zero);
    let center_lat = h3o::LatLng::from(cell_res0).lat();
    let south_lat0 = compute_cell_south_lat(cell_res0.into());
    assert!(south_lat0 < center_lat, "South lat must be south of center lat");
    for v in cell_res0.boundary().iter() {
        assert!(south_lat0 <= v.lat(), "South lat must bound all boundary vertices");
    }

    // Verify worst-case cells identified in the geodetic correctness audit:
    // Res 2: 820407fffffffff (measured reach 1.8045°, old radius 1.7° under-reached by 11.6km)
    let cell_res2 = 0x820407fffffffffu64;
    let s_lat2 = compute_cell_south_lat(cell_res2);
    let c_lat2 = h3o::LatLng::from(h3o::CellIndex::try_from(cell_res2).unwrap()).lat();
    let span2 = c_lat2 - s_lat2;
    assert!(span2 > 1.7, "Empirical reach must exceed old 1.7° limit");
    assert!(span2 <= max_hex_radius_deg(2), "Span must be safely bounded by updated radius 1.95°");

    // Res 3: 833201fffffffff (measured reach 0.6820°, old radius 0.65° under-reached by 3.6km)
    let cell_res3 = 0x833201fffffffffu64;
    let s_lat3 = compute_cell_south_lat(cell_res3);
    let c_lat3 = h3o::LatLng::from(h3o::CellIndex::try_from(cell_res3).unwrap()).lat();
    let span3 = c_lat3 - s_lat3;
    assert!(span3 > 0.65, "Empirical reach must exceed old 0.65° limit");
    assert!(span3 <= max_hex_radius_deg(3), "Span must be safely bounded by updated radius 0.74°");

    // Res 4: 8404001ffffffff (measured reach 0.2579°, old radius 0.25° under-reached by 875m)
    let cell_res4 = 0x8404001ffffffffu64;
    let s_lat4 = compute_cell_south_lat(cell_res4);
    let c_lat4 = h3o::LatLng::from(h3o::CellIndex::try_from(cell_res4).unwrap()).lat();
    let span4 = c_lat4 - s_lat4;
    assert!(span4 > 0.25, "Empirical reach must exceed old 0.25° limit");
    assert!(span4 <= max_hex_radius_deg(4), "Span must be safely bounded by updated radius 0.28°");
}

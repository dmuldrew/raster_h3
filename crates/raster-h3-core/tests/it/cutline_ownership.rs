//! `Cutline` ownership with cached centroid vectors must decide exactly as the
//! original per-call computation did, for every tile and point, including
//! exact distance ties, antimeridian-straddling and near-polar tiles.

use crate::helpers::TestGeoTiffBuilder;

use std::path::PathBuf;

use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};

/// The original distance: unit vectors recomputed from degrees on every call.
fn reference_chord_sq(a: (f64, f64), b: (f64, f64)) -> f64 {
    let unit = |(lon, lat): (f64, f64)| {
        let (lon, lat) = (lon.to_radians(), lat.to_radians());
        (lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin())
    };
    let (p, q) = (unit(a), unit(b));
    (p.0 - q.0).powi(2) + (p.1 - q.1).powi(2) + (p.2 - q.2).powi(2)
}

/// The original `Cutline` rule from `MosaicReader::is_point_owned_by`.
fn reference_cutline_owned(m: &MosaicReader, tile_idx: usize, lon: f64, lat: f64) -> bool {
    let own = reference_chord_sq((lon, lat), m.tiles[tile_idx].centroid_wgs84);
    !m.tiles.iter().enumerate().any(|(idx, t)| {
        if idx == tile_idx {
            return false;
        }
        let d = reference_chord_sq((lon, lat), t.centroid_wgs84);
        (d < own || (d == own && idx < tile_idx)) && t.contains(lon, lat)
    })
}

/// Deterministic uniform values in `[0, 1)` (SplitMix64).
struct Rng(u64);

impl Rng {
    fn next_f64(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Write `(origin_lon, origin_lat, width_px)` tiles of `px`-degree pixels.
fn write_tiles(dir: &std::path::Path, px: f64, tiles: &[(f64, f64, u32)]) -> Vec<PathBuf> {
    tiles
        .iter()
        .enumerate()
        .map(|(i, &(lon, lat, size))| {
            let path = dir.join(format!("tile_{i:02}.tif"));
            TestGeoTiffBuilder::new(size, size)
                .origin(lon, lat)
                .pixel_size(px)
                .write_constant(&path, 1.0f32)
                .unwrap();
            path
        })
        .collect()
}

/// Compare new and reference ownership for every tile at `n` points drawn
/// from `lon_range` × `lat_range` (longitudes wrapped to [-180, 180)).
/// Returns (points inside ≥ 2 tiles, points with an exact distance tie).
fn assert_matches_reference(
    paths: &[PathBuf],
    lon_range: (f64, f64),
    lat_range: (f64, f64),
    n: usize,
) -> (usize, usize) {
    let mosaic = MosaicReader::open(paths, None, None, OverlapRule::Cutline).unwrap();
    let mut rng = Rng(0xC0FFEE ^ paths.len() as u64);
    let (mut overlapping, mut ties) = (0, 0);
    for _ in 0..n {
        let raw_lon = lon_range.0 + rng.next_f64() * (lon_range.1 - lon_range.0);
        let lon = (raw_lon + 180.0).rem_euclid(360.0) - 180.0;
        let lat = lat_range.0 + rng.next_f64() * (lat_range.1 - lat_range.0);

        let containing = mosaic.tiles.iter().filter(|t| t.contains(lon, lat)).count();
        overlapping += usize::from(containing >= 2);
        let dists: Vec<f64> = mosaic
            .tiles
            .iter()
            .map(|t| reference_chord_sq((lon, lat), t.centroid_wgs84))
            .collect();
        ties += usize::from((1..dists.len()).any(|i| dists[..i].contains(&dists[i])));

        for tile_idx in 0..mosaic.tiles.len() {
            assert_eq!(
                mosaic.is_point_owned_by(tile_idx, lon, lat),
                reference_cutline_owned(&mosaic, tile_idx, lon, lat),
                "tile {tile_idx} at ({lon}, {lat})"
            );
        }
    }
    (overlapping, ties)
}

#[test]
fn cutline_matches_reference_on_overlapping_grid() {
    let dir = tempfile::tempdir().unwrap();
    // 4×4 grid of 96 px tiles overlapping their neighbours by 16 px.
    let stride = 80.0 * 0.001;
    let tiles: Vec<_> = (0..16)
        .map(|i| {
            (
                -100.0 + (i % 4) as f64 * stride,
                40.0 - (i / 4) as f64 * stride,
                96,
            )
        })
        .collect();
    let paths = write_tiles(dir.path(), 0.001, &tiles);
    let span = 3.0 * stride + 0.096;
    let (overlapping, _) =
        assert_matches_reference(&paths, (-100.0, -100.0 + span), (40.0 - span, 40.0), 4000);
    assert!(
        overlapping > 500,
        "fixture must exercise overlaps ({overlapping})"
    );
}

#[test]
fn cutline_matches_reference_with_exact_ties() {
    let dir = tempfile::tempdir().unwrap();
    // Three identical footprints share a centroid, so every distance to them
    // ties and the lower index must win; a fourth tile is offset.
    let tiles = [
        (10.0, 5.0, 64),
        (10.0, 5.0, 64),
        (10.0, 5.0, 64),
        (10.03, 4.97, 64),
    ];
    let paths = write_tiles(dir.path(), 0.001, &tiles);
    let (overlapping, ties) = assert_matches_reference(&paths, (10.0, 10.094), (4.906, 5.0), 3000);
    assert!(
        overlapping > 500 && ties > 500,
        "{overlapping} overlaps, {ties} ties"
    );

    // Within the shared footprint, only the first duplicate owns the point
    // unless the offset tile is nearer.
    let mosaic = MosaicReader::open(&paths, None, None, OverlapRule::Cutline).unwrap();
    assert!(mosaic.is_point_owned_by(0, 10.001, 4.999));
    assert!(!mosaic.is_point_owned_by(1, 10.001, 4.999));
    assert!(!mosaic.is_point_owned_by(2, 10.001, 4.999));
}

#[test]
fn cutline_matches_reference_across_antimeridian_and_near_pole() {
    let dir = tempfile::tempdir().unwrap();
    // One tile stored on the unwrapped branch (180.95..181.15, i.e.
    // -179.05..-178.85) partly overlapping one on the wrapped branch
    // (-179.0..-178.8), so containment needs the longitude-period fallback.
    let am = write_tiles(
        dir.path(),
        0.001,
        &[(180.95, 1.0, 200), (-179.0, 0.95, 200)],
    );
    let (overlapping, _) = assert_matches_reference(&am, (-179.05, -178.8), (0.75, 1.0), 3000);
    assert!(overlapping > 300, "antimeridian overlaps ({overlapping})");

    // Near-polar tiles where longitudes converge.
    let polar_dir = dir.path().join("polar");
    std::fs::create_dir(&polar_dir).unwrap();
    let polar = write_tiles(
        &polar_dir,
        0.01,
        &[(-10.0, 89.9, 100), (-9.5, 89.85, 100), (-9.8, 89.95, 100)],
    );
    let (overlapping, _) = assert_matches_reference(&polar, (-10.0, -8.5), (88.85, 89.95), 3000);
    assert!(overlapping > 300, "polar overlaps ({overlapping})");
}

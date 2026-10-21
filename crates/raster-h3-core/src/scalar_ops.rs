//! Engine-neutral H3 scalar and utility operations shared across DuckDB, DataFusion,
//! Polars/Python, PostgreSQL, and CLI targets.

use std::path::PathBuf;

pub use h3o::geom::ContainmentMode;
use h3o::geom::{MultiPolygon, PolyfillConfig, ToCells};
use h3o::{CellIndex, LatLng, Resolution};

use crate::encoding::{
    fast_hex_u64, h3_index_to_wkb, parse_hex_u64, wkb_to_multipolygon, WkbBuf, WKB_BUF_LEN,
};
use crate::raster::geotiff::GeoTiffStreamReader;

/// Approximate H3 cell areas in m^2 by resolution (0 to 15) for query planner cardinality estimation.
pub const H3_AREA_M2: [f64; 16] = [
    4.357e12, 6.097e11, 8.680e10, 1.239e10, 1.770e9, 2.529e8, 3.613e7, 5.161e6, 7.373e5, 1.053e5,
    1.505e4, 2.150e3, 3.071e2, 4.387e1, 6.268e0, 8.954e-1,
];

/// Parse a list of H3 resolutions from a string (comma/whitespace separated, sorted, deduplicated, <= 15).
pub fn parse_resolutions_str(s: &str) -> Vec<u8> {
    let mut list: Vec<u8> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<u8>().ok())
        .filter(|&r| r <= 15)
        .collect();
    list.sort_unstable();
    list.dedup();
    list
}

/// Strict variant of [`parse_resolutions_str`]: `None` if any entry is not an integer in
/// `0..=15` or the list is empty, instead of silently dropping invalid entries.
pub fn try_parse_resolutions_str(s: &str) -> Option<Vec<u8>> {
    let mut list = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .map(|item| item.parse::<u8>().ok().filter(|&r| r <= 15))
        .collect::<Option<Vec<u8>>>()?;
    if list.is_empty() {
        return None;
    }
    list.sort_unstable();
    list.dedup();
    Some(list)
}

/// Parse bounding box coordinates from a comma/whitespace separated string `[min_lon, min_lat, max_lon, max_lat]`.
/// Returns `None` if any token is not a finite number or the count is not exactly four.
pub fn parse_bbox_str(s: &str) -> Option<[f64; 4]> {
    let coords = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .map(|item| item.parse::<f64>().ok().filter(|v| v.is_finite()))
        .collect::<Option<Vec<f64>>>()?;
    coords.try_into().ok()
}

/// Estimate query cardinality (number of H3 cells emitted) for a set of raster sources and resolutions.
pub fn estimate_raster_cardinality(resolved_paths: &[PathBuf], resolutions: &[u8]) -> u64 {
    if let Some(first_path) = resolved_paths.first() {
        if let Ok(reader) = GeoTiffStreamReader::open(first_path) {
            let w = reader.metadata.width as f64;
            let h = reader.metadata.height as f64;
            let total_pixels = (w * h) as u64 * resolved_paths.len() as u64;

            let (x0, y0) = reader.metadata.geotransform.pixel_to_coord(0.0, 0.0);
            let (x1, y1) = reader.metadata.geotransform.pixel_to_coord(w, h);
            let dx = (x1 - x0).abs();
            let dy = (y1 - y0).abs();

            let area_m2 =
                if matches!(reader.metadata.epsg, Some(4326)) || reader.metadata.epsg.is_none() {
                    dx * 111_320.0 * dy * 110_540.0 * resolved_paths.len() as f64
                } else {
                    dx * dy * resolved_paths.len() as f64
                };

            let mut total_hex_est = 0u64;
            for &res in resolutions {
                let hex_area = H3_AREA_M2.get(res as usize).copied().unwrap_or(7.373e5);
                let hex_count = (area_m2 / hex_area).ceil() as u64;
                total_hex_est = total_hex_est.saturating_add(hex_count.min(total_pixels).max(1));
            }
            total_hex_est.max(1)
        } else {
            10_000 * resolutions.len() as u64
        }
    } else {
        10_000 * resolutions.len() as u64
    }
}

/// Convert a 64-bit H3 cell index into its canonical lowercase hexadecimal string.
/// Returns `None` if `cell` is `0`.
#[inline]
pub fn h3_to_string(cell: u64) -> Option<String> {
    if cell == 0 {
        return None;
    }
    let mut buf = [0u8; 16];
    let hex_bytes = fast_hex_u64(cell, &mut buf);
    std::str::from_utf8(hex_bytes).ok().map(str::to_owned)
}

/// Parse a hexadecimal H3 string into a 64-bit unsigned integer.
/// Returns `0` on invalid input (matching DuckDB `string_to_h3` behavior) or `Some(u64)` via `try_string_to_h3`.
#[inline]
pub fn try_string_to_h3(hex: &str) -> Option<u64> {
    parse_hex_u64(hex)
}

/// Parse a hexadecimal H3 string into a 64-bit unsigned integer, returning `0` on failure.
#[inline]
pub fn string_to_h3(hex: &str) -> u64 {
    parse_hex_u64(hex).unwrap_or(0)
}

/// Extract the centroid latitude (in degrees, WGS84) of an H3 cell index.
/// Returns `f64::NAN` if `cell` is not a valid H3 index.
#[inline]
pub fn h3_to_lat(cell: u64) -> f64 {
    match CellIndex::try_from(cell) {
        Ok(idx) => LatLng::from(idx).lat(),
        Err(_) => f64::NAN,
    }
}

/// Extract the centroid longitude (in degrees, WGS84) of an H3 cell index.
/// Returns `f64::NAN` if `cell` is not a valid H3 index.
#[inline]
pub fn h3_to_lng(cell: u64) -> f64 {
    match CellIndex::try_from(cell) {
        Ok(idx) => LatLng::from(idx).lng(),
        Err(_) => f64::NAN,
    }
}

/// Extract the H3 resolution (`0..=15`) of a 64-bit H3 cell index, or `-1` if invalid.
#[inline]
pub fn h3_get_resolution(cell: u64) -> i64 {
    match CellIndex::try_from(cell) {
        Ok(idx) => u8::from(idx.resolution()) as i64,
        Err(_) => -1,
    }
}

/// Return `true` if `cell` is a valid 64-bit H3 cell index.
#[inline]
pub fn h3_is_valid_u64(cell: u64) -> bool {
    CellIndex::try_from(cell).is_ok()
}

/// Return `true` if `hex` is a valid hexadecimal H3 cell index string.
#[inline]
pub fn h3_is_valid_str(hex: &str) -> bool {
    match parse_hex_u64(hex) {
        Some(val) => CellIndex::try_from(val).is_ok(),
        None => false,
    }
}

/// Serialize a 64-bit H3 cell index to an OGC Well-Known Binary (WKB) 2D Polygon.
#[inline]
pub fn h3_to_wkb_u64(cell: u64) -> Option<Vec<u8>> {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let len = h3_index_to_wkb(cell, &mut buf)?;
    Some(buf[..len].to_vec())
}

/// Serialize a hexadecimal H3 cell index string to an OGC Well-Known Binary (WKB) 2D Polygon.
#[inline]
pub fn h3_to_wkb_str(hex: &str) -> Option<Vec<u8>> {
    let cell = parse_hex_u64(hex)?;
    h3_to_wkb_u64(cell)
}

/// Compute the parent H3 cell index at `parent_res` (`0..=15`) for a 64-bit H3 cell index.
#[inline]
pub fn h3_cell_to_parent_u64(cell: u64, parent_res: i64) -> Option<u64> {
    if !(0..=15).contains(&parent_res) {
        return None;
    }
    let idx = CellIndex::try_from(cell).ok()?;
    let target_res = Resolution::try_from(parent_res as u8).ok()?;
    idx.parent(target_res).map(u64::from)
}

/// Compute the parent H3 cell hex string at `parent_res` (`0..=15`) for a hexadecimal H3 cell string.
#[inline]
pub fn h3_cell_to_parent_str(hex: &str, parent_res: i64) -> Option<String> {
    let cell = parse_hex_u64(hex)?;
    let parent = h3_cell_to_parent_u64(cell, parent_res)?;
    h3_to_string(parent)
}

/// Upper bound on the number of cells a single `h3_polygon_to_cells` call may return
/// (80 MB of `u64`s). Checked against h3o's pre-tiling estimate, so a polygon far too large
/// for the requested resolution fails fast instead of exhausting memory.
pub const H3_POLYGON_TO_CELLS_MAX_CELLS: usize = 10_000_000;

/// Containment modes accepted by [`h3_polygon_to_cells_wkb`], as `(name, mode)` pairs.
///
/// The first group are h3o's names; `center`, `full` and `overlapping` are the H3 C library
/// (`polygonToCellsExperimental`) names for the same behaviour.
pub const H3_CONTAINMENT_MODES: [(&str, ContainmentMode); 7] = [
    ("contains_centroid", ContainmentMode::ContainsCentroid),
    ("contains_boundary", ContainmentMode::ContainsBoundary),
    ("intersects_boundary", ContainmentMode::IntersectsBoundary),
    ("covers", ContainmentMode::Covers),
    ("center", ContainmentMode::ContainsCentroid),
    ("full", ContainmentMode::ContainsBoundary),
    ("overlapping", ContainmentMode::Covers),
];

/// Parse a containment mode name (case-insensitive, `-` and `_` interchangeable).
/// See [`H3_CONTAINMENT_MODES`] for the accepted names.
pub fn parse_containment_mode(s: &str) -> Option<ContainmentMode> {
    let norm = s.trim().to_ascii_lowercase().replace('-', "_");
    H3_CONTAINMENT_MODES
        .iter()
        .find(|(name, _)| *name == norm)
        .map(|&(_, mode)| mode)
}

/// Why [`h3_polygon_to_cells_wkb`] produced no cell list.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolygonToCellsError {
    /// Resolution outside `0..=15`.
    #[error("resolution {0} is outside 0..=15")]
    InvalidResolution(i64),
    /// Input is not a valid WKB `POLYGON` / `MULTIPOLYGON` with finite coordinates.
    #[error("invalid polygon geometry: {0}")]
    InvalidGeometry(&'static str),
    /// h3o's estimate exceeds [`H3_POLYGON_TO_CELLS_MAX_CELLS`].
    #[error(
        "polygon would produce up to {estimate} cells, above the limit of {limit}; \
         use a coarser resolution or split the polygon"
    )]
    TooManyCells { estimate: usize, limit: usize },
}

/// Fill a WKB `POLYGON` / `MULTIPOLYGON` (WGS84 longitude/latitude degrees) with H3 cells
/// at `res` (`0..=15`) using h3o's polyfill and the given containment `mode`.
///
/// Returns the cells sorted ascending and deduplicated; an empty geometry yields an empty list.
pub fn h3_polygon_to_cells_wkb(
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
) -> Result<Vec<u64>, PolygonToCellsError> {
    let resolution = u8::try_from(res)
        .ok()
        .and_then(|r| Resolution::try_from(r).ok())
        .ok_or(PolygonToCellsError::InvalidResolution(res))?;
    let multi = wkb_to_multipolygon(wkb).map_err(PolygonToCellsError::InvalidGeometry)?;
    if multi.0.is_empty() {
        return Ok(Vec::new());
    }
    let shape = MultiPolygon::from_degrees(multi)
        .map_err(|_| PolygonToCellsError::InvalidGeometry("non-finite coordinates"))?;
    let config = PolyfillConfig::new(resolution).containment_mode(mode);
    let estimate = shape.max_cells_count(config);
    if estimate > H3_POLYGON_TO_CELLS_MAX_CELLS {
        return Err(PolygonToCellsError::TooManyCells {
            estimate,
            limit: H3_POLYGON_TO_CELLS_MAX_CELLS,
        });
    }
    let mut cells: Vec<u64> = shape.to_cells(config).map(u64::from).collect();
    // Overlapping members of a MULTIPOLYGON (and some modes) can emit a cell twice.
    cells.sort_unstable();
    cells.dedup();
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scalar_ops_roundtrip_and_validity() {
        let valid_u64 = 0x8828308281fffffu64;
        assert!(h3_is_valid_u64(valid_u64));
        assert!(!h3_is_valid_u64(0));

        let hex = h3_to_string(valid_u64).unwrap();
        assert_eq!(hex, "8828308281fffff");
        assert!(h3_is_valid_str(&hex));
        assert!(!h3_is_valid_str("not_hex"));
        assert_eq!(string_to_h3(&hex), valid_u64);
        assert_eq!(h3_get_resolution(valid_u64), 8);
        assert_eq!(h3_get_resolution(0), -1);

        let lat = h3_to_lat(valid_u64);
        let lng = h3_to_lng(valid_u64);
        assert!((lat - 37.77).abs() < 0.2);
        assert!((lng - (-122.42)).abs() < 0.2);
        assert!(h3_to_lat(0).is_nan());
        assert!(h3_to_lng(0).is_nan());

        let wkb = h3_to_wkb_u64(valid_u64).unwrap();
        assert!(wkb.len() >= 109);
        assert_eq!(h3_to_wkb_str(&hex).unwrap(), wkb);

        let parent = h3_cell_to_parent_u64(valid_u64, 5).unwrap();
        assert_eq!(h3_get_resolution(parent), 5);
        let parent_hex = h3_cell_to_parent_str(&hex, 5).unwrap();
        assert_eq!(string_to_h3(&parent_hex), parent);
        assert!(h3_cell_to_parent_u64(valid_u64, 12).is_none());
    }

    /// Little-endian WKB `POLYGON` with one ring of `(lng, lat)` points.
    fn polygon_wkb(ring: &[(f64, f64)]) -> Vec<u8> {
        let mut wkb = vec![1u8];
        wkb.extend(3u32.to_le_bytes());
        wkb.extend(1u32.to_le_bytes());
        wkb.extend((ring.len() as u32).to_le_bytes());
        for &(x, y) in ring {
            wkb.extend(x.to_le_bytes());
            wkb.extend(y.to_le_bytes());
        }
        wkb
    }

    fn rect_wkb(min_lng: f64, min_lat: f64, max_lng: f64, max_lat: f64) -> Vec<u8> {
        polygon_wkb(&[
            (min_lng, min_lat),
            (max_lng, min_lat),
            (max_lng, max_lat),
            (min_lng, max_lat),
            (min_lng, min_lat),
        ])
    }

    #[test]
    fn test_parse_containment_mode() {
        assert_eq!(
            parse_containment_mode("contains_centroid"),
            Some(ContainmentMode::ContainsCentroid)
        );
        assert_eq!(
            parse_containment_mode(" Contains-Boundary "),
            Some(ContainmentMode::ContainsBoundary)
        );
        assert_eq!(
            parse_containment_mode("INTERSECTS_BOUNDARY"),
            Some(ContainmentMode::IntersectsBoundary)
        );
        assert_eq!(
            parse_containment_mode("covers"),
            Some(ContainmentMode::Covers)
        );
        assert_eq!(
            parse_containment_mode("center"),
            Some(ContainmentMode::ContainsCentroid)
        );
        assert_eq!(
            parse_containment_mode("full"),
            Some(ContainmentMode::ContainsBoundary)
        );
        assert_eq!(
            parse_containment_mode("overlapping"),
            Some(ContainmentMode::Covers)
        );
        assert_eq!(parse_containment_mode("overlap_bbox"), None);
        assert_eq!(parse_containment_mode(""), None);
    }

    #[test]
    fn test_polygon_to_cells_containment_modes() {
        // ~0.1° box over San Francisco.
        let wkb = rect_wkb(-122.47, 37.73, -122.37, 37.81);
        let run = |mode| h3_polygon_to_cells_wkb(&wkb, 8, mode).unwrap();
        let centroid = run(ContainmentMode::ContainsCentroid);
        let boundary = run(ContainmentMode::ContainsBoundary);
        let intersects = run(ContainmentMode::IntersectsBoundary);
        let covers = run(ContainmentMode::Covers);

        assert!(!boundary.is_empty());
        assert!(boundary.len() < centroid.len() && centroid.len() < intersects.len());
        assert!(boundary.iter().all(|c| centroid.binary_search(c).is_ok()));
        assert!(centroid.iter().all(|c| intersects.binary_search(c).is_ok()));
        assert_eq!(intersects, covers);
        for cells in [&centroid, &boundary, &intersects] {
            assert!(cells.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
            assert!(cells.iter().all(|&c| h3_get_resolution(c) == 8));
        }

        // Centroid-mode cells have centroids inside the box.
        for &c in &centroid {
            let (lat, lng) = (h3_to_lat(c), h3_to_lng(c));
            assert!((37.73..=37.81).contains(&lat) && (-122.47..=-122.37).contains(&lng));
        }
    }

    #[test]
    fn test_polygon_to_cells_polygon_inside_one_cell() {
        // A ~10 m triangle well inside a single resolution-5 cell.
        let wkb = polygon_wkb(&[
            (-122.4194, 37.7749),
            (-122.4193, 37.7749),
            (-122.4194, 37.7750),
            (-122.4194, 37.7749),
        ]);
        let mode = |m| h3_polygon_to_cells_wkb(&wkb, 5, m).unwrap();
        assert!(mode(ContainmentMode::IntersectsBoundary).is_empty());
        assert!(mode(ContainmentMode::ContainsBoundary).is_empty());
        let covers = mode(ContainmentMode::Covers);
        assert_eq!(covers.len(), 1);
        let expected = u64::from(
            LatLng::new(37.7749, -122.4194)
                .unwrap()
                .to_cell(Resolution::Five),
        );
        assert_eq!(covers[0], expected);
    }

    #[test]
    fn test_polygon_to_cells_errors() {
        let wkb = rect_wkb(-122.47, 37.73, -122.37, 37.81);
        let mode = ContainmentMode::ContainsCentroid;
        assert_eq!(
            h3_polygon_to_cells_wkb(&wkb, 16, mode),
            Err(PolygonToCellsError::InvalidResolution(16))
        );
        assert_eq!(
            h3_polygon_to_cells_wkb(&wkb, -1, mode),
            Err(PolygonToCellsError::InvalidResolution(-1))
        );
        assert!(matches!(
            h3_polygon_to_cells_wkb(b"not wkb", 5, mode),
            Err(PolygonToCellsError::InvalidGeometry(_))
        ));
        assert!(matches!(
            h3_polygon_to_cells_wkb(&rect_wkb(0.0, 0.0, f64::NAN, 1.0), 5, mode),
            Err(PolygonToCellsError::InvalidGeometry(_))
        ));
        assert!(matches!(
            h3_polygon_to_cells_wkb(&rect_wkb(-10.0, -10.0, 10.0, 10.0), 15, mode),
            Err(PolygonToCellsError::TooManyCells { .. })
        ));

        // POLYGON EMPTY
        let mut empty = vec![1u8];
        empty.extend(3u32.to_le_bytes());
        empty.extend(0u32.to_le_bytes());
        assert_eq!(h3_polygon_to_cells_wkb(&empty, 5, mode), Ok(Vec::new()));
    }

    #[test]
    fn test_parse_bbox_str_rejects_malformed_tokens() {
        assert_eq!(parse_bbox_str("1, 2 3,4"), Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(parse_bbox_str("1,garbage,2,3,4"), None);
        assert_eq!(parse_bbox_str("1,2,3"), None);
        assert_eq!(parse_bbox_str("1,2,3,4,5"), None);
        assert_eq!(parse_bbox_str("1,2,3,inf"), None);
        assert_eq!(parse_bbox_str("1,2,NaN,4"), None);
    }
}

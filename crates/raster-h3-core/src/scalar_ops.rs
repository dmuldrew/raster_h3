//! Engine-neutral H3 scalar and utility operations shared across DuckDB, DataFusion,
//! Polars/Python, PostgreSQL, and CLI targets.

use std::path::PathBuf;

pub use h3o::geom::ContainmentMode;
use h3o::geom::TilerBuilder;
use h3o::{CellIndex, LatLng, Resolution};

use crate::encoding::{
    a5_index_to_wkb, fast_hex_u64, h3_index_to_wkb, parse_hex_u64, wkb_to_multipolygon, WkbBuf,
    WKB_BUF_LEN,
};
use crate::grid::{a5_is_valid_cell, GridKind};
use crate::raster::geotiff::GeoTiffStreamReader;

/// Approximate H3 cell areas in m^2 by resolution (0 to 15) for query planner cardinality estimation.
pub const H3_AREA_M2: [f64; 16] = [
    4.357e12, 6.097e11, 8.680e10, 1.239e10, 1.770e9, 2.529e8, 3.613e7, 5.161e6, 7.373e5, 1.053e5,
    1.505e4, 2.150e3, 3.071e2, 4.387e1, 6.268e0, 8.954e-1,
];

/// Parse a list of H3 resolutions from a string (comma/whitespace separated, sorted, deduplicated, <= 15).
pub fn parse_resolutions_str(s: &str) -> Vec<u8> {
    parse_resolutions_str_for(GridKind::H3, s)
}

/// Parse a list of resolutions for `grid` from a string (comma/whitespace separated, sorted, deduplicated).
pub fn parse_resolutions_str_for(grid: GridKind, s: &str) -> Vec<u8> {
    let max_res = grid.max_resolution();
    let mut list: Vec<u8> = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .filter_map(|item| item.parse::<u8>().ok())
        .filter(|&r| r <= max_res)
        .collect();
    list.sort_unstable();
    list.dedup();
    list
}

/// Strict variant of [`parse_resolutions_str`]: `None` if any entry is not an integer in
/// `0..=15` or the list is empty, instead of silently dropping invalid entries.
pub fn try_parse_resolutions_str(s: &str) -> Option<Vec<u8>> {
    try_parse_resolutions_str_for(GridKind::H3, s)
}

/// Strict variant of [`parse_resolutions_str_for`] for `grid`.
pub fn try_parse_resolutions_str_for(grid: GridKind, s: &str) -> Option<Vec<u8>> {
    let max_res = grid.max_resolution();
    let mut list = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|item| !item.is_empty())
        .map(|item| item.parse::<u8>().ok().filter(|&r| r <= max_res))
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
    estimate_raster_cardinality_for(GridKind::H3, resolved_paths, resolutions)
}

/// Estimate query cardinality (number of cells emitted) for a set of raster sources, `grid`, and resolutions.
pub fn estimate_raster_cardinality_for(
    grid: GridKind,
    resolved_paths: &[PathBuf],
    resolutions: &[u8],
) -> u64 {
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
                let cell_area = grid.area_m2(res);
                let hex_count = (area_m2 / cell_area).ceil() as u64;
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
    /// Resolution outside the grid's range (`0..=15` for H3).
    #[error("resolution {0} is outside the grid's range")]
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
    let mut cells = Vec::new();
    h3_polygon_to_cells_wkb_into(wkb, res, mode, &mut cells)?;
    Ok(cells)
}

/// [`h3_polygon_to_cells_wkb`] writing into a caller-supplied buffer, so a
/// caller handling many rows can reuse one allocation. `out` is cleared first;
/// on error its contents are unspecified.
pub fn h3_polygon_to_cells_wkb_into(
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
    out: &mut Vec<u64>,
) -> Result<(), PolygonToCellsError> {
    out.clear();
    let resolution = u8::try_from(res)
        .ok()
        .and_then(|r| Resolution::try_from(r).ok())
        .ok_or(PolygonToCellsError::InvalidResolution(res))?;
    let multi = wkb_to_multipolygon(wkb).map_err(PolygonToCellsError::InvalidGeometry)?;
    if multi.0.is_empty() {
        return Ok(());
    }
    let mut tiler = TilerBuilder::new(resolution).containment_mode(mode).build();
    tiler
        .add_batch(multi)
        .map_err(|_| PolygonToCellsError::InvalidGeometry("non-finite coordinates"))?;
    let estimate = tiler.coverage_size_hint();
    if estimate > H3_POLYGON_TO_CELLS_MAX_CELLS {
        return Err(PolygonToCellsError::TooManyCells {
            estimate,
            limit: H3_POLYGON_TO_CELLS_MAX_CELLS,
        });
    }
    out.extend(tiler.into_coverage().map(u64::from));
    // Overlapping members of a MULTIPOLYGON (and some modes) can emit a cell twice.
    out.sort_unstable();
    out.dedup();
    Ok(())
}

/// Index a WGS84 `(lon, lat)` coordinate (in degrees) to a cell of `grid` at `res`.
/// Longitudes outside `[-180, 180]` wrap.
#[inline]
pub fn grid_lonlat_to_cell(grid: GridKind, lon: f64, lat: f64, res: i64) -> Option<u64> {
    let res = u8::try_from(res)
        .ok()
        .filter(|&r| r <= grid.max_resolution())?;
    grid.cell_at(lat, crate::grid::wrap_lon_180(lon), res)
}

/// Centroid latitude (degrees, WGS84) of a cell of `grid`, `f64::NAN` if invalid.
#[inline]
pub fn grid_to_lat(grid: GridKind, cell: u64) -> f64 {
    grid.center_lat_lng(cell).map_or(f64::NAN, |(lat, _)| lat)
}

/// Centroid longitude (degrees, WGS84) of a cell of `grid`, `f64::NAN` if invalid.
#[inline]
pub fn grid_to_lng(grid: GridKind, cell: u64) -> f64 {
    grid.center_lat_lng(cell).map_or(f64::NAN, |(_, lng)| lng)
}

/// Resolution of a cell of `grid`, or `-1` if invalid.
#[inline]
pub fn grid_get_resolution(grid: GridKind, cell: u64) -> i64 {
    grid.resolution(cell).map_or(-1, i64::from)
}

/// Whether `hex` is a valid hexadecimal cell index of `grid`.
#[inline]
pub fn grid_is_valid_str(grid: GridKind, hex: &str) -> bool {
    parse_hex_u64(hex).is_some_and(|cell| grid.is_valid(cell))
}

/// Serialize a cell of `grid` to an OGC Well-Known Binary (WKB) 2D polygon.
#[inline]
pub fn grid_to_wkb_u64(grid: GridKind, cell: u64) -> Option<Vec<u8>> {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let len = crate::encoding::cell_index_to_wkb(grid, cell, &mut buf)?;
    Some(buf[..len].to_vec())
}

/// Ancestor of a cell of `grid` at `parent_res`, `None` if invalid or finer than `cell`.
#[inline]
pub fn grid_cell_to_parent_u64(grid: GridKind, cell: u64, parent_res: i64) -> Option<u64> {
    let parent_res = u8::try_from(parent_res).ok()?;
    grid.parent(cell, parent_res)
}

/// Fill a WKB `POLYGON` / `MULTIPOLYGON` (WGS84 longitude/latitude degrees) with cells
/// of `grid` at `res`, writing them sorted and deduplicated into `out`.
///
/// S2 and HEALPix cells are tested against the polygon in lon/lat degrees, the model
/// their geometry is published in; a request whose bounding box holds more than
/// [`H3_POLYGON_TO_CELLS_MAX_CELLS`] cells is rejected before generation.
pub fn grid_polygon_to_cells_wkb_into(
    grid: GridKind,
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
    out: &mut Vec<u64>,
) -> Result<(), PolygonToCellsError> {
    match grid {
        GridKind::H3 => h3_polygon_to_cells_wkb_into(wkb, res, mode, out),
        GridKind::A5 => a5_polygon_to_cells_wkb_into(wkb, res, mode, out),
        GridKind::S2 | GridKind::Healpix => {
            out.clear();
            let level = u8::try_from(res)
                .ok()
                .filter(|&r| r <= grid.max_resolution())
                .ok_or(PolygonToCellsError::InvalidResolution(res))?;
            let multi = wkb_to_multipolygon(wkb).map_err(PolygonToCellsError::InvalidGeometry)?;
            let result = crate::polygon_fill::fill_nested_grid(grid, &multi, level, mode, out);
            if result.is_err() {
                out.clear();
            }
            result
        }
    }
}

/// Convert a 64-bit A5 cell index into its canonical lowercase hexadecimal string.
/// Returns `None` if `cell` is `0`.
#[inline]
pub fn a5_to_string(cell: u64) -> Option<String> {
    h3_to_string(cell)
}

/// Parse a hexadecimal A5 string into a 64-bit unsigned integer.
#[inline]
pub fn try_string_to_a5(hex: &str) -> Option<u64> {
    parse_hex_u64(hex)
}

/// Parse a hexadecimal A5 string into a 64-bit unsigned integer, returning `0` on failure.
#[inline]
pub fn string_to_a5(hex: &str) -> u64 {
    parse_hex_u64(hex).unwrap_or(0)
}

/// Index a WGS84 `(lon, lat)` coordinate (in degrees) to an A5 cell at `res` (`0..=30`).
#[inline]
pub fn a5_lonlat_to_cell(lon: f64, lat: f64, res: i64) -> Option<u64> {
    if !lon.is_finite() || !(-90.0..=90.0).contains(&lat) || !(0..=30).contains(&res) {
        return None;
    }
    a5::lonlat_to_cell(
        a5::LonLat::new(crate::grid::wrap_lon_180(lon), lat),
        res as i32,
    )
    .ok()
}

/// Extract the centroid latitude (in degrees, WGS84) of an A5 cell index.
/// Returns `f64::NAN` if `cell` is not a valid A5 index.
#[inline]
pub fn a5_to_lat(cell: u64) -> f64 {
    GridKind::A5
        .center_lat_lng(cell)
        .map_or(f64::NAN, |(lat, _)| lat)
}

/// Extract the centroid longitude (in degrees, WGS84) of an A5 cell index.
/// Returns `f64::NAN` if `cell` is not a valid A5 index.
#[inline]
pub fn a5_to_lng(cell: u64) -> f64 {
    GridKind::A5
        .center_lat_lng(cell)
        .map_or(f64::NAN, |(_, lng)| lng)
}

/// Extract the A5 resolution (`0..=30`) of a 64-bit A5 cell index, or `-1` if invalid.
#[inline]
pub fn a5_get_resolution(cell: u64) -> i64 {
    GridKind::A5.resolution(cell).map_or(-1, i64::from)
}

/// Return `true` if `cell` is a valid 64-bit A5 cell index.
#[inline]
pub fn a5_is_valid_u64(cell: u64) -> bool {
    a5_is_valid_cell(cell)
}

/// Return `true` if `hex` is a valid hexadecimal A5 cell index string.
#[inline]
pub fn a5_is_valid_str(hex: &str) -> bool {
    parse_hex_u64(hex).is_some_and(a5_is_valid_cell)
}

/// Serialize a 64-bit A5 cell index to an OGC Well-Known Binary (WKB) 2D Polygon.
#[inline]
pub fn a5_to_wkb_u64(cell: u64) -> Option<Vec<u8>> {
    let mut buf: WkbBuf = [0u8; WKB_BUF_LEN];
    let len = a5_index_to_wkb(cell, &mut buf)?;
    Some(buf[..len].to_vec())
}

/// Serialize a hexadecimal A5 cell index string to an OGC Well-Known Binary (WKB) 2D Polygon.
#[inline]
pub fn a5_to_wkb_str(hex: &str) -> Option<Vec<u8>> {
    let cell = parse_hex_u64(hex)?;
    a5_to_wkb_u64(cell)
}

/// Compute the parent A5 cell index at `parent_res` (`0..=30`) for a 64-bit A5 cell index.
#[inline]
pub fn a5_cell_to_parent_u64(cell: u64, parent_res: i64) -> Option<u64> {
    if !(0..=30).contains(&parent_res) {
        return None;
    }
    GridKind::A5.parent(cell, parent_res as u8)
}

/// Compute the parent A5 cell hex string at `parent_res` (`0..=30`) for a hexadecimal A5 cell string.
#[inline]
pub fn a5_cell_to_parent_str(hex: &str, parent_res: i64) -> Option<String> {
    let cell = parse_hex_u64(hex)?;
    let parent = a5_cell_to_parent_u64(cell, parent_res)?;
    a5_to_string(parent)
}

/// Fill a WKB `POLYGON` / `MULTIPOLYGON` (WGS84 longitude/latitude degrees) with A5 cells
/// at `res` (`0..=30`) using `a5::polygon_to_cells` and the given containment `mode`.
///
/// Returns the cells sorted ascending and deduplicated; an empty geometry yields an empty list.
/// A conservative bounding-cap and boundary-sampling budget rejects oversized
/// requests before generation. Thin/concave polygons may need to be split even
/// when their final cell count would be below the limit.
pub fn a5_polygon_to_cells_wkb(
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
) -> Result<Vec<u64>, PolygonToCellsError> {
    let mut cells = Vec::new();
    a5_polygon_to_cells_wkb_into(wkb, res, mode, &mut cells)?;
    Ok(cells)
}

/// [`a5_polygon_to_cells_wkb`] writing into a caller-supplied buffer.
pub fn a5_polygon_to_cells_wkb_into(
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
    out: &mut Vec<u64>,
) -> Result<(), PolygonToCellsError> {
    out.clear();
    let result = a5_fill_polygons(wkb, res, mode, out);
    if result.is_err() {
        // Earlier polygons of a multipolygon may already be in `out`.
        out.clear();
    }
    result
}

fn a5_fill_polygons(
    wkb: &[u8],
    res: i64,
    mode: ContainmentMode,
    out: &mut Vec<u64>,
) -> Result<(), PolygonToCellsError> {
    if !(0..=30).contains(&res) {
        return Err(PolygonToCellsError::InvalidResolution(res));
    }
    let multi = wkb_to_multipolygon(wkb).map_err(PolygonToCellsError::InvalidGeometry)?;
    if multi.0.is_empty() {
        return Ok(());
    }
    let containment = match mode {
        ContainmentMode::IntersectsBoundary | ContainmentMode::Covers => {
            a5::Containment::Overlapping
        }
        _ => a5::Containment::Center,
    };
    let opts = a5::PolygonToCellsOptions { containment };
    for poly in &multi.0 {
        let mut rings = Vec::with_capacity(1 + poly.interiors().len());
        let ext: Vec<a5::LonLat> = poly
            .exterior()
            .coords()
            .map(|c| {
                if !c.x.is_finite() || !c.y.is_finite() {
                    Err(PolygonToCellsError::InvalidGeometry(
                        "non-finite coordinates",
                    ))
                } else {
                    Ok(a5::LonLat::new(c.x, c.y))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if ext.len() < 3 {
            continue;
        }
        rings.push(ext);
        for interior in poly.interiors() {
            let hole: Vec<a5::LonLat> = interior
                .coords()
                .map(|c| {
                    if !c.x.is_finite() || !c.y.is_finite() {
                        Err(PolygonToCellsError::InvalidGeometry(
                            "non-finite coordinates",
                        ))
                    } else {
                        Ok(a5::LonLat::new(c.x, c.y))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            rings.push(hole);
        }
        // A5 materializes boundary samples before filling, and uncompact allocates
        // all descendants at once. Budget both stages before entering either API.
        let prepared = prepare_a5_polygon(&rings);
        check_a5_fill_budget(&prepared, res as i32, out.len())?;
        let compacted = a5::polygon_to_cells(&rings, res as i32, Some(opts))
            .map_err(|_| PolygonToCellsError::InvalidGeometry("invalid polygon"))?;
        check_a5_expansion_budget(&compacted, res as i32, out.len())?;
        let mut cells = a5::uncompact(&compacted, res as i32)
            .map_err(|_| PolygonToCellsError::InvalidGeometry("uncompact failed"))?;
        if matches!(mode, ContainmentMode::ContainsBoundary) {
            cells.retain(|&cell| a5_boundary_contained(cell, &prepared));
        }

        if out.len().saturating_add(cells.len()) > H3_POLYGON_TO_CELLS_MAX_CELLS {
            return Err(PolygonToCellsError::TooManyCells {
                estimate: out.len().saturating_add(cells.len()),
                limit: H3_POLYGON_TO_CELLS_MAX_CELLS,
            });
        }
        out.extend(cells);
    }
    out.sort_unstable();
    out.dedup();
    Ok(())
}

fn prepare_a5_polygon(
    rings: &[Vec<a5::LonLat>],
) -> a5::geometry::prepared_polygon::PreparedPolygon {
    use a5::core::coordinate_transforms::{from_lon_lat, to_cartesian};
    a5::geometry::prepared_polygon::prepare_polygon(
        rings
            .iter()
            .map(|ring| {
                let ring = if ring.len() > 1 && ring.first() == ring.last() {
                    &ring[..ring.len() - 1]
                } else {
                    ring.as_slice()
                };
                ring.iter()
                    .map(|&p| to_cartesian(from_lon_lat(p)))
                    .collect()
            })
            .collect(),
    )
}

fn check_a5_cell_limit(estimate: usize) -> Result<(), PolygonToCellsError> {
    if estimate > H3_POLYGON_TO_CELLS_MAX_CELLS {
        Err(PolygonToCellsError::TooManyCells {
            estimate,
            limit: H3_POLYGON_TO_CELLS_MAX_CELLS,
        })
    } else {
        Ok(())
    }
}

fn check_a5_expansion_budget(
    cells: &[u64],
    res: i32,
    existing: usize,
) -> Result<(), PolygonToCellsError> {
    let mut count = existing;
    for &cell in cells {
        let source = a5::get_resolution(cell);
        let descendants = a5::core::cell_info::get_num_cells(res)
            / a5::core::cell_info::get_num_cells(source).max(1);
        count = count.saturating_add(usize::try_from(descendants).unwrap_or(usize::MAX));
        check_a5_cell_limit(count)?;
    }
    Ok(())
}

/// Conservative work estimate, not an exact output count. A bounding cap also
/// bounds cells inside concave polygons and holes (which A5 fills internally).
/// Include dense boundary sampling separately: a thin or repeatedly traced ring
/// can have few output cells but still allocate an enormous sample vector.
fn check_a5_fill_budget(
    polygon: &a5::geometry::prepared_polygon::PreparedPolygon,
    res: i32,
    existing: usize,
) -> Result<(), PolygonToCellsError> {
    use a5::core::constants::AUTHALIC_RADIUS_EARTH as R;
    use a5::utils::vector::angle;
    let radius = a5::traversal::cap::estimate_cell_radius(res);
    // Include the full extent of every cell touching the cap, not only centers.
    let cap = (polygon.cap.angle + 2.0 * radius / R).min(std::f64::consts::PI);
    let area = 4.0 * std::f64::consts::PI * R * R * (cap / 2.0).sin().powi(2);
    let cells = (area / a5::cell_area(res)).ceil() as usize;
    let samples = polygon.ring_vecs_list.iter().fold(0usize, |total, ring| {
        ring.iter()
            .zip(ring.iter().cycle().skip(1))
            .take(ring.len())
            .fold(total, |n, (&a, &b)| {
                n.saturating_add((angle(a, b) * R / (radius * 0.4)).ceil() as usize)
                    .saturating_add(2)
            })
    });
    check_a5_cell_limit(existing.saturating_add(cells).saturating_add(samples))
}

fn a5_boundary_contained(
    cell: u64,
    polygon: &a5::geometry::prepared_polygon::PreparedPolygon,
) -> bool {
    use a5::core::coordinate_transforms::{from_lon_lat, to_cartesian};
    use a5::geometry::prepared_polygon::point_in_prepared_polygon;
    let Ok(boundary) = a5::cell_to_boundary(
        cell,
        Some(a5::CellToBoundaryOptions {
            closed_ring: false,
            segments: Some(1),
        }),
    ) else {
        return false;
    };
    let vertices: Vec<_> = boundary
        .into_iter()
        .map(|p| to_cartesian(from_lon_lat(p)))
        .collect();
    if vertices.len() < 3
        || !vertices
            .iter()
            .all(|&p| point_in_prepared_polygon(p, polygon))
    {
        return false;
    }
    // Vertex containment alone misses holes and concave notches crossing edges.
    for ring in &polygon.ring_vecs_list {
        for (&a, &b) in vertices
            .iter()
            .zip(vertices.iter().cycle().skip(1))
            .take(vertices.len())
        {
            for (&c, &d) in ring
                .iter()
                .zip(ring.iter().cycle().skip(1))
                .take(ring.len())
            {
                if spherical_arcs_cross(a, b, c, d) {
                    return false;
                }
            }
        }
    }
    // A hole entirely enclosed by the cell also prevents full containment.
    let cell_polygon = a5::geometry::prepared_polygon::prepare_polygon(vec![vertices]);
    !polygon.ring_vecs_list.iter().skip(1).any(|hole| {
        hole.first()
            .is_some_and(|&p| point_in_prepared_polygon(p, &cell_polygon))
    })
}

fn spherical_arcs_cross(
    a: a5::coordinate_systems::Cartesian,
    b: a5::coordinate_systems::Cartesian,
    c: a5::coordinate_systems::Cartesian,
    d: a5::coordinate_systems::Cartesian,
) -> bool {
    use a5::coordinate_systems::Cartesian;
    use a5::utils::vector::angle;
    fn cross(a: Cartesian, b: Cartesian) -> Cartesian {
        Cartesian::new(
            a.y() * b.z() - a.z() * b.y(),
            a.z() * b.x() - a.x() * b.z(),
            a.x() * b.y() - a.y() * b.x(),
        )
    }
    let intersection = cross(cross(a, b), cross(c, d));
    let norm = a5::utils::vector::length(intersection);
    if norm == 0.0 {
        return false;
    }
    for sign in [-1.0, 1.0] {
        let p = Cartesian::new(
            sign * intersection.x() / norm,
            sign * intersection.y() / norm,
            sign * intersection.z() / norm,
        );
        if angle(a, p) + angle(p, b) <= angle(a, b) + 1e-12
            && angle(c, p) + angle(p, d) <= angle(c, d) + 1e-12
        {
            return true;
        }
    }
    false
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
        assert!(mode(ContainmentMode::ContainsBoundary).is_empty());
        let expected = vec![u64::from(
            LatLng::new(37.7749, -122.4194)
                .unwrap()
                .to_cell(Resolution::Five),
        )];
        // h3o >= 0.11 tests intersection against the whole cell, so the enclosing cell matches.
        assert_eq!(mode(ContainmentMode::IntersectsBoundary), expected);
        assert_eq!(mode(ContainmentMode::Covers), expected);
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

    #[test]
    fn test_a5_scalar_ops_and_polygon_to_cells() {
        let cell = a5_lonlat_to_cell(-122.4194, 37.7749, 8).expect("valid A5 cell");
        assert!(a5_is_valid_u64(cell));
        assert!(!a5_is_valid_u64(0));
        assert_eq!(a5_get_resolution(cell), 8);

        let hex = a5_to_string(cell).unwrap();
        assert!(a5_is_valid_str(&hex));
        assert_eq!(string_to_a5(&hex), cell);

        let lat = a5_to_lat(cell);
        let lng = a5_to_lng(cell);
        assert!((lat - 37.77).abs() < 1.0);
        assert!((lng - (-122.42)).abs() < 1.0);

        let wkb = a5_to_wkb_u64(cell).unwrap();
        assert_eq!(wkb.len(), 109);
        assert_eq!(a5_to_wkb_str(&hex).unwrap(), wkb);

        // Longitudes outside [-180, 180] wrap rather than clamp.
        assert_eq!(
            a5_lonlat_to_cell(190.0, 10.0, 8),
            a5_lonlat_to_cell(-170.0, 10.0, 8)
        );
        assert_ne!(
            a5_lonlat_to_cell(190.0, 10.0, 8),
            a5_lonlat_to_cell(180.0, 10.0, 8)
        );

        let parent = a5_cell_to_parent_u64(cell, 5).unwrap();
        assert_eq!(a5_get_resolution(parent), 5);
        assert!(a5_cell_to_parent_u64(cell, 10).is_none());

        let box_wkb = rect_wkb(-122.5, 37.7, -122.3, 37.85);
        let centroid = a5_polygon_to_cells_wkb(&box_wkb, 10, ContainmentMode::ContainsCentroid)
            .expect("a5 polyfill centroid");
        let boundary = a5_polygon_to_cells_wkb(&box_wkb, 10, ContainmentMode::ContainsBoundary)
            .expect("a5 polyfill boundary");
        let covers = a5_polygon_to_cells_wkb(&box_wkb, 10, ContainmentMode::Covers)
            .expect("a5 polyfill covers");
        assert!(!boundary.is_empty());
        assert!(boundary.len() <= centroid.len());
        assert!(centroid.len() <= covers.len());
        assert!(centroid.iter().all(|&c| a5_get_resolution(c) == 10));
    }
    fn polygon_with_holes_wkb(rings: &[Vec<(f64, f64)>]) -> Vec<u8> {
        let mut bytes = vec![1];
        bytes.extend(3u32.to_le_bytes());
        bytes.extend((rings.len() as u32).to_le_bytes());
        for ring in rings {
            bytes.extend((ring.len() as u32).to_le_bytes());
            for &(x, y) in ring {
                bytes.extend(x.to_le_bytes());
                bytes.extend(y.to_le_bytes());
            }
        }
        bytes
    }

    fn square(x: f64, y: f64, d: f64) -> Vec<(f64, f64)> {
        vec![
            (x - d, y - d),
            (x + d, y - d),
            (x + d, y + d),
            (x - d, y + d),
            (x - d, y - d),
        ]
    }

    #[test]
    fn a5_oversized_fill_is_rejected_before_generation() {
        for mode in [
            ContainmentMode::ContainsCentroid,
            ContainmentMode::ContainsBoundary,
            ContainmentMode::Covers,
        ] {
            let mut out = vec![123];
            let error =
                a5_polygon_to_cells_wkb_into(&rect_wkb(-123., 37., -122., 38.), 30, mode, &mut out)
                    .unwrap_err();
            assert!(matches!(error, PolygonToCellsError::TooManyCells { .. }));
            assert!(out.is_empty());
        }
    }

    #[test]
    fn a5_expansion_budget_checks_descendants_without_allocating() {
        let root = GridKind::A5.cell_at(0., 0., 0).unwrap();
        assert!(check_a5_expansion_budget(&[root], 1, H3_POLYGON_TO_CELLS_MAX_CELLS - 5).is_ok());
        assert!(check_a5_expansion_budget(&[root], 1, H3_POLYGON_TO_CELLS_MAX_CELLS - 4).is_err());
        assert!(matches!(
            check_a5_expansion_budget(&[root], 30, 0),
            Err(PolygonToCellsError::TooManyCells { .. })
        ));
    }

    #[test]
    fn a5_boundary_sample_budget_includes_holes_and_repeated_edges() {
        // Small area, huge repeated boundary: budget work even if output is tiny.
        let endpoints = [a5::LonLat::new(0., 0.), a5::LonLat::new(0.01, 0.)];
        let small = prepare_a5_polygon(&[endpoints.to_vec()]);
        assert!(check_a5_fill_budget(&small, 20, 0).is_ok());
        let ring: Vec<_> = (0..20000).flat_map(|_| endpoints).collect();
        let prep = prepare_a5_polygon(&[ring]);
        assert!(matches!(
            check_a5_fill_budget(&prep, 20, 0),
            Err(PolygonToCellsError::TooManyCells { .. })
        ));
    }

    #[test]
    fn a5_full_containment_rejects_crossing_and_enclosed_holes() {
        let cell = GridKind::A5.cell_at(37.7749, -122.4194, 8).unwrap();
        let (lat, lon) = GridKind::A5.center_lat_lng(cell).unwrap();
        let mut vertices = [(0., 0.); 10];
        GridKind::A5.boundary_lng_lat(cell, &mut vertices);
        let outer = square(lon, lat, 2.);
        assert!(a5_polygon_to_cells_wkb(
            &polygon_with_holes_wkb(std::slice::from_ref(&outer)),
            8,
            ContainmentMode::ContainsBoundary
        )
        .unwrap()
        .contains(&cell));
        for (hx, hy) in [
            (
                (vertices[0].0 + vertices[1].0) / 2.,
                (vertices[0].1 + vertices[1].1) / 2.,
            ),
            (
                lon + 0.2 * (vertices[0].0 - lon),
                lat + 0.2 * (vertices[0].1 - lat),
            ),
        ] {
            let wkb = polygon_with_holes_wkb(&[outer.clone(), square(hx, hy, 0.003)]);
            assert!(
                a5_polygon_to_cells_wkb(&wkb, 8, ContainmentMode::ContainsCentroid)
                    .unwrap()
                    .contains(&cell)
            );
            assert!(
                !a5_polygon_to_cells_wkb(&wkb, 8, ContainmentMode::ContainsBoundary)
                    .unwrap()
                    .contains(&cell)
            );
        }
    }
    #[test]
    fn a5_full_containment_rejects_concave_notch_between_vertices() {
        let cell = GridKind::A5.cell_at(37.7749, -122.4194, 8).unwrap();
        let (lat, lon) = GridKind::A5.center_lat_lng(cell).unwrap();
        let mut vertices = [(0., 0.); 10];
        let n = GridKind::A5.boundary_lng_lat(cell, &mut vertices);
        let (x, y) = (0..n)
            .map(|i| {
                let a = vertices[i];
                let b = vertices[(i + 1) % n];
                ((a.0 + b.0) / 2., (a.1 + b.1) / 2.)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap();
        let ring = vec![
            (lon - 2., lat - 2.),
            (lon + 2., lat - 2.),
            (lon + 2., y - 0.003),
            (x - 0.01, y - 0.003),
            (x - 0.01, y + 0.003),
            (lon + 2., y + 0.003),
            (lon + 2., lat + 2.),
            (lon - 2., lat + 2.),
            (lon - 2., lat - 2.),
        ];
        let wkb = polygon_with_holes_wkb(&[ring]);
        assert!(
            a5_polygon_to_cells_wkb(&wkb, 8, ContainmentMode::ContainsCentroid)
                .unwrap()
                .contains(&cell)
        );
        assert!(
            !a5_polygon_to_cells_wkb(&wkb, 8, ContainmentMode::ContainsBoundary)
                .unwrap()
                .contains(&cell)
        );
    }

    #[test]
    fn a5_full_containment_handles_antimeridian_rings() {
        let cell = GridKind::A5.cell_at(0., 179.99, 8).unwrap();
        let (lat, lon) = GridKind::A5.center_lat_lng(cell).unwrap();
        let wkb = polygon_with_holes_wkb(&[square(lon, lat, 1.)]);
        assert!(
            a5_polygon_to_cells_wkb(&wkb, 8, ContainmentMode::ContainsBoundary)
                .unwrap()
                .contains(&cell)
        );
    }

    const NESTED: [GridKind; 2] = [GridKind::S2, GridKind::Healpix];

    fn fill(grid: GridKind, wkb: &[u8], res: i64, mode: ContainmentMode) -> Vec<u64> {
        let mut out = vec![123];
        grid_polygon_to_cells_wkb_into(grid, wkb, res, mode, &mut out).unwrap();
        out
    }

    #[test]
    fn nested_grid_scalar_ops_round_trip() {
        for grid in NESTED {
            let cell = grid_lonlat_to_cell(grid, -122.4194, 37.7749, 12).unwrap();
            assert_eq!(grid_get_resolution(grid, cell), 12);
            assert!((grid_to_lat(grid, cell) - 37.7749).abs() < 0.1);
            assert!((grid_to_lng(grid, cell) + 122.4194).abs() < 0.1);
            let hex = h3_to_string(cell).unwrap();
            assert!(grid_is_valid_str(grid, &hex));
            assert!(!grid_is_valid_str(grid, "0"));
            assert_eq!(grid_to_wkb_u64(grid, cell).unwrap().len(), 93);
            let parent = grid_cell_to_parent_u64(grid, cell, 5).unwrap();
            assert_eq!(grid_get_resolution(grid, parent), 5);
            assert_eq!(grid_cell_to_parent_u64(grid, cell, 13), None);
            assert_eq!(grid_cell_to_parent_u64(grid, cell, -1), None);
            // Out-of-range longitudes wrap; out-of-range resolutions and latitudes fail.
            assert_eq!(
                grid_lonlat_to_cell(grid, 190.0, 10.0, 8),
                grid_lonlat_to_cell(grid, -170.0, 10.0, 8)
            );
            assert_eq!(grid_lonlat_to_cell(grid, 0.0, 0.0, 31), None);
            assert_eq!(grid_lonlat_to_cell(grid, 0.0, 91.0, 3), None);
            assert_eq!(grid_get_resolution(grid, 0), -1);
            assert!(grid_to_lat(grid, 0).is_nan());
            assert_eq!(grid_to_wkb_u64(grid, 0), None);
        }
    }

    #[test]
    fn nested_grid_polygon_fill_matches_a_scan_of_the_box() {
        let (w, s, e, n) = (-122.5, 37.7, -122.3, 37.85);
        let wkb = rect_wkb(w, s, e, n);
        for grid in NESTED {
            let res = 11u8;
            let centroid = fill(
                grid,
                &wkb,
                i64::from(res),
                ContainmentMode::ContainsCentroid,
            );
            let boundary = fill(
                grid,
                &wkb,
                i64::from(res),
                ContainmentMode::ContainsBoundary,
            );
            let covers = fill(grid, &wkb, i64::from(res), ContainmentMode::Covers);
            let intersects = fill(
                grid,
                &wkb,
                i64::from(res),
                ContainmentMode::IntersectsBoundary,
            );
            assert!(!boundary.is_empty(), "{grid}");
            assert!(boundary.iter().all(|c| centroid.contains(c)));
            assert!(centroid.iter().all(|c| covers.contains(c)));
            assert!(boundary.len() < centroid.len() && centroid.len() < covers.len());
            assert_eq!(covers, intersects);
            assert!(centroid.windows(2).all(|pair| pair[0] < pair[1]));

            // Every cell a dense scan of a wider box finds, classified by its centre.
            let mut scanned = std::collections::BTreeSet::new();
            let step = 0.0005;
            let mut lat = s - 0.1;
            while lat < n + 0.1 {
                let mut lon = w - 0.1;
                while lon < e + 0.1 {
                    scanned.insert(grid.cell_at(lat, lon, res).unwrap());
                    lon += step;
                }
                lat += step;
            }
            let expected: Vec<u64> = scanned
                .into_iter()
                .filter(|&cell| {
                    let (lat, lon) = grid.center_lat_lng(cell).unwrap();
                    (w..e).contains(&lon) && (s..n).contains(&lat)
                })
                .collect();
            assert_eq!(centroid, expected, "{grid}");
        }
    }

    #[test]
    fn nested_grid_polygon_fill_handles_holes_tiny_polygons_and_poles() {
        for grid in NESTED {
            let cell = grid.cell_at(37.7749, -122.4194, 9).unwrap();
            let (lat, lon) = grid.center_lat_lng(cell).unwrap();
            let outer = square(lon, lat, 1.0);
            let solid = polygon_with_holes_wkb(std::slice::from_ref(&outer));
            assert!(fill(grid, &solid, 9, ContainmentMode::ContainsBoundary).contains(&cell));
            // A hole inside the cell: its centre stays in, full containment fails.
            let holed = polygon_with_holes_wkb(&[outer, square(lon + 0.01, lat + 0.01, 0.002)]);
            assert!(fill(grid, &holed, 9, ContainmentMode::ContainsCentroid).contains(&cell));
            assert!(!fill(grid, &holed, 9, ContainmentMode::ContainsBoundary).contains(&cell));
            assert!(fill(grid, &holed, 9, ContainmentMode::Covers).contains(&cell));

            // A polygon smaller than a cell: only the overlap modes find it.
            let tiny = polygon_with_holes_wkb(&[square(lon, lat, 0.0005)]);
            assert_eq!(fill(grid, &tiny, 9, ContainmentMode::Covers), vec![cell]);
            assert!(fill(grid, &tiny, 9, ContainmentMode::ContainsBoundary).is_empty());

            // A polar cap box finds the cells around the pole.
            let cap = rect_wkb(-180.0, 88.0, 180.0, 90.0);
            let cells = fill(grid, &cap, 6, ContainmentMode::Covers);
            for lon in [-170.0, -60.0, 45.0, 179.0] {
                assert!(
                    cells.contains(&grid.cell_at(89.5, lon, 6).unwrap()),
                    "{grid} {lon}"
                );
            }
            // Resolution 0 returns base cells.
            let base = fill(grid, &solid, 0, ContainmentMode::Covers);
            assert_eq!(base, vec![grid.cell_at(lat, lon, 0).unwrap()]);
        }
    }

    #[test]
    fn nested_grid_polygon_fill_rejects_bad_requests() {
        for grid in NESTED {
            let mut out = vec![123];
            let big = rect_wkb(-123.0, 37.0, -122.0, 38.0);
            let error = grid_polygon_to_cells_wkb_into(
                grid,
                &big,
                i64::from(grid.max_resolution()),
                ContainmentMode::Covers,
                &mut out,
            )
            .unwrap_err();
            assert!(matches!(error, PolygonToCellsError::TooManyCells { .. }));
            assert!(out.is_empty());
            for res in [-1, i64::from(grid.max_resolution()) + 1] {
                assert_eq!(
                    grid_polygon_to_cells_wkb_into(
                        grid,
                        &big,
                        res,
                        ContainmentMode::Covers,
                        &mut out
                    ),
                    Err(PolygonToCellsError::InvalidResolution(res))
                );
            }
            assert!(matches!(
                grid_polygon_to_cells_wkb_into(
                    grid,
                    &[1, 2, 3],
                    5,
                    ContainmentMode::Covers,
                    &mut out
                ),
                Err(PolygonToCellsError::InvalidGeometry(_))
            ));
        }
    }
}

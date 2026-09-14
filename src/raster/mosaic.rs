//! Multi-File Directory, Globbing, and VRT Mosaic Ingestion
//!
//! Provides seamless streaming aggregation across collections of GeoTIFF tiles
//! with deterministic scanline horizon interleaving and configurable overlap resolution.

use std::fs;
use std::path::{Path, PathBuf};

use crate::aggregator::horizon_streamer::chunk_intersects_bbox;
use crate::crs::transformer::CrsTransformer;
use crate::error::{RasterH3Error, Result};
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::http_range::is_remote_url;

/// Overlap resolution strategy for overlapping tiles
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlapRule {
    /// Geometric Voronoi bisector between tile centroids (Default)
    #[default]
    Cutline,
    /// Priority ordering: first file in input list takes precedence
    First,
    /// Accumulate all overlapping observations into target H3 cell
    Average,
}

impl OverlapRule {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "cutline" | "voronoi" | "nearest" => Self::Cutline,
            "first" | "priority" | "painter" => Self::First,
            "average" | "mean" | "all" | "multi_temporal" => Self::Average,
            _ => Self::Cutline,
        }
    }
}

/// Classical linear wildcard matching supporting `*` and `?`
pub fn glob_match(pattern: &str, s: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    let (mut pi, mut si) = (0, 0);
    let (mut star_idx, mut match_idx) = (None, 0);

    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_idx = Some(pi);
            pi += 1;
            match_idx = si;
        } else if let Some(star) = star_idx {
            pi = star + 1;
            match_idx += 1;
            si = match_idx;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }

    pi == p.len()
}

/// Recursively collect files matching pattern in a directory
fn collect_matching_files(dir: &Path, file_pattern: &str, results: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_matching_files(&path, file_pattern, results);
            } else if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                if glob_match(file_pattern, file_name) {
                    results.push(path);
                }
            }
        }
    }
}

/// Parse GDAL VRT XML file and extract source file paths
fn parse_vrt_sources(vrt_path: &Path) -> Result<Vec<PathBuf>> {
    let content = fs::read_to_string(vrt_path).map_err(|e| {
        RasterH3Error::InvalidParameter(format!("Failed to read VRT file {:?}: {}", vrt_path, e))
    })?;

    let parent_dir = vrt_path.parent().unwrap_or_else(|| Path::new("."));
    let mut sources = Vec::new();

    let mut start_pos = 0;
    while let Some(tag_open) = content[start_pos..].find("<SourceFilename") {
        let tag_start = start_pos + tag_open;
        if let Some(tag_close) = content[tag_start..].find('>') {
            let content_start = tag_start + tag_close + 1;
            if let Some(end_tag) = content[content_start..].find("</SourceFilename>") {
                let filename_raw = content[content_start..content_start + end_tag].trim();
                if is_remote_url(filename_raw) {
                    sources.push(PathBuf::from(filename_raw));
                } else {
                    let full_path = parent_dir.join(filename_raw);
                    if full_path.exists() {
                        sources.push(full_path);
                    } else if Path::new(filename_raw).exists() {
                        sources.push(PathBuf::from(filename_raw));
                    } else {
                        sources.push(full_path);
                    }
                }
                start_pos = content_start + end_tag + 17;
                continue;
            }
        }
        break;
    }

    if sources.is_empty() {
        return Err(RasterH3Error::InvalidParameter(format!(
            "No valid <SourceFilename> entries found in VRT: {:?}",
            vrt_path
        )));
    }

    Ok(sources)
}

/// Resolve input string to a sorted list of GeoTIFF file paths or remote URLs
pub fn resolve_raster_sources(input: &str) -> Result<Vec<PathBuf>> {
    let trimmed = input.trim();

    // 1. Check for GDAL VRT file
    let path = Path::new(trimmed);
    if (trimmed.ends_with(".vrt") || trimmed.ends_with(".VRT")) && path.is_file() {
        return parse_vrt_sources(path);
    }

    // 2. Check for comma-separated list (can contain local paths or remote URLs)
    if trimmed.contains(',') {
        let mut paths = Vec::new();
        for part in trimmed.split(',') {
            let p_str = part.trim();
            if !p_str.is_empty() {
                if is_remote_url(p_str) {
                    paths.push(PathBuf::from(p_str));
                } else {
                    let p = PathBuf::from(p_str);
                    if !p.exists() {
                        return Err(RasterH3Error::InvalidParameter(format!(
                            "Source file does not exist: {:?}",
                            p
                        )));
                    }
                    paths.push(p);
                }
            }
        }
        if !paths.is_empty() {
            return Ok(paths);
        }
    }

    // 3. Check for single remote URL (http://, https://, s3://)
    if is_remote_url(trimmed) {
        return Ok(vec![PathBuf::from(trimmed)]);
    }

    // 3. Check for glob wildcard pattern (* or ?)
    if trimmed.contains('*') || trimmed.contains('?') {
        let mut results = Vec::new();
        let path_obj = Path::new(trimmed);

        let (base_dir, file_pattern) = if let Some(parent) = path_obj.parent() {
            if parent.as_os_str().is_empty() {
                (Path::new("."), path_obj.file_name().and_then(|f| f.to_str()).unwrap_or(trimmed))
            } else {
                (parent, path_obj.file_name().and_then(|f| f.to_str()).unwrap_or("*"))
            }
        } else {
            (Path::new("."), trimmed)
        };

        collect_matching_files(base_dir, file_pattern, &mut results);
        results.sort();
        if results.is_empty() {
            return Err(RasterH3Error::InvalidParameter(format!(
                "No files matched glob pattern: {}",
                trimmed
            )));
        }
        return Ok(results);
    }

    // 4. Single file
    if path.exists() {
        Ok(vec![path.to_path_buf()])
    } else {
        Err(RasterH3Error::InvalidParameter(format!(
            "Input file path does not exist: {}",
            trimmed
        )))
    }
}

/// Metadata and spatial bounds for a single tile in a mosaic
#[derive(Clone)]
pub struct TileDescriptor {
    pub tile_idx: usize,
    pub file_path: PathBuf,
    pub reader: GeoTiffStreamReader,
    pub bounds_wgs84: [f64; 4], // [min_lon, min_lat, max_lon, max_lat]
    pub centroid_wgs84: (f64, f64), // (lon, lat)
    pub crs_transformer: CrsTransformer,
}

impl TileDescriptor {
    pub fn new(
        tile_idx: usize,
        reader: GeoTiffStreamReader,
        custom_crs: Option<&str>,
    ) -> Result<Self> {
        let (epsg_to_use, proj_to_use) = if let Some(custom) = custom_crs {
            (None, Some(custom))
        } else {
            (reader.metadata.epsg, reader.metadata.proj_string.as_deref())
        };

        let crs_transformer = CrsTransformer::from_crs_or_epsg(epsg_to_use, proj_to_use)
            .map_err(|e| match e {
                RasterH3Error::CrsError(msg) => RasterH3Error::CrsError(format!(
                    "Tile {} ({:?}): {}",
                    tile_idx, reader.file_path, msg
                )),
                other => other,
            })?;

        let w = reader.metadata.width as f64;
        let h = reader.metadata.height as f64;
        let gt = &reader.metadata.geotransform;

        let corners = [
            gt.pixel_to_coord(0.0, 0.0),
            gt.pixel_to_coord(w, 0.0),
            gt.pixel_to_coord(w, h),
            gt.pixel_to_coord(0.0, h),
        ];

        let mut min_lon = f64::INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lon = f64::NEG_INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for (x, y) in corners {
            if let Ok((lon, lat)) = crs_transformer.transform_point(x, y) {
                min_lon = min_lon.min(lon);
                max_lon = max_lon.max(lon);
                min_lat = min_lat.min(lat);
                max_lat = max_lat.max(lat);
            }
        }

        let centroid_wgs84 = ((min_lon + max_lon) * 0.5, (min_lat + max_lat) * 0.5);

        Ok(Self {
            tile_idx,
            file_path: reader.file_path.clone(),
            reader,
            bounds_wgs84: [min_lon, min_lat, max_lon, max_lat],
            centroid_wgs84,
            crs_transformer,
        })
    }
}

/// Global chunk reference across all tiles in a mosaic, sorted by latitude
#[derive(Debug, Clone, Copy)]
pub struct MosaicChunkRef {
    pub tile_idx: usize,
    pub chunk_idx: u32,
    pub north_lat: f64,
    pub south_lat: f64,
    pub has_overlap: bool,
}

/// Multi-file mosaic reader managing tile descriptors and globally interleaved chunk prefetching
pub struct MosaicReader {
    pub tiles: Vec<TileDescriptor>,
    pub mosaic_bounds_wgs84: [f64; 4],
    pub overlap_rule: OverlapRule,
    pub chunk_refs: Vec<MosaicChunkRef>,
    pub max_samples_per_pixel: u16,
}

impl MosaicReader {
    /// Create a 1-tile MosaicReader from an existing single GeoTiffStreamReader
    pub fn from_single_reader(
        reader: GeoTiffStreamReader,
        bbox: Option<[f64; 4]>,
        custom_crs: Option<&str>,
    ) -> Result<Self> {
        let desc = TileDescriptor::new(0, reader, custom_crs)?;
        let m_bounds = desc.bounds_wgs84;
        let max_spp = desc.reader.metadata.samples_per_pixel;
        let tiles = vec![desc];

        let mut chunk_refs = Vec::new();
        let tile = &tiles[0];
        let total_chunks = tile.reader.chunk_layout.total_chunks;
        let gt = &tile.reader.metadata.geotransform;
        let crs_trans = &tile.crs_transformer;

        for chunk_idx in 0..total_chunks {
            let chunk_bounds = tile.reader.chunk_layout.get_chunk_bounds(
                chunk_idx,
                tile.reader.metadata.width,
                tile.reader.metadata.height,
            );

            if let Some(ref b) = bbox {
                if !chunk_intersects_bbox(&chunk_bounds, gt, crs_trans, b) {
                    continue;
                }
            }

            let corners = [
                gt.pixel_to_coord(chunk_bounds.col_offset as f64, chunk_bounds.row_offset as f64),
                gt.pixel_to_coord((chunk_bounds.col_offset + chunk_bounds.width) as f64, chunk_bounds.row_offset as f64),
                gt.pixel_to_coord((chunk_bounds.col_offset + chunk_bounds.width) as f64, (chunk_bounds.row_offset + chunk_bounds.height) as f64),
                gt.pixel_to_coord(chunk_bounds.col_offset as f64, (chunk_bounds.row_offset + chunk_bounds.height) as f64),
            ];

            let mut north_lat = f64::NEG_INFINITY;
            let mut south_lat = f64::INFINITY;

            for (x, y) in corners {
                if let Ok((_lon, lat)) = crs_trans.transform_point(x, y) {
                    north_lat = north_lat.max(lat);
                    south_lat = south_lat.min(lat);
                }
            }

            if north_lat == f64::NEG_INFINITY {
                north_lat = 0.0;
                south_lat = 0.0;
            }

            chunk_refs.push(MosaicChunkRef {
                tile_idx: 0,
                chunk_idx,
                north_lat,
                south_lat,
                has_overlap: false,
            });
        }

        chunk_refs.sort_by(|a, b| {
            b.north_lat
                .partial_cmp(&a.north_lat)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk_idx.cmp(&b.chunk_idx))
        });

        Ok(Self {
            tiles,
            mosaic_bounds_wgs84: m_bounds,
            overlap_rule: OverlapRule::Cutline,
            chunk_refs,
            max_samples_per_pixel: max_spp,
        })
    }

    /// Open a collection of GeoTIFF tiles as a unified mosaic
    pub fn open(
        paths: &[PathBuf],
        bbox: Option<[f64; 4]>,
        custom_crs: Option<&str>,
        overlap_rule: OverlapRule,
    ) -> Result<Self> {
        if paths.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "Mosaic paths list cannot be empty".to_string(),
            ));
        }

        let mut tiles = Vec::with_capacity(paths.len());
        let mut m_min_lon = f64::INFINITY;
        let mut m_min_lat = f64::INFINITY;
        let mut m_max_lon = f64::NEG_INFINITY;
        let mut m_max_lat = f64::NEG_INFINITY;
        let mut max_spp = 1u16;

        for (idx, p) in paths.iter().enumerate() {
            let reader = GeoTiffStreamReader::open(p)?;
            max_spp = max_spp.max(reader.metadata.samples_per_pixel);
            let desc = TileDescriptor::new(idx, reader, custom_crs)?;

            // If user specified bbox, skip tiles that don't intersect the bbox at all
            if let Some(ref b) = bbox {
                let tb = &desc.bounds_wgs84;
                if tb[2] < b[0] || tb[0] > b[2] || tb[3] < b[1] || tb[1] > b[3] {
                    continue;
                }
            }

            m_min_lon = m_min_lon.min(desc.bounds_wgs84[0]);
            m_min_lat = m_min_lat.min(desc.bounds_wgs84[1]);
            m_max_lon = m_max_lon.max(desc.bounds_wgs84[2]);
            m_max_lat = m_max_lat.max(desc.bounds_wgs84[3]);

            tiles.push(desc);
        }

        if tiles.is_empty() {
            return Err(RasterH3Error::InvalidParameter(
                "No mosaic tiles intersect the requested bounding box".to_string(),
            ));
        }

        // Build globally latitude-sorted chunk list across all tiles
        let mut chunk_refs = Vec::new();
        for (tile_idx, tile) in tiles.iter().enumerate() {
            let total_chunks = tile.reader.chunk_layout.total_chunks;
            let gt = &tile.reader.metadata.geotransform;
            let crs_trans = &tile.crs_transformer;

            for chunk_idx in 0..total_chunks {
                let chunk_bounds = tile.reader.chunk_layout.get_chunk_bounds(
                    chunk_idx,
                    tile.reader.metadata.width,
                    tile.reader.metadata.height,
                );

                if let Some(ref b) = bbox {
                    if !chunk_intersects_bbox(&chunk_bounds, gt, crs_trans, b) {
                        continue;
                    }
                }

                let corners = [
                    gt.pixel_to_coord(chunk_bounds.col_offset as f64, chunk_bounds.row_offset as f64),
                    gt.pixel_to_coord((chunk_bounds.col_offset + chunk_bounds.width) as f64, chunk_bounds.row_offset as f64),
                    gt.pixel_to_coord((chunk_bounds.col_offset + chunk_bounds.width) as f64, (chunk_bounds.row_offset + chunk_bounds.height) as f64),
                    gt.pixel_to_coord(chunk_bounds.col_offset as f64, (chunk_bounds.row_offset + chunk_bounds.height) as f64),
                ];

                let mut c_min_lon = f64::INFINITY;
                let mut c_min_lat = f64::INFINITY;
                let mut c_max_lon = f64::NEG_INFINITY;
                let mut c_max_lat = f64::NEG_INFINITY;

                for (x, y) in corners {
                    if let Ok((lon, lat)) = crs_trans.transform_point(x, y) {
                        c_min_lon = c_min_lon.min(lon);
                        c_max_lon = c_max_lon.max(lon);
                        c_min_lat = c_min_lat.min(lat);
                        c_max_lat = c_max_lat.max(lat);
                    }
                }

                if c_max_lat == f64::NEG_INFINITY {
                    c_min_lon = 0.0;
                    c_max_lon = 0.0;
                    c_min_lat = 0.0;
                    c_max_lat = 0.0;
                }

                let mut has_overlap = false;
                if tiles.len() > 1 && overlap_rule != OverlapRule::Average {
                    for (other_idx, other_tile) in tiles.iter().enumerate() {
                        if other_idx == tile_idx {
                            continue;
                        }
                        let ob = &other_tile.bounds_wgs84;
                        if !(c_max_lon < ob[0] || c_min_lon > ob[2] || c_max_lat < ob[1] || c_min_lat > ob[3]) {
                            has_overlap = true;
                            break;
                        }
                    }
                }

                chunk_refs.push(MosaicChunkRef {
                    tile_idx,
                    chunk_idx,
                    north_lat: c_max_lat,
                    south_lat: c_min_lat,
                    has_overlap,
                });
            }
        }

        // Sort all chunks from all tiles globally in North-to-South order
        chunk_refs.sort_by(|a, b| {
            b.north_lat
                .partial_cmp(&a.north_lat)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.tile_idx.cmp(&b.tile_idx))
                .then_with(|| a.chunk_idx.cmp(&b.chunk_idx))
        });

        Ok(Self {
            tiles,
            mosaic_bounds_wgs84: [m_min_lon, m_min_lat, m_max_lon, m_max_lat],
            overlap_rule,
            chunk_refs,
            max_samples_per_pixel: max_spp,
        })
    }

    /// Check if point is owned by tile_idx under current overlap rule
    #[inline(always)]
    pub fn is_point_owned_by(&self, tile_idx: usize, lon: f64, lat: f64) -> bool {
        if self.tiles.len() <= 1 || self.overlap_rule == OverlapRule::Average {
            return true;
        }

        match self.overlap_rule {
            OverlapRule::Average => true,
            OverlapRule::First => {
                for j in 0..tile_idx {
                    let b = &self.tiles[j].bounds_wgs84;
                    if lon >= b[0] && lon <= b[2] && lat >= b[1] && lat <= b[3] {
                        return false;
                    }
                }
                true
            }
            OverlapRule::Cutline => {
                let mut best_tile = tile_idx;
                let mut best_dist_sq = f64::INFINITY;

                for (idx, tile) in self.tiles.iter().enumerate() {
                    let b = &tile.bounds_wgs84;
                    if lon >= b[0] && lon <= b[2] && lat >= b[1] && lat <= b[3] {
                        let d_lon = lon - tile.centroid_wgs84.0;
                        let d_lat = lat - tile.centroid_wgs84.1;
                        let dist_sq = d_lon * d_lon + d_lat * d_lat;
                        if dist_sq < best_dist_sq {
                            best_dist_sq = dist_sq;
                            best_tile = idx;
                        }
                    }
                }

                best_tile == tile_idx
            }
        }
    }

    /// Total pixel count across all tiles in the mosaic
    pub fn total_pixels(&self) -> u64 {
        self.tiles
            .iter()
            .map(|t| (t.reader.metadata.width as u64) * (t.reader.metadata.height as u64))
            .sum()
    }
}

//! Partitioned scans: splitting one mosaic across independent workers (e.g. Spark tasks)
//! without reading any chunk twice.
//!
//! A [`ScanPartition`] is a set of rectangular [`ChunkBlock`]s of the mosaic's tile chunk
//! grids. [`plan_partitions`] cuts a mosaic into partitions from metadata only; each worker
//! reopens the same mosaic, sets [`MultiResolutionConfig::partition`], and streams only its
//! own chunks with the normal single-node pipeline.
//!
//! A cell whose samples may come from chunks of more than one partition cannot be finalized
//! by any single worker. When a partitioned stream finalizes a cell, it classifies it:
//!
//! - **interior**: no chunk outside the partition can hold a sample inside the cell, so the
//!   cell is complete and is emitted as a normal record (filters applied);
//! - **boundary**: otherwise. The cell's accumulator is emitted as a [`PartialCell`]
//!   (unfiltered), to be merged with the other partitions' partials by
//!   [`PartialMerger`](super::partial::PartialMerger).
//!
//! Classification is conservative. A chunk's samples lie inside its certified WGS84 bounds
//! ([`CrsTransformer::rejection_bounds`], the whole globe when uncertified), and a cell's
//! samples lie inside the cell, so a cell whose padded lat/lon box misses the bounds of
//! every outside chunk is interior. Anything uncertain (uncertified CRS bounds, cells near a
//! pole or across the antimeridian) is classed as boundary, which is always correct and only
//! costs extra merge work.
//!
//! [`MultiResolutionConfig::partition`]: super::config::MultiResolutionConfig::partition
//! [`CrsTransformer::rejection_bounds`]: crate::crs::transformer::CrsTransformer::rejection_bounds

use h3o::{CellIndex, Resolution};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::error::{RasterH3Error, Result};
use crate::raster::mosaic::{MosaicChunkRef, MosaicReader};

/// A rectangle of one tile's chunk grid: chunk columns `col_start..col_end` and chunk rows
/// `row_start..row_end` (half-open) of tile `tile_idx`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkBlock {
    /// Index of the tile in [`MosaicReader::tiles`].
    pub tile_idx: u32,
    /// First chunk column (inclusive).
    pub col_start: u32,
    /// Last chunk column (exclusive).
    pub col_end: u32,
    /// First chunk row (inclusive).
    pub row_start: u32,
    /// Last chunk row (exclusive).
    pub row_end: u32,
}

impl ChunkBlock {
    /// Whether chunk `(col, row)` of tile `tile_idx` lies in this block.
    #[inline]
    pub fn contains(&self, tile_idx: usize, col: u32, row: u32) -> bool {
        self.tile_idx as usize == tile_idx
            && (self.col_start..self.col_end).contains(&col)
            && (self.row_start..self.row_end).contains(&row)
    }
}

/// One independently scannable part of a mosaic. Serializable (see [`Self::to_json`]) so a
/// driver can plan partitions and ship them to workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanPartition {
    /// 0-based index of this partition in its plan.
    pub index: u32,
    /// Number of partitions in the plan.
    pub count: u32,
    /// [`mosaic_fingerprint`] of the mosaic the plan was made for. A worker whose mosaic
    /// has another layout (other raster dimensions, chunking or georeferencing, bbox or
    /// overlap rule) refuses the partition. Rasters that differ only in pixel values share
    /// a layout, and a plan for one is valid for the others.
    pub mosaic_fingerprint: u64,
    /// Disjoint chunk rectangles; together, the plan's partitions cover every chunk once.
    pub blocks: Vec<ChunkBlock>,
}

impl ScanPartition {
    /// Serialize to JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ScanPartition serializes")
    }

    /// Parse a partition serialized with [`Self::to_json`].
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| RasterH3Error::InvalidParameter(format!("invalid scan partition: {e}")))
    }

    /// Whether `chunk` belongs to this partition.
    pub fn contains_chunk(&self, mosaic: &MosaicReader, chunk: &MosaicChunkRef) -> bool {
        let (col, row) = chunk_col_row(mosaic, chunk);
        self.blocks
            .iter()
            .any(|b| b.contains(chunk.tile_idx, col, row))
    }

    /// Number of the mosaic's chunks in this partition.
    pub fn chunk_count(&self, mosaic: &MosaicReader) -> usize {
        mosaic
            .chunk_refs
            .iter()
            .filter(|c| self.contains_chunk(mosaic, c))
            .count()
    }

    /// Check that this partition was planned for `mosaic` and that its blocks fit it.
    pub fn validate_for(&self, mosaic: &MosaicReader) -> Result<()> {
        if self.count == 0 || self.index >= self.count {
            return Err(RasterH3Error::InvalidParameter(format!(
                "scan partition index {} out of range for {} partitions",
                self.index, self.count
            )));
        }
        let fingerprint = mosaic_fingerprint(mosaic);
        if fingerprint != self.mosaic_fingerprint {
            return Err(RasterH3Error::InvalidParameter(format!(
                "scan partition was planned for a different mosaic (fingerprint {:016x}, this mosaic {:016x}); \
                 open the same sources with the same bbox, CRS override and overlap rule",
                self.mosaic_fingerprint, fingerprint
            )));
        }
        for b in &self.blocks {
            let Some(tile) = mosaic.tiles.get(b.tile_idx as usize) else {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "scan partition block names tile {} of {}",
                    b.tile_idx,
                    mosaic.tiles.len()
                )));
            };
            let layout = &tile.reader.chunk_layout;
            if b.col_start >= b.col_end
                || b.row_start >= b.row_end
                || b.col_end > layout.chunks_across
                || b.row_end > layout.chunks_down
            {
                return Err(RasterH3Error::InvalidParameter(format!(
                    "scan partition block {b:?} does not fit tile {} ({}x{} chunks)",
                    b.tile_idx, layout.chunks_across, layout.chunks_down
                )));
            }
        }
        Ok(())
    }
}

/// Chunk grid position of a chunk reference.
#[inline]
fn chunk_col_row(mosaic: &MosaicReader, chunk: &MosaicChunkRef) -> (u32, u32) {
    let across = mosaic.tiles[chunk.tile_idx]
        .reader
        .chunk_layout
        .chunks_across
        .max(1);
    (chunk.chunk_idx % across, chunk.chunk_idx / across)
}

/// A stable hash of everything that determines which chunks a mosaic streams and how its
/// samples are owned: tile dimensions, chunk layouts, geotransforms, the overlap rule and
/// the ordered chunk list (which reflects the bbox). File paths are left out so workers may
/// mount the same files under different paths.
pub fn mosaic_fingerprint(mosaic: &MosaicReader) -> u64 {
    // FNV-1a: stable across processes and Rust versions, unlike `DefaultHasher`.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |v: u64| {
        for byte in v.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(mosaic.tiles.len() as u64);
    feed(mosaic.overlap_rule as u64);
    for tile in &mosaic.tiles {
        let md = &tile.reader.metadata;
        let layout = &tile.reader.chunk_layout;
        for v in [
            md.width,
            md.height,
            layout.chunk_width,
            layout.chunk_height,
            layout.chunks_across,
            layout.chunks_down,
        ] {
            feed(u64::from(v));
        }
        let gt = &md.geotransform;
        for v in [gt.a, gt.b, gt.c0, gt.d, gt.e, gt.f0] {
            feed(v.to_bits());
        }
    }
    feed(mosaic.chunk_refs.len() as u64);
    for c in &mosaic.chunk_refs {
        feed(c.tile_idx as u64);
        feed(u64::from(c.chunk_idx));
    }
    h
}

/// Split a mosaic into partitions of about `target_chunks` chunks each.
///
/// Each tile's chunk grid is cut into rectangles that are roughly square in pixels
/// (fewer boundary cells per chunk read); rectangles holding no streamed chunk (pruned by
/// the bbox) are dropped. Consecutive rectangles are then packed into partitions while
/// they fit `target_chunks`, so small tiles of a mosaic share a partition. Every streamed
/// chunk belongs to exactly one partition.
///
/// Larger partitions amortize per-task setup and give fewer boundary cells; aim for a
/// few hundred chunks or more per partition.
pub fn plan_partitions(mosaic: &MosaicReader, target_chunks: usize) -> Result<Vec<ScanPartition>> {
    if target_chunks == 0 {
        return Err(RasterH3Error::InvalidParameter(
            "target_chunks must be at least 1".into(),
        ));
    }
    // Streamed chunk positions per tile.
    let mut present: Vec<Vec<(u32, u32)>> = vec![Vec::new(); mosaic.tiles.len()];
    for c in &mosaic.chunk_refs {
        present[c.tile_idx].push(chunk_col_row(mosaic, c));
    }

    let mut blocks: Vec<(ChunkBlock, usize)> = Vec::new();
    for (tile_idx, chunks) in present.iter().enumerate() {
        if chunks.is_empty() {
            continue;
        }
        let layout = &mosaic.tiles[tile_idx].reader.chunk_layout;
        let (c0, c1) = (
            chunks.iter().map(|p| p.0).min().unwrap_or(0),
            chunks.iter().map(|p| p.0).max().unwrap_or(0) + 1,
        );
        let (r0, r1) = (
            chunks.iter().map(|p| p.1).min().unwrap_or(0),
            chunks.iter().map(|p| p.1).max().unwrap_or(0) + 1,
        );
        let (cols, rows) = ((c1 - c0) as usize, (r1 - r0) as usize);
        // Square in pixels: bw * chunk_width ~= bh * chunk_height, bw * bh ~= target.
        let aspect = f64::from(layout.chunk_height.max(1)) / f64::from(layout.chunk_width.max(1));
        let bw = ((target_chunks as f64 * aspect).sqrt().round() as usize).clamp(1, cols);
        let bh = target_chunks.div_ceil(bw).clamp(1, rows);

        let mut grid = vec![false; cols * rows];
        for &(c, r) in chunks {
            grid[(r - r0) as usize * cols + (c - c0) as usize] = true;
        }
        for br in (0..rows).step_by(bh) {
            for bc in (0..cols).step_by(bw) {
                let (re, ce) = ((br + bh).min(rows), (bc + bw).min(cols));
                let n = (br..re)
                    .flat_map(|r| (bc..ce).map(move |c| r * cols + c))
                    .filter(|&i| grid[i])
                    .count();
                if n > 0 {
                    blocks.push((
                        ChunkBlock {
                            tile_idx: tile_idx as u32,
                            col_start: c0 + bc as u32,
                            col_end: c0 + ce as u32,
                            row_start: r0 + br as u32,
                            row_end: r0 + re as u32,
                        },
                        n,
                    ));
                }
            }
        }
    }

    let fingerprint = mosaic_fingerprint(mosaic);
    let mut partitions: Vec<ScanPartition> = Vec::new();
    let mut current: Vec<ChunkBlock> = Vec::new();
    let mut current_chunks = 0usize;
    for (block, n) in blocks {
        if !current.is_empty() && current_chunks + n > target_chunks {
            partitions.push(ScanPartition {
                index: 0,
                count: 0,
                mosaic_fingerprint: fingerprint,
                blocks: std::mem::take(&mut current),
            });
            current_chunks = 0;
        }
        current.push(block);
        current_chunks += n;
    }
    if !current.is_empty() {
        partitions.push(ScanPartition {
            index: 0,
            count: 0,
            mosaic_fingerprint: fingerprint,
            blocks: current,
        });
    }
    let count = partitions.len() as u32;
    for (i, p) in partitions.iter_mut().enumerate() {
        p.index = i as u32;
        p.count = count;
    }
    Ok(partitions)
}

/// A copy of `mosaic` that streams only the chunks of `partition`, in the same order.
pub(crate) fn restrict_mosaic(
    mosaic: &MosaicReader,
    partition: &ScanPartition,
) -> Result<MosaicReader> {
    partition.validate_for(mosaic)?;
    Ok(MosaicReader {
        tiles: mosaic.tiles.clone(),
        mosaic_bounds_wgs84: mosaic.mosaic_bounds_wgs84,
        overlap_rule: mosaic.overlap_rule,
        chunk_refs: mosaic
            .chunk_refs
            .iter()
            .filter(|c| partition.contains_chunk(mosaic, c))
            .copied()
            .collect(),
        max_samples_per_pixel: mosaic.max_samples_per_pixel,
    })
}

/// A sample-bearing cell that a partitioned stream could not finalize because other
/// partitions may hold samples inside it. Merge all partitions' partials for the same
/// `(resolution, h3_index)` to obtain the cell's final state.
#[derive(Debug, Clone)]
pub struct PartialCell<A> {
    /// H3 resolution of the cell.
    pub resolution: u8,
    /// H3 cell index.
    pub h3_index: u64,
    /// This partition's share of the cell's state (not filtered).
    pub accumulator: A,
}

/// Lat/lon box `[min_lon, min_lat, max_lon, max_lat]`, never crossing the antimeridian.
type Rect = [f64; 4];

#[inline]
fn rects_intersect(a: &Rect, b: &Rect) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}

/// Bounds of a chunk outside the partition, in a form that is safe to test against without
/// wrap handling: anything that is not a finite, non-crossing box inside [-180, 180] is
/// widened to the full longitude range (and the whole globe when non-finite).
fn normalized_foreign_bounds(b: [f64; 4]) -> Rect {
    let [w, s, e, n] = b;
    if !(w.is_finite() && s.is_finite() && e.is_finite() && n.is_finite()) || s > n {
        return [-180.0, -90.0, 180.0, 90.0];
    }
    if w > e || w < -180.0 || e > 180.0 {
        return [-180.0, s.max(-90.0), 180.0, n.min(90.0)];
    }
    [w, s.max(-90.0), e, n.min(90.0)]
}

/// Padded lat/lon box enclosing the cell, or `None` when no simple box is safe (the cell
/// is near a pole or crosses the antimeridian).
///
/// H3 edges are great-circle arcs, which can bulge past their end points; each edge is
/// padded by half its lat + lon extent, the same bound the eviction horizon uses
/// (`compute_cell_south_lat`). With `descendants`, the box is grown by a further full span
/// in every direction so it encloses every descendant of the cell at any finer resolution
/// (H3 children are not nested inside their parent but stay within a fraction of its
/// radius of it).
fn cell_box(cell: CellIndex, descendants: bool) -> Option<Rect> {
    let boundary = cell.boundary();
    if boundary.is_empty() {
        return None;
    }
    let (mut w, mut s, mut e, mut n) = (
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    );
    let mut pad = 0.0f64;
    for i in 0..boundary.len() {
        let p = boundary[i];
        let q = boundary[(i + 1) % boundary.len()];
        w = w.min(p.lng());
        e = e.max(p.lng());
        s = s.min(p.lat());
        n = n.max(p.lat());
        let half = 0.5 * ((p.lat() - q.lat()).abs() + (p.lng() - q.lng()).abs());
        pad = pad.max(half);
    }
    let lon_span = e - w;
    if lon_span.is_nan() || lon_span > 180.0 {
        return None; // wraps the antimeridian (or contains a pole)
    }
    let (span_lon, span_lat) = (e - w, n - s);
    let (mut grow_lon, mut grow_lat) = (pad, pad);
    if descendants {
        grow_lon += span_lon.max(span_lat);
        grow_lat += span_lon.max(span_lat);
    }
    // Relative slack for rounding in the boundary computation.
    let eps = 1e-9 + 1e-6 * span_lon.max(span_lat);
    let rect = [
        w - grow_lon - eps,
        s - grow_lat - eps,
        e + grow_lon + eps,
        n + grow_lat + eps,
    ];
    if rect[0] < -180.0 || rect[2] > 180.0 || rect[1] < -89.0 || rect[3] > 89.0 {
        return None;
    }
    Some(rect)
}

/// Spatial index over the bounds of every streamed chunk outside one partition.
struct ForeignChunks {
    rects: Vec<Rect>,
    /// Grid extent `[min_lon, min_lat, max_lon, max_lat]`.
    extent: Rect,
    nx: usize,
    ny: usize,
    /// For each grid bucket, the indexes of the rects overlapping it.
    buckets: Vec<Vec<u32>>,
}

impl ForeignChunks {
    fn new(rects: Vec<Rect>) -> Self {
        let mut extent = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for r in &rects {
            extent = [
                extent[0].min(r[0]),
                extent[1].min(r[1]),
                extent[2].max(r[2]),
                extent[3].max(r[3]),
            ];
        }
        let side = ((rects.len() as f64).sqrt().ceil() as usize).clamp(1, 512);
        let (nx, ny) = if rects.is_empty() {
            (1, 1)
        } else {
            (side, side)
        };
        let mut index = Self {
            rects,
            extent,
            nx,
            ny,
            buckets: vec![Vec::new(); nx * ny],
        };
        for i in 0..index.rects.len() {
            let r = index.rects[i];
            let (x0, y0, x1, y1) = index.bucket_range(&r);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    index.buckets[y * nx + x].push(i as u32);
                }
            }
        }
        index
    }

    /// Inclusive bucket range covering `r`, clamped to the grid.
    fn bucket_range(&self, r: &Rect) -> (usize, usize, usize, usize) {
        let [w, s, e, n] = self.extent;
        let fx = |lon: f64| {
            let t = if e > w { (lon - w) / (e - w) } else { 0.0 };
            ((t * self.nx as f64).floor().max(0.0) as usize).min(self.nx - 1)
        };
        let fy = |lat: f64| {
            let t = if n > s { (lat - s) / (n - s) } else { 0.0 };
            ((t * self.ny as f64).floor().max(0.0) as usize).min(self.ny - 1)
        };
        (fx(r[0]), fy(r[1]), fx(r[2]), fy(r[3]))
    }

    /// Whether `r` intersects the bounds of any outside chunk.
    fn intersects(&self, r: &Rect) -> bool {
        if self.rects.is_empty() || !rects_intersect(r, &self.extent) {
            return false;
        }
        let (x0, y0, x1, y1) = self.bucket_range(r);
        (y0..=y1).any(|y| {
            (x0..=x1).any(|x| {
                self.buckets[y * self.nx + x]
                    .iter()
                    .any(|&i| rects_intersect(r, &self.rects[i as usize]))
            })
        })
    }
}

/// Ancestors whose cached verdict is "every descendant is interior" are looked up first, so
/// most cells away from the partition's edges are classified with a parent computation and a
/// hash lookup instead of a boundary computation.
const ANCESTOR_LEVELS: [u8; 2] = [3, 1];
/// Cache entries kept per level before the cache is cleared.
const ANCESTOR_CACHE_LIMIT: usize = 1 << 18;

/// Classifies finalized cells of a partitioned stream as interior or boundary.
pub(crate) struct BoundaryClassifier {
    foreign: ForeignChunks,
    caches: [FxHashMap<u64, bool>; ANCESTOR_LEVELS.len()],
    interior: u64,
    boundary: u64,
}

impl BoundaryClassifier {
    /// Index the chunks of `mosaic` (the full, unrestricted mosaic) outside `partition`.
    pub(crate) fn new(mosaic: &MosaicReader, partition: &ScanPartition) -> Self {
        let rects = mosaic
            .chunk_refs
            .iter()
            .filter(|c| !partition.contains_chunk(mosaic, c))
            .map(|c| {
                let tile = &mosaic.tiles[c.tile_idx];
                let md = &tile.reader.metadata;
                let chunk =
                    tile.reader
                        .chunk_layout
                        .get_chunk_bounds(c.chunk_idx, md.width, md.height);
                let bounds = tile.crs_transformer.transform_rect_bounds(
                    &md.geotransform,
                    f64::from(chunk.col_offset),
                    f64::from(chunk.row_offset),
                    f64::from(chunk.width),
                    f64::from(chunk.height),
                );
                normalized_foreign_bounds(tile.crs_transformer.rejection_bounds(bounds))
            })
            .collect();
        Self {
            foreign: ForeignChunks::new(rects),
            caches: Default::default(),
            interior: 0,
            boundary: 0,
        }
    }

    /// Whether no outside chunk can hold a sample inside `cell_u64`.
    pub(crate) fn is_interior(&mut self, cell_u64: u64) -> bool {
        let interior = self.classify(cell_u64);
        if interior {
            self.interior += 1;
        } else {
            self.boundary += 1;
        }
        interior
    }

    fn classify(&mut self, cell_u64: u64) -> bool {
        if self.foreign.rects.is_empty() {
            return true; // a single partition covers the whole mosaic
        }
        let Ok(cell) = CellIndex::try_from(cell_u64) else {
            return false;
        };
        let res = u8::from(cell.resolution());
        for (level, &up) in ANCESTOR_LEVELS.iter().enumerate() {
            let Some(ancestor) = res
                .checked_sub(up)
                .and_then(|r| Resolution::try_from(r).ok())
                .and_then(|r| cell.parent(r))
            else {
                continue;
            };
            let foreign = &self.foreign;
            let cache = &mut self.caches[level];
            if cache.len() >= ANCESTOR_CACHE_LIMIT {
                cache.clear();
            }
            let clear = *cache.entry(u64::from(ancestor)).or_insert_with(|| {
                cell_box(ancestor, true).is_some_and(|r| !foreign.intersects(&r))
            });
            if clear {
                return true;
            }
        }
        cell_box(cell, false).is_some_and(|r| !self.foreign.intersects(&r))
    }

    /// `(interior, boundary)` cells classified so far.
    pub(crate) fn counts(&self) -> (u64, u64) {
        (self.interior, self.boundary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use h3o::LatLng;

    /// The descendant box of an ancestor must enclose every vertex of its descendants a few
    /// levels down, including near pentagons and on distorted (Class III) faces.
    #[test]
    fn descendant_box_encloses_descendants() {
        let mut checked = 0;
        for res in 1..=9u8 {
            let r = Resolution::try_from(res).unwrap();
            let mut cells: Vec<CellIndex> = Vec::new();
            for i in 0..60 {
                let lat = -80.0 + (i as f64) * 2.7;
                let lon = -179.0 + (i as f64) * 5.9;
                cells.push(LatLng::new(lat, lon).unwrap().to_cell(r));
            }
            cells.extend(
                CellIndex::base_cells()
                    .filter(|c| c.is_pentagon())
                    .filter_map(|c| c.center_child(r)),
            );
            for ancestor in cells {
                let Some(b) = cell_box(ancestor, true) else {
                    continue;
                };
                for down in 1..=3u8 {
                    let Ok(child_res) = Resolution::try_from(res + down) else {
                        continue;
                    };
                    for child in ancestor.children(child_res) {
                        for v in child.boundary().iter() {
                            assert!(
                                v.lng() >= b[0]
                                    && v.lng() <= b[2]
                                    && v.lat() >= b[1]
                                    && v.lat() <= b[3],
                                "descendant {child} of {ancestor} leaves its box"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 100_000, "only {checked} vertices checked");
    }

    #[test]
    fn foreign_bounds_are_normalized_conservatively() {
        assert_eq!(
            normalized_foreign_bounds([1.0, 2.0, 3.0, 4.0]),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(
            normalized_foreign_bounds([170.0, 2.0, 190.0, 4.0]),
            [-180.0, 2.0, 180.0, 4.0]
        );
        assert_eq!(
            normalized_foreign_bounds([f64::NAN, 2.0, 3.0, 4.0]),
            [-180.0, -90.0, 180.0, 90.0]
        );
    }

    #[test]
    fn foreign_index_matches_linear_scan() {
        let rects: Vec<Rect> = (0..400)
            .map(|i| {
                let (x, y) = ((i % 20) as f64, (i / 20) as f64);
                [x * 0.1, y * 0.1, x * 0.1 + 0.05, y * 0.1 + 0.05]
            })
            .collect();
        let index = ForeignChunks::new(rects.clone());
        for i in 0..2000 {
            let (x, y) = (
                (i * 37 % 211) as f64 * 0.01 - 0.05,
                (i * 53 % 223) as f64 * 0.01 - 0.05,
            );
            let q = [x, y, x + 0.012, y + 0.007];
            let linear = rects.iter().any(|r| rects_intersect(&q, r));
            assert_eq!(index.intersects(&q), linear, "query {q:?}");
        }
    }
}

//! Bounded-memory export of unordered H3 features to a PMTiles archive.
//!
//! [`TilePyramidAccumulator`] can only write a tile once no later feature can
//! touch it, which it learns from features arriving north to south. Callers
//! such as `COPY ... (FORMAT pmtiles)` hand features over in arbitrary order,
//! so [`ExternalFeatureExport`] performs an external sort first:
//!
//! 1. [`push`](ExternalFeatureExport::push) buffers features until the
//!    in-memory estimate exceeds the budget, then sorts the buffer by cell
//!    center latitude (north first) and writes it to a temporary run file.
//! 2. [`finish_into`](ExternalFeatureExport::finish_into) k-way merges the
//!    runs (or sorts the buffer directly when nothing spilled) and feeds the
//!    accumulator in latitude order, evicting finished tiles to the
//!    [`PmtilesWriter`]'s disk spill as the merge moves south.
//!
//! Peak memory is therefore the buffer budget plus one band of open tiles,
//! not the whole input. The sort order (latitude descending, then H3 index)
//! is total, so the archive bytes do not depend on the budget.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use h3o::{CellIndex, LatLng};

use crate::pmtiles::features::{
    build_default_pmtiles_fields, build_pmtiles_metadata, H3Feature, PmtilesExportSummary,
    TilePyramidAccumulator,
};
use crate::pmtiles::mvt::{MercatorPoint, MvtValue};
use crate::pmtiles::pyramid::{
    cell_boundary_mercator, h3_res_to_zoom, max_hex_radius_deg, Wgs84BoundsAccumulator,
};
use crate::pmtiles::writer::PmtilesWriter;

/// Default in-memory buffer budget: 256 MiB.
pub const DEFAULT_EXPORT_MEMORY_BUDGET: usize = 256 << 20;

/// Evict finished tiles after this many merged features. Eviction compresses
/// the ready tiles as one parallel batch, so calling it per feature would
/// produce tiny batches.
const EVICT_EVERY: usize = 4096;

/// A validated feature waiting to be sorted: its cell center latitude (the
/// sort key), its index, and its properties with keys interned as ids.
struct SortRecord {
    lat: f64,
    h3_index: u64,
    properties: Vec<(u16, MvtValue)>,
}

impl SortRecord {
    /// Latitude descending (north first), then H3 index ascending. Total, so
    /// in-memory and merged orders agree exactly.
    fn order(&self, other: &Self) -> Ordering {
        other
            .lat
            .total_cmp(&self.lat)
            .then(self.h3_index.cmp(&other.h3_index))
    }

    /// Rough heap footprint, used only to decide when to spill.
    fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            + self
                .properties
                .iter()
                .map(|(_, v)| {
                    size_of::<(u16, MvtValue)>()
                        + match v {
                            MvtValue::String(s) => s.capacity(),
                            _ => 0,
                        }
                })
                .sum::<usize>()
    }

    fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.lat.to_le_bytes())?;
        w.write_all(&self.h3_index.to_le_bytes())?;
        let count = u16::try_from(self.properties.len())
            .map_err(|_| io::Error::other("more than 65535 properties on one feature"))?;
        w.write_all(&count.to_le_bytes())?;
        for (key, value) in &self.properties {
            w.write_all(&key.to_le_bytes())?;
            match value {
                MvtValue::String(s) => {
                    w.write_all(&[0])?;
                    let len = u32::try_from(s.len())
                        .map_err(|_| io::Error::other("property string longer than 4 GiB"))?;
                    w.write_all(&len.to_le_bytes())?;
                    w.write_all(s.as_bytes())?;
                }
                MvtValue::HexStr(bytes, len) => {
                    w.write_all(&[1, *len])?;
                    w.write_all(&bytes[..usize::from(*len).min(bytes.len())])?;
                }
                MvtValue::Float(v) => {
                    w.write_all(&[2])?;
                    w.write_all(&v.to_le_bytes())?;
                }
                MvtValue::Double(v) => {
                    w.write_all(&[3])?;
                    w.write_all(&v.to_le_bytes())?;
                }
                MvtValue::Int(v) => {
                    w.write_all(&[4])?;
                    w.write_all(&v.to_le_bytes())?;
                }
                MvtValue::UInt(v) => {
                    w.write_all(&[5])?;
                    w.write_all(&v.to_le_bytes())?;
                }
                MvtValue::Bool(v) => w.write_all(&[6, u8::from(*v)])?,
            }
        }
        Ok(())
    }

    /// Read the next record, or `None` at a clean end of the run.
    fn read_from<R: Read>(r: &mut R) -> io::Result<Option<Self>> {
        let mut lat = [0u8; 8];
        match r.read_exact(&mut lat) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let h3_index = u64::from_le_bytes(read_array(r)?);
        let count = u16::from_le_bytes(read_array(r)?);
        let mut properties = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let key = u16::from_le_bytes(read_array(r)?);
            let [tag] = read_array(r)?;
            let value = match tag {
                0 => {
                    let len = u32::from_le_bytes(read_array(r)?) as usize;
                    let mut buf = vec![0u8; len];
                    r.read_exact(&mut buf)?;
                    MvtValue::String(String::from_utf8(buf).map_err(io::Error::other)?)
                }
                1 => {
                    let [len] = read_array(r)?;
                    let mut bytes = [0u8; 16];
                    let n = usize::from(len).min(bytes.len());
                    r.read_exact(&mut bytes[..n])?;
                    MvtValue::HexStr(bytes, len)
                }
                2 => MvtValue::Float(f32::from_le_bytes(read_array(r)?)),
                3 => MvtValue::Double(f64::from_le_bytes(read_array(r)?)),
                4 => MvtValue::Int(i64::from_le_bytes(read_array(r)?)),
                5 => MvtValue::UInt(u64::from_le_bytes(read_array(r)?)),
                6 => {
                    let [b] = read_array(r)?;
                    MvtValue::Bool(b != 0)
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("corrupt export run: unknown value tag {other}"),
                    ))
                }
            };
            properties.push((key, value));
        }
        Ok(Some(Self {
            lat: f64::from_le_bytes(lat),
            h3_index,
            properties,
        }))
    }
}

fn read_array<R: Read, const N: usize>(r: &mut R) -> io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Head of one run during the merge, ordered so `BinaryHeap` (a max-heap)
/// pops the next record in export order.
struct MergeHead {
    record: SortRecord,
    run: usize,
}

impl PartialEq for MergeHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MergeHead {}

impl PartialOrd for MergeHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MergeHead {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse: the record that sorts first must be the heap maximum.
        // Ties between runs fall back to the run index for determinism.
        other
            .record
            .order(&self.record)
            .then(other.run.cmp(&self.run))
    }
}

/// External-sort PMTiles exporter for H3 features in arbitrary order. See
/// the module documentation.
pub struct ExternalFeatureExport {
    budget_bytes: usize,
    spill_dir: Option<PathBuf>,
    buffer: Vec<SortRecord>,
    buffer_bytes: usize,
    runs: Vec<File>,
    keys: Vec<Cow<'static, str>>,
    key_ids: HashMap<Cow<'static, str>, u16>,
    bounds: Wgs84BoundsAccumulator,
    min_zoom: u8,
    max_zoom: u8,
    coarsest_res: u8,
    total_features: usize,
    valid_features: usize,
    evict_every: usize,
    /// Extra `vector_layers` fields (name -> `"Number"` / `"String"` / `"Boolean"`) listed
    /// in the archive metadata next to the default `h3_index`, `h3_hex` and `resolution`.
    layer_fields: serde_json::Map<String, serde_json::Value>,
}

impl ExternalFeatureExport {
    /// Create an exporter that keeps at most about `budget_bytes` of buffered
    /// features in memory, spilling sorted runs to `spill_dir` (the system
    /// temp directory when `None`).
    ///
    /// `spill_dir` is created only when a run actually has to be written, and
    /// the system temp directory is used if it cannot be. The PMTiles
    /// writer's tile spill goes to `spill_dir` only if it exists by then.
    pub fn new(budget_bytes: usize, spill_dir: Option<&Path>) -> Self {
        Self {
            budget_bytes: budget_bytes.max(1),
            spill_dir: spill_dir.map(Path::to_path_buf),
            buffer: Vec::new(),
            buffer_bytes: 0,
            runs: Vec::new(),
            keys: Vec::new(),
            key_ids: HashMap::new(),
            bounds: Wgs84BoundsAccumulator::new(),
            min_zoom: u8::MAX,
            max_zoom: 0,
            coarsest_res: u8::MAX,
            total_features: 0,
            valid_features: 0,
            evict_every: EVICT_EVERY,
            layer_fields: serde_json::Map::new(),
        }
    }

    /// List the features' property names and types in the archive's `vector_layers`
    /// metadata (TileJSON field types: `"Number"`, `"String"` or `"Boolean"`), in addition
    /// to the default `h3_index`, `h3_hex` and `resolution`. Map clients use the list to
    /// offer styling by property; it does not change the tiles.
    pub fn with_layer_fields<I, K>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = (K, &'static str)>,
        K: Into<String>,
    {
        for (name, kind) in fields {
            self.layer_fields
                .insert(name.into(), serde_json::Value::from(kind));
        }
        self
    }

    /// Number of sorted runs spilled to disk so far.
    pub fn spilled_runs(&self) -> usize {
        self.runs.len()
    }

    /// Add one feature. Features whose index is not a valid H3 cell are
    /// counted and dropped, as in [`crate::pmtiles::H3PmtilesTiler::export_h3_features`].
    pub fn push(&mut self, feature: H3Feature) -> io::Result<()> {
        self.total_features += 1;
        let Ok(cell) = CellIndex::try_from(feature.h3_index) else {
            return Ok(());
        };
        self.valid_features += 1;
        self.bounds.record_cell(cell);
        let res = u8::from(cell.resolution());
        let zoom = h3_res_to_zoom(res);
        self.min_zoom = self.min_zoom.min(zoom);
        self.max_zoom = self.max_zoom.max(zoom);
        self.coarsest_res = self.coarsest_res.min(res);

        let mut properties = Vec::with_capacity(feature.properties.len());
        for (key, value) in feature.properties {
            properties.push((self.intern(key)?, value));
        }
        let record = SortRecord {
            lat: LatLng::from(cell).lat(),
            h3_index: feature.h3_index,
            properties,
        };
        self.buffer_bytes += record.estimated_bytes();
        self.buffer.push(record);
        if self.buffer_bytes >= self.budget_bytes {
            self.spill()?;
        }
        Ok(())
    }

    fn intern(&mut self, key: Cow<'static, str>) -> io::Result<u16> {
        if let Some(&id) = self.key_ids.get(&key) {
            return Ok(id);
        }
        let id = u16::try_from(self.keys.len())
            .map_err(|_| io::Error::other("more than 65535 distinct property names"))?;
        self.keys.push(key.clone());
        self.key_ids.insert(key, id);
        Ok(id)
    }

    fn sort_buffer(&mut self) {
        self.buffer.sort_unstable_by(SortRecord::order);
    }

    /// Sort the buffer and write it out as one run.
    fn spill(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.sort_buffer();
        let file = match &self.spill_dir {
            Some(dir) => match std::fs::create_dir_all(dir) {
                Ok(()) => tempfile::tempfile_in(dir)?,
                Err(_) => tempfile::tempfile()?,
            },
            None => tempfile::tempfile()?,
        };
        let mut w = BufWriter::with_capacity(1 << 20, file);
        for record in self.buffer.drain(..) {
            record.write_to(&mut w)?;
        }
        let mut file = w.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(0))?;
        self.runs.push(file);
        self.buffer_bytes = 0;
        // Release the capacity too, so memory really drops after a spill.
        self.buffer = Vec::new();
        Ok(())
    }

    /// Build the archive and write it to `path` (atomically, via a temporary
    /// file next to it).
    pub fn finish<P: AsRef<Path>>(self, path: P) -> io::Result<PmtilesExportSummary> {
        let (summary, writer) = self.build()?;
        writer.finish(path)?;
        Ok(summary)
    }

    /// Build the archive and stream it into `out`.
    pub fn finish_into<W: Write>(self, out: &mut W) -> io::Result<PmtilesExportSummary> {
        let (summary, writer) = self.build()?;
        writer.finish_into(out)?;
        Ok(summary)
    }

    /// The default fields plus [`Self::with_layer_fields`]'s, the caller's types winning.
    fn fields_metadata(&self) -> serde_json::Value {
        let mut fields = match build_default_pmtiles_fields() {
            serde_json::Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for (name, kind) in &self.layer_fields {
            fields.insert(name.clone(), kind.clone());
        }
        serde_json::Value::Object(fields)
    }

    /// Merge everything into a ready-to-finish writer.
    fn build(mut self) -> io::Result<(PmtilesExportSummary, PmtilesWriter)> {
        let (min_zoom, max_zoom) = if self.valid_features == 0 {
            (0, 0)
        } else {
            (self.min_zoom, self.max_zoom)
        };
        let bbox = self.bounds.finish([-180.0, -85.0, 180.0, 85.0]);
        let metadata = build_pmtiles_metadata(
            "h3_pmtiles_export",
            "H3 vector hexagon tile pyramid exported by raster_h3",
            min_zoom,
            max_zoom,
            "h3_hexagons",
            "H3 hexagonal vector polygons with attributes",
            self.fields_metadata(),
            None,
        );
        // Spill once more before choosing the writer's directory, so a merge
        // that needs `spill_dir` has already created it.
        if !self.runs.is_empty() {
            self.spill()?;
        }
        let writer_dir = self.spill_dir.as_deref().filter(|d| d.is_dir());
        let mut writer =
            PmtilesWriter::new_in(min_zoom, max_zoom, bbox, metadata.to_string(), writer_dir)?;

        // A cell reaches at most its radius north of its center, so a tile is
        // final once its south edge is that far north of the merge position.
        // The coarsest resolution present bounds the radius; the 2.5 factor
        // matches the raster tilers' margin.
        let margin = if self.valid_features == 0 {
            0.0
        } else {
            2.5 * max_hex_radius_deg(self.coarsest_res)
        };
        let mut accumulator = TilePyramidAccumulator::new(margin);
        let keys = std::mem::take(&mut self.keys);
        let evict_every = self.evict_every.max(1);
        let mut merged = 0usize;
        let mut add = |record: SortRecord, writer: &mut PmtilesWriter| -> io::Result<()> {
            let lat = record.lat;
            add_record(&mut accumulator, &keys, record);
            merged += 1;
            if merged.is_multiple_of(evict_every) {
                accumulator.evict_and_write_tiles(lat, writer)?;
            }
            Ok(())
        };

        if self.runs.is_empty() {
            self.sort_buffer();
            for record in std::mem::take(&mut self.buffer) {
                add(record, &mut writer)?;
            }
        } else {
            let mut readers: Vec<BufReader<File>> = std::mem::take(&mut self.runs)
                .into_iter()
                .map(|f| BufReader::with_capacity(256 << 10, f))
                .collect();
            let mut heap = BinaryHeap::with_capacity(readers.len());
            for (run, reader) in readers.iter_mut().enumerate() {
                if let Some(record) = SortRecord::read_from(reader)? {
                    heap.push(MergeHead { record, run });
                }
            }
            while let Some(MergeHead { record, run }) = heap.pop() {
                if let Some(next) = SortRecord::read_from(&mut readers[run])? {
                    heap.push(MergeHead { record: next, run });
                }
                add(record, &mut writer)?;
            }
        }
        accumulator.flush_all(&mut writer)?;

        let summary = PmtilesExportSummary {
            total_features: self.total_features,
            valid_features: self.valid_features,
            invalid_features_dropped: self.total_features - self.valid_features,
            total_tiles: writer.tile_count(),
            min_zoom,
            max_zoom,
        };
        Ok((summary, writer))
    }
}

/// Add one sorted record to the accumulator, with the default `h3_index`,
/// `h3_hex` and `resolution` properties unless the caller supplied them.
fn add_record(acc: &mut TilePyramidAccumulator, keys: &[Cow<'static, str>], record: SortRecord) {
    // Only valid cells are buffered (checked in `push`).
    let Ok(cell) = CellIndex::try_from(record.h3_index) else {
        return;
    };
    let center: LatLng = cell.into();
    let center_merc = MercatorPoint::from_lat_lng(center.lat(), center.lng());
    let (v_merc, v_count) = cell_boundary_mercator(cell);
    let res = u8::from(cell.resolution());

    let mut properties: Vec<(Cow<'static, str>, MvtValue)> = record
        .properties
        .into_iter()
        .map(|(id, v)| (keys[usize::from(id)].clone(), v))
        .collect();
    if !properties.iter().any(|(k, _)| k == "h3_index") {
        properties.push((Cow::Borrowed("h3_index"), MvtValue::UInt(record.h3_index)));
    }
    if !properties.iter().any(|(k, _)| k == "h3_hex") {
        properties.push((
            Cow::Borrowed("h3_hex"),
            MvtValue::from_hex_u64(record.h3_index),
        ));
    }
    if !properties.iter().any(|(k, _)| k == "resolution") {
        properties.push((Cow::Borrowed("resolution"), MvtValue::UInt(u64::from(res))));
    }
    acc.add_hexagon_mercator(
        record.h3_index,
        center_merc,
        &v_merc[..v_count],
        h3_res_to_zoom(res),
        properties,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_features() -> Vec<H3Feature> {
        // All 343 res-8 descendants of a res-5 cell, plus the res-5 cell and
        // an invalid index, shuffled deterministically so input order is
        // unrelated to latitude.
        let parent = CellIndex::try_from(0x85283473fffffff_u64).unwrap();
        let mut cells: Vec<u64> = parent
            .children(h3o::Resolution::Eight)
            .map(u64::from)
            .collect();
        cells.push(u64::from(parent));
        cells.sort_unstable_by_key(|c| c.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut features: Vec<H3Feature> = cells
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                H3Feature::new(
                    c,
                    vec![
                        ("name", MvtValue::String(format!("cell-{i}"))),
                        ("value", MvtValue::Double(i as f64 * 0.5)),
                        ("flag", MvtValue::Bool(i % 2 == 0)),
                        ("hex", MvtValue::from_hex_u64(c)),
                    ],
                )
            })
            .collect();
        features.push(H3Feature::new::<&str>(1, Vec::new()));
        features
    }

    fn export(budget: usize) -> (PmtilesExportSummary, usize, Vec<u8>) {
        let mut exporter = ExternalFeatureExport::new(budget, None);
        // Evict often so the test covers eviction during the merge.
        exporter.evict_every = 8;
        for f in sample_features() {
            exporter.push(f).unwrap();
        }
        let runs = exporter.spilled_runs();
        let mut out = Vec::new();
        let summary = exporter.finish_into(&mut out).unwrap();
        (summary, runs, out)
    }

    /// The archive's gzipped JSON metadata.
    fn metadata(archive: &[u8]) -> serde_json::Value {
        let offset = u64::from_le_bytes(archive[24..32].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(archive[32..40].try_into().unwrap()) as usize;
        let mut json = String::new();
        flate2::read::GzDecoder::new(&archive[offset..offset + len])
            .read_to_string(&mut json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn layer_fields_are_listed_in_the_metadata() {
        let fields = |exporter: ExternalFeatureExport| {
            let mut exporter = exporter;
            for f in sample_features() {
                exporter.push(f).unwrap();
            }
            let mut out = Vec::new();
            exporter.finish_into(&mut out).unwrap();
            metadata(&out)["vector_layers"][0]["fields"].clone()
        };
        let defaults = fields(ExternalFeatureExport::new(usize::MAX, None));
        assert_eq!(
            defaults,
            serde_json::json!({"h3_index": "Number", "h3_hex": "String", "resolution": "Number"})
        );
        let listed = fields(
            ExternalFeatureExport::new(usize::MAX, None)
                .with_layer_fields([("name", "String"), ("value", "Number"), ("flag", "Boolean")])
                // A caller-supplied `h3_index` keeps the caller's type.
                .with_layer_fields([("h3_index", "String")]),
        );
        assert_eq!(
            listed,
            serde_json::json!({
                "h3_index": "String", "h3_hex": "String", "resolution": "Number",
                "name": "String", "value": "Number", "flag": "Boolean"
            })
        );
    }

    #[test]
    fn spilled_and_in_memory_exports_are_identical() {
        let (in_mem, in_mem_runs, in_mem_bytes) = export(usize::MAX);
        let (spilled, runs, spilled_bytes) = export(4 << 10);
        assert_eq!(in_mem_runs, 0);
        assert!(runs > 4, "expected several runs, got {runs}");
        assert_eq!(in_mem_bytes, spilled_bytes);
        assert_eq!(spilled.total_features, 345);
        assert_eq!(spilled.valid_features, 344);
        assert_eq!(spilled.invalid_features_dropped, 1);
        assert_eq!(spilled.total_tiles, in_mem.total_tiles);
        assert!(spilled.total_tiles > 0);
        assert_eq!((spilled.min_zoom, spilled.max_zoom), (8, 13));
        assert_eq!(&spilled_bytes[..7], b"PMTiles");
    }

    #[test]
    fn matches_legacy_tile_set() {
        // The same features through the original single-pass path (all in
        // one accumulator, flushed at the end) yield the same tile count.
        let mut acc = TilePyramidAccumulator::new(0.0);
        let mut keys = Vec::new();
        let mut key_ids = HashMap::new();
        for f in sample_features() {
            let Ok(cell) = CellIndex::try_from(f.h3_index) else {
                continue;
            };
            let properties = f
                .properties
                .into_iter()
                .map(|(k, v)| {
                    let id = *key_ids.entry(k.clone()).or_insert_with(|| {
                        keys.push(k);
                        (keys.len() - 1) as u16
                    });
                    (id, v)
                })
                .collect();
            let record = SortRecord {
                lat: LatLng::from(cell).lat(),
                h3_index: f.h3_index,
                properties,
            };
            add_record(&mut acc, &keys, record);
        }
        let legacy_tiles = acc.active_tile_count();
        let (summary, _, _) = export(4 << 10);
        assert_eq!(summary.total_tiles, legacy_tiles);
    }

    #[test]
    fn records_round_trip_through_a_run() {
        let record = SortRecord {
            lat: -12.5,
            h3_index: 0x85283473fffffff,
            properties: vec![
                (0, MvtValue::String("héllo".into())),
                (1, MvtValue::from_hex_u64(0x85283473fffffff)),
                (2, MvtValue::Float(1.5)),
                (3, MvtValue::Double(-2.25)),
                (4, MvtValue::Int(-7)),
                (5, MvtValue::UInt(9)),
                (6, MvtValue::Bool(true)),
            ],
        };
        let mut buf = Vec::new();
        record.write_to(&mut buf).unwrap();
        let mut reader = &buf[..];
        let back = SortRecord::read_from(&mut reader).unwrap().unwrap();
        assert_eq!(back.lat, record.lat);
        assert_eq!(back.h3_index, record.h3_index);
        assert_eq!(back.properties.len(), record.properties.len());
        for ((ka, va), (kb, vb)) in back.properties.iter().zip(&record.properties) {
            assert_eq!(ka, kb);
            assert!(va == vb);
        }
        assert!(SortRecord::read_from(&mut reader).unwrap().is_none());
    }

    #[test]
    fn empty_export_is_a_valid_archive() {
        let mut out = Vec::new();
        let summary = ExternalFeatureExport::new(1024, None)
            .finish_into(&mut out)
            .unwrap();
        assert_eq!(summary.total_tiles, 0);
        assert_eq!(&out[..7], b"PMTiles");
    }

    #[test]
    fn spill_dir_is_created_only_when_spilling() {
        let root = tempfile::tempdir().unwrap();
        let run = |budget: usize, name: &str| {
            let dir = root.path().join(name);
            let mut exporter = ExternalFeatureExport::new(budget, Some(&dir));
            for f in sample_features() {
                exporter.push(f).unwrap();
            }
            let runs = exporter.spilled_runs();
            exporter.finish_into(&mut Vec::new()).unwrap();
            (runs, dir.is_dir())
        };
        assert_eq!(run(usize::MAX, "fits"), (0, false));
        let (runs, created) = run(4 << 10, "spills");
        assert!(runs > 0 && created);
    }
}

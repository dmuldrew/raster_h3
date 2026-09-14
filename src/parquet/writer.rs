//! Native Parquet Streaming Writer for H3 Raster Hexification
//!
//! Streams aggregated H3 records directly from `MultiScanHorizonStreamer` or
//! `MultiCategoricalHorizonStreamer` into Snappy- or ZSTD-compressed Parquet row groups
//! without crossing DuckDB SQL/C-FFI boundaries.
//!
//! Employs lock-free double-buffered channel streaming: the aggregation stream drains
//! into the current buffer while a background thread sorts and flushes the previous buffer
//! to disk with zero pipeline stalls.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;

use h3o::{CellIndex, LatLng};
use parquet::basic::{Compression, Encoding};
use parquet::column::writer::ColumnWriter;
use parquet::data_type::ByteArray;
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::ColumnPath;

use parquet::file::metadata::KeyValue;
use serde_json::json;

use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer,
};
use crate::functions::fast_hex::fast_hex_u64;
use crate::functions::wkb::cell_to_wkb;
use crate::raster::geotiff::GeoTiffStreamReader;

#[derive(Debug, Clone)]
pub struct ParquetExportConfig {
    pub compact: bool,
    pub compression: Compression,
    pub row_group_size: usize,
    pub is_categorical: bool,
    pub geoparquet: bool,
}

impl Default for ParquetExportConfig {
    fn default() -> Self {
        Self {
            compact: true,
            compression: Compression::SNAPPY,
            row_group_size: 131_072,
            is_categorical: false,
            geoparquet: false,
        }
    }
}

/// Build an OGC GeoParquet 1.1 compliant JSON metadata object for Parquet FileMetaData key-value store
pub fn build_geoparquet_metadata(primary_column: &str, bbox: [f64; 4]) -> String {
    let geo = json!({
        "version": "1.1.0",
        "primary_column": primary_column,
        "columns": {
            primary_column: {
                "encoding": "WKB",
                "geometry_types": ["Polygon"],
                "crs": {
                    "$schema": "https://proj.org/schemas/v0.7/projjson.schema.json",
                    "type": "GeographicCRS",
                    "name": "WGS 84 (CRS84)",
                    "datum_ensemble": {
                        "name": "World Geodetic System 1984 ensemble",
                        "members": [
                            { "name": "World Geodetic System 1984 (Transit)" },
                            { "name": "World Geodetic System 1984 (G730)" },
                            { "name": "World Geodetic System 1984 (G873)" },
                            { "name": "World Geodetic System 1984 (G1150)" },
                            { "name": "World Geodetic System 1984 (G1674)" },
                            { "name": "World Geodetic System 1984 (G1762)" },
                            { "name": "World Geodetic System 1984 (G2139)" }
                        ],
                        "ellipsoid": {
                            "name": "WGS 84",
                            "semi_major_axis": 6378137.0,
                            "inverse_flattening": 298.257223563
                        },
                        "accuracy": "2.0"
                    },
                    "coordinate_system": {
                        "subtype": "ellipsoidal",
                        "axis": [
                            {
                                "name": "Geodetic longitude",
                                "abbreviation": "Lon",
                                "direction": "east",
                                "unit": "degree"
                            },
                            {
                                "name": "Geodetic latitude",
                                "abbreviation": "Lat",
                                "direction": "north",
                                "unit": "degree"
                            }
                        ]
                    },
                    "id": {
                        "authority": "OGC",
                        "code": "CRS84"
                    }
                },
                "bbox": [bbox[0], bbox[1], bbox[2], bbox[3]],
                "edges": "planar"
            }
        }
    });
    geo.to_string()
}

/// Common interface for horizon streamers supplying records to the Parquet pipeline
pub trait ParquetStreamer {
    type Record;

    /// Drain up to `max_rows` completed records directly into a consumer closure
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> usize
    where
        F: FnMut(usize, Self::Record);

    /// Optional spatial bounding box in WGS84 [min_lon, min_lat, max_lon, max_lat]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        None
    }
}

impl ParquetStreamer for MultiScanHorizonStreamer {
    type Record = MultiContinuousRecord;

    #[inline(always)]
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> usize
    where
        F: FnMut(usize, Self::Record),
    {
        self.drain_completed_into(max_rows, consumer)
    }

    #[inline]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        Some(self.mosaic.mosaic_bounds_wgs84)
    }
}

impl ParquetStreamer for MultiCategoricalHorizonStreamer {
    type Record = MultiCategoricalRecord;

    #[inline(always)]
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> usize
    where
        F: FnMut(usize, Self::Record),
    {
        self.drain_completed_into(max_rows, consumer)
    }

    #[inline]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        Some(self.mosaic.mosaic_bounds_wgs84)
    }
}

#[inline]
fn compute_sort_permutation(h3_indices: &[i64]) -> Option<Vec<usize>> {
    let n = h3_indices.len();
    if n <= 1 {
        return None;
    }
    let mut already_sorted = true;
    for i in 1..n {
        if h3_indices[i] < h3_indices[i - 1] {
            already_sorted = false;
            break;
        }
    }
    if already_sorted {
        return None;
    }
    let mut perm: Vec<usize> = (0..n).collect();
    perm.sort_unstable_by_key(|&i| h3_indices[i]);
    Some(perm)
}

#[inline]
fn reorder_by_perm<T: Copy>(vec: &mut Vec<T>, perm: &[usize]) {
    let mut reordered = Vec::with_capacity(perm.len());
    for &i in perm {
        reordered.push(vec[i]);
    }
    *vec = reordered;
}

#[inline]
fn reorder_by_perm_take<T: Default>(vec: &mut Vec<T>, perm: &[usize]) {
    let mut reordered = Vec::with_capacity(perm.len());
    for &i in perm {
        reordered.push(std::mem::take(&mut vec[i]));
    }
    *vec = reordered;
}

#[inline(always)]
fn write_i64_column(
    row_group_writer: &mut SerializedRowGroupWriter<'_, File>,
    values: &[i64],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(mut col_writer) = row_group_writer.next_column()? {
        if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
            typed.write_batch(values, None, None)?;
        }
        col_writer.close()?;
    }
    Ok(())
}

#[inline(always)]
fn write_f64_column(
    row_group_writer: &mut SerializedRowGroupWriter<'_, File>,
    values: &[f64],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(mut col_writer) = row_group_writer.next_column()? {
        if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
            typed.write_batch(values, None, None)?;
        }
        col_writer.close()?;
    }
    Ok(())
}

#[inline(always)]
fn write_byte_array_column(
    row_group_writer: &mut SerializedRowGroupWriter<'_, File>,
    values: &[ByteArray],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some(mut col_writer) = row_group_writer.next_column()? {
        if let ColumnWriter::ByteArrayColumnWriter(ref mut typed) = col_writer.untyped() {
            typed.write_batch(values, None, None)?;
        }
        col_writer.close()?;
    }
    Ok(())
}

///// Common trait for Parquet row group column buffers (continuous and categorical)
pub trait ParquetRowGroupBuffer: Sized + Send + 'static {
    type Record;

    /// Allocate a new buffer with target row group capacity and options
    fn with_capacity_and_options(capacity: usize, compact: bool, geoparquet: bool) -> Self;

    /// Allocate a new buffer with target row group capacity (default geoparquet: false)
    fn with_capacity(capacity: usize, compact: bool) -> Self {
        Self::with_capacity_and_options(capacity, compact, false)
    }

    /// Push a single stream record into column vectors
    fn push_record(&mut self, record: Self::Record);

    /// Current number of rows in the buffer
    fn len(&self) -> usize;

    /// Check if the buffer is empty
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all column vectors for buffer recycling
    fn clear(&mut self);

    /// Sort all columns by H3 index in-place (if not already sorted)
    fn sort_by_h3_index(&mut self);

    /// Write all columns as a new row group in the Parquet file
    fn flush_to_row_group(
        &self,
        writer: &mut SerializedFileWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Return schema definition string for this buffer type
    fn schema_message(compact: bool, geoparquet: bool) -> &'static str;
}

/// Columnar buffer for continuous raster aggregation row groups
pub struct ContinuousRowGroupBuffer {
    compact: bool,
    geoparquet: bool,
    h3_indices: Vec<i64>,
    geometries: Vec<ByteArray>,
    min_values: Vec<f64>,
    max_values: Vec<f64>,
    sum_values: Vec<f64>,
    avg_values: Vec<f64>,
    pixel_counts: Vec<f64>,
    h3_hexes: Vec<ByteArray>,
    lats: Vec<f64>,
    lngs: Vec<f64>,
}

impl ParquetRowGroupBuffer for ContinuousRowGroupBuffer {
    type Record = MultiContinuousRecord;

    fn with_capacity_and_options(capacity: usize, compact: bool, geoparquet: bool) -> Self {
        Self {
            compact,
            geoparquet,
            h3_indices: Vec::with_capacity(capacity),
            geometries: if geoparquet { Vec::with_capacity(capacity) } else { Vec::new() },
            min_values: Vec::with_capacity(capacity),
            max_values: Vec::with_capacity(capacity),
            sum_values: Vec::with_capacity(capacity),
            avg_values: Vec::with_capacity(capacity),
            pixel_counts: Vec::with_capacity(capacity),
            h3_hexes: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
            lats: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
            lngs: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
        }
    }

    #[inline(always)]
    fn push_record(&mut self, record: Self::Record) {
        let cell_u64 = record.h3_index;
        let acc = record.accumulator;
        self.h3_indices.push(cell_u64 as i64);

        if self.geoparquet {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                let mut wkb_buf = [0u8; 128];
                let len = cell_to_wkb(cell, &mut wkb_buf);
                self.geometries.push(ByteArray::from(&wkb_buf[..len]));
            } else {
                self.geometries.push(ByteArray::from(Vec::new()));
            }
        }

        self.min_values.push(acc.min);
        self.max_values.push(acc.max);
        self.sum_values.push(acc.sum);
        self.avg_values.push(acc.mean());
        self.pixel_counts.push(acc.count);

        if !self.compact {
            let mut hex_buf = [0u8; 16];
            let hex_slice = fast_hex_u64(cell_u64, &mut hex_buf);
            self.h3_hexes.push(ByteArray::from(hex_slice));

            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                let ll: LatLng = cell.into();
                self.lats.push(ll.lat());
                self.lngs.push(ll.lng());
            } else {
                self.lats.push(0.0);
                self.lngs.push(0.0);
            }
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.h3_indices.len()
    }

    fn clear(&mut self) {
        self.h3_indices.clear();
        if self.geoparquet {
            self.geometries.clear();
        }
        self.min_values.clear();
        self.max_values.clear();
        self.sum_values.clear();
        self.avg_values.clear();
        self.pixel_counts.clear();
        if !self.compact {
            self.h3_hexes.clear();
            self.lats.clear();
            self.lngs.clear();
        }
    }

    fn sort_by_h3_index(&mut self) {
        let perm = match compute_sort_permutation(&self.h3_indices) {
            Some(p) => p,
            None => return,
        };

        reorder_by_perm(&mut self.h3_indices, &perm);
        if self.geoparquet {
            reorder_by_perm_take(&mut self.geometries, &perm);
        }
        if !self.compact {
            reorder_by_perm_take(&mut self.h3_hexes, &perm);
        }
        reorder_by_perm(&mut self.min_values, &perm);
        reorder_by_perm(&mut self.max_values, &perm);
        reorder_by_perm(&mut self.sum_values, &perm);
        reorder_by_perm(&mut self.avg_values, &perm);
        reorder_by_perm(&mut self.pixel_counts, &perm);
        if !self.compact {
            reorder_by_perm(&mut self.lats, &perm);
            reorder_by_perm(&mut self.lngs, &perm);
        }
    }

    fn flush_to_row_group(
        &self,
        writer: &mut SerializedFileWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        write_i64_column(&mut row_group_writer, &self.h3_indices)?;
        if !self.compact {
            write_byte_array_column(&mut row_group_writer, &self.h3_hexes)?;
        }
        if self.geoparquet {
            write_byte_array_column(&mut row_group_writer, &self.geometries)?;
        }
        write_f64_column(&mut row_group_writer, &self.min_values)?;
        write_f64_column(&mut row_group_writer, &self.max_values)?;
        write_f64_column(&mut row_group_writer, &self.sum_values)?;
        write_f64_column(&mut row_group_writer, &self.avg_values)?;
        write_f64_column(&mut row_group_writer, &self.pixel_counts)?;
        if !self.compact {
            write_f64_column(&mut row_group_writer, &self.lats)?;
            write_f64_column(&mut row_group_writer, &self.lngs)?;
        }

        row_group_writer.close()?;
        Ok(())
    }

    fn schema_message(compact: bool, geoparquet: bool) -> &'static str {
        match (compact, geoparquet) {
            (true, false) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED DOUBLE min_value;
                    REQUIRED DOUBLE max_value;
                    REQUIRED DOUBLE sum_value;
                    REQUIRED DOUBLE avg_value;
                    REQUIRED DOUBLE pixel_count;
                }
            ",
            (true, true) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY geometry;
                    REQUIRED DOUBLE min_value;
                    REQUIRED DOUBLE max_value;
                    REQUIRED DOUBLE sum_value;
                    REQUIRED DOUBLE avg_value;
                    REQUIRED DOUBLE pixel_count;
                }
            ",
            (false, false) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY h3_hex (UTF8);
                    REQUIRED DOUBLE min_value;
                    REQUIRED DOUBLE max_value;
                    REQUIRED DOUBLE sum_value;
                    REQUIRED DOUBLE avg_value;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED DOUBLE lat;
                    REQUIRED DOUBLE lng;
                }
            ",
            (false, true) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY h3_hex (UTF8);
                    REQUIRED BYTE_ARRAY geometry;
                    REQUIRED DOUBLE min_value;
                    REQUIRED DOUBLE max_value;
                    REQUIRED DOUBLE sum_value;
                    REQUIRED DOUBLE avg_value;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED DOUBLE lat;
                    REQUIRED DOUBLE lng;
                }
            ",
        }
    }
}

/// Columnar buffer for categorical raster aggregation row groups
pub struct CategoricalRowGroupBuffer {
    compact: bool,
    geoparquet: bool,
    h3_indices: Vec<i64>,
    geometries: Vec<ByteArray>,
    majorities: Vec<i64>,
    fractions: Vec<f64>,
    pixel_counts: Vec<f64>,
    distinct_classes: Vec<i64>,
    entropies: Vec<f64>,
    h3_hexes: Vec<ByteArray>,
    lats: Vec<f64>,
    lngs: Vec<f64>,
}

impl ParquetRowGroupBuffer for CategoricalRowGroupBuffer {
    type Record = MultiCategoricalRecord;

    fn with_capacity_and_options(capacity: usize, compact: bool, geoparquet: bool) -> Self {
        Self {
            compact,
            geoparquet,
            h3_indices: Vec::with_capacity(capacity),
            geometries: if geoparquet { Vec::with_capacity(capacity) } else { Vec::new() },
            majorities: Vec::with_capacity(capacity),
            fractions: Vec::with_capacity(capacity),
            pixel_counts: Vec::with_capacity(capacity),
            distinct_classes: Vec::with_capacity(capacity),
            entropies: Vec::with_capacity(capacity),
            h3_hexes: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
            lats: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
            lngs: if compact { Vec::new() } else { Vec::with_capacity(capacity) },
        }
    }

    #[inline(always)]
    fn push_record(&mut self, record: Self::Record) {
        let cell_u64 = record.h3_index;
        let acc = record.accumulator;
        let (maj_cat, _maj_cnt, maj_frac) = acc.majority();
        self.h3_indices.push(cell_u64 as i64);

        if self.geoparquet {
            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                let mut wkb_buf = [0u8; 128];
                let len = cell_to_wkb(cell, &mut wkb_buf);
                self.geometries.push(ByteArray::from(&wkb_buf[..len]));
            } else {
                self.geometries.push(ByteArray::from(Vec::new()));
            }
        }

        self.majorities.push(maj_cat);
        self.fractions.push(maj_frac);
        self.pixel_counts.push(acc.total_count);
        self.distinct_classes.push(acc.unique_classes() as i64);
        self.entropies.push(acc.shannon_entropy());

        if !self.compact {
            let mut hex_buf = [0u8; 16];
            let hex_slice = fast_hex_u64(cell_u64, &mut hex_buf);
            self.h3_hexes.push(ByteArray::from(hex_slice));

            if let Ok(cell) = CellIndex::try_from(cell_u64) {
                let ll: LatLng = cell.into();
                self.lats.push(ll.lat());
                self.lngs.push(ll.lng());
            } else {
                self.lats.push(0.0);
                self.lngs.push(0.0);
            }
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.h3_indices.len()
    }

    fn clear(&mut self) {
        self.h3_indices.clear();
        if self.geoparquet {
            self.geometries.clear();
        }
        self.majorities.clear();
        self.fractions.clear();
        self.pixel_counts.clear();
        self.distinct_classes.clear();
        self.entropies.clear();
        if !self.compact {
            self.h3_hexes.clear();
            self.lats.clear();
            self.lngs.clear();
        }
    }

    fn sort_by_h3_index(&mut self) {
        let perm = match compute_sort_permutation(&self.h3_indices) {
            Some(p) => p,
            None => return,
        };

        reorder_by_perm(&mut self.h3_indices, &perm);
        if self.geoparquet {
            reorder_by_perm_take(&mut self.geometries, &perm);
        }
        if !self.compact {
            reorder_by_perm_take(&mut self.h3_hexes, &perm);
        }
        reorder_by_perm(&mut self.majorities, &perm);
        reorder_by_perm(&mut self.fractions, &perm);
        reorder_by_perm(&mut self.pixel_counts, &perm);
        reorder_by_perm(&mut self.distinct_classes, &perm);
        reorder_by_perm(&mut self.entropies, &perm);
        if !self.compact {
            reorder_by_perm(&mut self.lats, &perm);
            reorder_by_perm(&mut self.lngs, &perm);
        }
    }

    fn flush_to_row_group(
        &self,
        writer: &mut SerializedFileWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        write_i64_column(&mut row_group_writer, &self.h3_indices)?;
        if !self.compact {
            write_byte_array_column(&mut row_group_writer, &self.h3_hexes)?;
        }
        if self.geoparquet {
            write_byte_array_column(&mut row_group_writer, &self.geometries)?;
        }
        write_i64_column(&mut row_group_writer, &self.majorities)?;
        write_f64_column(&mut row_group_writer, &self.fractions)?;
        write_f64_column(&mut row_group_writer, &self.pixel_counts)?;
        write_i64_column(&mut row_group_writer, &self.distinct_classes)?;
        write_f64_column(&mut row_group_writer, &self.entropies)?;
        if !self.compact {
            write_f64_column(&mut row_group_writer, &self.lats)?;
            write_f64_column(&mut row_group_writer, &self.lngs)?;
        }

        row_group_writer.close()?;
        Ok(())
    }

    fn schema_message(compact: bool, geoparquet: bool) -> &'static str {
        match (compact, geoparquet) {
            (true, false) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED INT64 majority;
                    REQUIRED DOUBLE majority_fraction;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED INT64 distinct_classes;
                    REQUIRED DOUBLE entropy;
                }
            ",
            (true, true) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY geometry;
                    REQUIRED INT64 majority;
                    REQUIRED DOUBLE majority_fraction;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED INT64 distinct_classes;
                    REQUIRED DOUBLE entropy;
                }
            ",
            (false, false) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY h3_hex (UTF8);
                    REQUIRED INT64 majority;
                    REQUIRED DOUBLE majority_fraction;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED INT64 distinct_classes;
                    REQUIRED DOUBLE entropy;
                    REQUIRED DOUBLE lat;
                    REQUIRED DOUBLE lng;
                }
            ",
            (false, true) => "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED BYTE_ARRAY h3_hex (UTF8);
                    REQUIRED BYTE_ARRAY geometry;
                    REQUIRED INT64 majority;
                    REQUIRED DOUBLE majority_fraction;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED INT64 distinct_classes;
                    REQUIRED DOUBLE entropy;
                    REQUIRED DOUBLE lat;
                    REQUIRED DOUBLE lng;
                }
            ",
        }
    }
}

/// Generic double-buffered streaming pipeline from any horizon streamer to Parquet with progress reporting
pub fn run_parquet_streaming_pipeline_with_progress<S, B, P, F>(
    mut streamer: S,
    parquet_path: P,
    parquet_config: ParquetExportConfig,
    mut progress_callback: F,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
where
    S: ParquetStreamer,
    B: ParquetRowGroupBuffer<Record = S::Record>,
    P: AsRef<Path>,
    F: FnMut(usize),
{
    let compact = parquet_config.compact;
    let geoparquet = parquet_config.geoparquet;
    let row_group_size = parquet_config.row_group_size.max(1);

    let message_type = B::schema_message(compact, geoparquet);
    let schema = Arc::new(parse_message_type(message_type)?);

    let mut props_builder = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_column_encoding(ColumnPath::from("h3_index"), Encoding::DELTA_BINARY_PACKED)
        .set_compression(parquet_config.compression);

    if geoparquet {
        let bbox = streamer.bounds_wgs84().unwrap_or([-180.0, -90.0, 180.0, 90.0]);
        let geo_json = build_geoparquet_metadata("geometry", bbox);
        props_builder = props_builder.set_key_value_metadata(Some(vec![
            KeyValue::new("geo".to_string(), geo_json),
        ]));
    }

    let props = Arc::new(props_builder.build());

    if let Some(parent) = parquet_path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let file = File::create(&parquet_path)?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;

    let (writer_tx, writer_rx) = sync_channel::<B>(2);
    let (recycle_tx, recycle_rx) = sync_channel::<B>(2);

    let buf1 = B::with_capacity_and_options(row_group_size, compact, geoparquet);
    let buf2 = B::with_capacity_and_options(row_group_size, compact, geoparquet);
    let _ = recycle_tx.send(buf2);

    let writer_handle = thread::spawn(move || -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut total_rows = 0usize;
        while let Ok(mut buf) = writer_rx.recv() {
            if !buf.is_empty() {
                buf.sort_by_h3_index();
                let count = buf.len();
                buf.flush_to_row_group(&mut writer)?;
                total_rows += count;
                buf.clear();
                let _ = recycle_tx.send(buf);
            }
        }
        writer.close()?;
        Ok(total_rows)
    });

    let mut current_buf = buf1;
    let mut total_drained = 0usize;

    loop {
        let space_left = row_group_size.saturating_sub(current_buf.len()).max(1);
        let drained = streamer.drain_completed_into(space_left, |_, record| {
            current_buf.push_record(record);
        });
        total_drained += drained;
        if drained > 0 {
            progress_callback(total_drained);
        }

        if current_buf.len() >= row_group_size {
            if writer_tx.send(current_buf).is_err() {
                break;
            }
            current_buf = match recycle_rx.recv() {
                Ok(b) => b,
                Err(_) => B::with_capacity_and_options(row_group_size, compact, geoparquet),
            };
        }

        if drained == 0 {
            if !current_buf.is_empty() {
                let _ = writer_tx.send(current_buf);
            }
            break;
        }
    }

    drop(writer_tx);
    let total_hexagons = match writer_handle.join() {
        Ok(res) => res?,
        Err(_) => return Err("Background Parquet writer thread panicked".into()),
    };
    Ok(total_hexagons)
}

/// Generic double-buffered streaming pipeline from any horizon streamer to Parquet
pub fn run_parquet_streaming_pipeline<S, B, P>(
    streamer: S,
    parquet_path: P,
    parquet_config: ParquetExportConfig,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
where
    S: ParquetStreamer,
    B: ParquetRowGroupBuffer<Record = S::Record>,
    P: AsRef<Path>,
{
    run_parquet_streaming_pipeline_with_progress::<S, B, P, _>(streamer, parquet_path, parquet_config, |_| {})
}

pub struct H3ParquetWriter;

impl H3ParquetWriter {
    /// Convenience helper to run continuous or categorical GeoTIFF/Mosaic-to-Parquet pipeline
    pub fn process_raster_source_to_parquet<P1: AsRef<Path>, P2: AsRef<Path>>(
        source: P1,
        parquet_path: P2,
        config: MultiResolutionConfig,
        parquet_config: ParquetExportConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let source_str = source.as_ref().to_string_lossy();
        let resolved_paths = crate::raster::mosaic::resolve_raster_sources(&source_str)?;

        if parquet_config.is_categorical {
            if resolved_paths.len() == 1
                && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or(""))
            {
                let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
                let streamer = MultiCategoricalHorizonStreamer::new(reader, &config)?;
                Self::write_categorical_streamer_to_parquet(streamer, parquet_path, parquet_config)
            } else {
                let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                    &resolved_paths,
                    config.bbox,
                    config.custom_crs.as_deref(),
                    config.overlap_rule,
                )?);
                let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)?;
                Self::write_categorical_streamer_to_parquet(streamer, parquet_path, parquet_config)
            }
        } else {
            if resolved_paths.len() == 1
                && !crate::raster::http_range::is_remote_url(resolved_paths[0].to_str().unwrap_or(""))
            {
                let reader = GeoTiffStreamReader::open(&resolved_paths[0])?;
                let streamer = MultiScanHorizonStreamer::new(reader, &config)?;
                Self::write_continuous_streamer_to_parquet(streamer, parquet_path, parquet_config)
            } else {
                let mosaic = std::sync::Arc::new(crate::raster::mosaic::MosaicReader::open(
                    &resolved_paths,
                    config.bbox,
                    config.custom_crs.as_deref(),
                    config.overlap_rule,
                )?);
                let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)?;
                Self::write_continuous_streamer_to_parquet(streamer, parquet_path, parquet_config)
            }
        }
    }

    /// Stream continuous raster aggregation directly into Parquet with progress reporting
    pub fn write_continuous_streamer_to_parquet_with_progress<P: AsRef<Path>, F: FnMut(usize)>(
        streamer: MultiScanHorizonStreamer,
        parquet_path: P,
        parquet_config: ParquetExportConfig,
        progress: F,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        run_parquet_streaming_pipeline_with_progress::<_, ContinuousRowGroupBuffer, _, F>(
            streamer,
            parquet_path,
            parquet_config,
            progress,
        )
    }

    /// Stream continuous raster aggregation directly into Parquet
    pub fn write_continuous_streamer_to_parquet<P: AsRef<Path>>(
        streamer: MultiScanHorizonStreamer,
        parquet_path: P,
        parquet_config: ParquetExportConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        run_parquet_streaming_pipeline::<_, ContinuousRowGroupBuffer, _>(
            streamer,
            parquet_path,
            parquet_config,
        )
    }

    /// Stream categorical raster aggregation directly into Parquet
    pub fn write_categorical_streamer_to_parquet<P: AsRef<Path>>(
        streamer: MultiCategoricalHorizonStreamer,
        parquet_path: P,
        parquet_config: ParquetExportConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        run_parquet_streaming_pipeline::<_, CategoricalRowGroupBuffer, _>(
            streamer,
            parquet_path,
            parquet_config,
        )
    }
}

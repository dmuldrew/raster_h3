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
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::ColumnPath;

use crate::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiCategoricalRecord, MultiContinuousRecord,
    MultiResolutionConfig, MultiScanHorizonStreamer,
};
use crate::functions::fast_hex::fast_hex_u64;
use crate::raster::geotiff::GeoTiffStreamReader;

#[derive(Debug, Clone)]
pub struct ParquetExportConfig {
    pub compact: bool,
    pub compression: Compression,
    pub row_group_size: usize,
    pub is_categorical: bool,
}

impl Default for ParquetExportConfig {
    fn default() -> Self {
        Self {
            compact: true,
            compression: Compression::SNAPPY,
            row_group_size: 131_072,
            is_categorical: false,
        }
    }
}

/// Common interface for horizon streamers supplying records to the Parquet pipeline
pub trait ParquetStreamer {
    type Record;

    /// Drain up to `max_rows` completed records directly into a consumer closure
    fn drain_completed_into<F>(&mut self, max_rows: usize, consumer: F) -> usize
    where
        F: FnMut(usize, Self::Record);
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
}

/// Common trait for Parquet row group column buffers (continuous and categorical)
pub trait ParquetRowGroupBuffer: Send + 'static {
    type Record;

    /// Allocate a new buffer with target row group capacity
    fn with_capacity(capacity: usize, compact: bool) -> Self;

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
    fn schema_message(compact: bool) -> &'static str;
}

/// Columnar buffer for continuous raster aggregation row groups
pub struct ContinuousRowGroupBuffer {
    compact: bool,
    h3_indices: Vec<i64>,
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

    fn with_capacity(capacity: usize, compact: bool) -> Self {
        Self {
            compact,
            h3_indices: Vec::with_capacity(capacity),
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
        let n = self.h3_indices.len();
        if n <= 1 {
            return;
        }

        let mut already_sorted = true;
        for i in 1..n {
            if self.h3_indices[i] < self.h3_indices[i - 1] {
                already_sorted = false;
                break;
            }
        }
        if already_sorted {
            return;
        }

        let mut perm: Vec<usize> = (0..n).collect();
        perm.sort_unstable_by_key(|&i| self.h3_indices[i]);

        let mut sorted_indices = Vec::with_capacity(n);
        let mut sorted_min = Vec::with_capacity(n);
        let mut sorted_max = Vec::with_capacity(n);
        let mut sorted_sum = Vec::with_capacity(n);
        let mut sorted_avg = Vec::with_capacity(n);
        let mut sorted_cnt = Vec::with_capacity(n);
        let mut sorted_hexes = if self.compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lats = if self.compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lngs = if self.compact { Vec::new() } else { Vec::with_capacity(n) };

        for &i in &perm {
            sorted_indices.push(self.h3_indices[i]);
            sorted_min.push(self.min_values[i]);
            sorted_max.push(self.max_values[i]);
            sorted_sum.push(self.sum_values[i]);
            sorted_avg.push(self.avg_values[i]);
            sorted_cnt.push(self.pixel_counts[i]);
            if !self.compact {
                sorted_hexes.push(std::mem::replace(&mut self.h3_hexes[i], ByteArray::from("")));
                sorted_lats.push(self.lats[i]);
                sorted_lngs.push(self.lngs[i]);
            }
        }

        self.h3_indices = sorted_indices;
        self.min_values = sorted_min;
        self.max_values = sorted_max;
        self.sum_values = sorted_sum;
        self.avg_values = sorted_avg;
        self.pixel_counts = sorted_cnt;
        if !self.compact {
            self.h3_hexes = sorted_hexes;
            self.lats = sorted_lats;
            self.lngs = sorted_lngs;
        }
    }

    fn flush_to_row_group(
        &self,
        writer: &mut SerializedFileWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        // Col 0: h3_index
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.h3_indices, None, None)?;
            }
            col_writer.close()?;
        }

        if !self.compact {
            // Col 1: h3_hex
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::ByteArrayColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.h3_hexes, None, None)?;
                }
                col_writer.close()?;
            }
        }

        // min_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.min_values, None, None)?;
            }
            col_writer.close()?;
        }

        // max_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.max_values, None, None)?;
            }
            col_writer.close()?;
        }

        // sum_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.sum_values, None, None)?;
            }
            col_writer.close()?;
        }

        // avg_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.avg_values, None, None)?;
            }
            col_writer.close()?;
        }

        // pixel_count
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.pixel_counts, None, None)?;
            }
            col_writer.close()?;
        }

        if !self.compact {
            // lat
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.lats, None, None)?;
                }
                col_writer.close()?;
            }

            // lng
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.lngs, None, None)?;
                }
                col_writer.close()?;
            }
        }

        row_group_writer.close()?;
        Ok(())
    }

    fn schema_message(compact: bool) -> &'static str {
        if compact {
            "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED DOUBLE min_value;
                    REQUIRED DOUBLE max_value;
                    REQUIRED DOUBLE sum_value;
                    REQUIRED DOUBLE avg_value;
                    REQUIRED DOUBLE pixel_count;
                }
            "
        } else {
            "
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
            "
        }
    }
}

/// Columnar buffer for categorical raster aggregation row groups
pub struct CategoricalRowGroupBuffer {
    compact: bool,
    h3_indices: Vec<i64>,
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

    fn with_capacity(capacity: usize, compact: bool) -> Self {
        Self {
            compact,
            h3_indices: Vec::with_capacity(capacity),
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
        let n = self.h3_indices.len();
        if n <= 1 {
            return;
        }

        let mut already_sorted = true;
        for i in 1..n {
            if self.h3_indices[i] < self.h3_indices[i - 1] {
                already_sorted = false;
                break;
            }
        }
        if already_sorted {
            return;
        }

        let mut perm: Vec<usize> = (0..n).collect();
        perm.sort_unstable_by_key(|&i| self.h3_indices[i]);

        let mut sorted_indices = Vec::with_capacity(n);
        let mut sorted_majorities = Vec::with_capacity(n);
        let mut sorted_fractions = Vec::with_capacity(n);
        let mut sorted_pixel_counts = Vec::with_capacity(n);
        let mut sorted_distinct = Vec::with_capacity(n);
        let mut sorted_entropies = Vec::with_capacity(n);
        let mut sorted_hexes = if self.compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lats = if self.compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lngs = if self.compact { Vec::new() } else { Vec::with_capacity(n) };

        for &i in &perm {
            sorted_indices.push(self.h3_indices[i]);
            sorted_majorities.push(self.majorities[i]);
            sorted_fractions.push(self.fractions[i]);
            sorted_pixel_counts.push(self.pixel_counts[i]);
            sorted_distinct.push(self.distinct_classes[i]);
            sorted_entropies.push(self.entropies[i]);
            if !self.compact {
                sorted_hexes.push(std::mem::replace(&mut self.h3_hexes[i], ByteArray::from("")));
                sorted_lats.push(self.lats[i]);
                sorted_lngs.push(self.lngs[i]);
            }
        }

        self.h3_indices = sorted_indices;
        self.majorities = sorted_majorities;
        self.fractions = sorted_fractions;
        self.pixel_counts = sorted_pixel_counts;
        self.distinct_classes = sorted_distinct;
        self.entropies = sorted_entropies;
        if !self.compact {
            self.h3_hexes = sorted_hexes;
            self.lats = sorted_lats;
            self.lngs = sorted_lngs;
        }
    }

    fn flush_to_row_group(
        &self,
        writer: &mut SerializedFileWriter<File>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        // Col 0: h3_index
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.h3_indices, None, None)?;
            }
            col_writer.close()?;
        }

        if !self.compact {
            // Col 1: h3_hex
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::ByteArrayColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.h3_hexes, None, None)?;
                }
                col_writer.close()?;
            }
        }

        // majority
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.majorities, None, None)?;
            }
            col_writer.close()?;
        }

        // majority_fraction
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.fractions, None, None)?;
            }
            col_writer.close()?;
        }

        // pixel_count
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.pixel_counts, None, None)?;
            }
            col_writer.close()?;
        }

        // distinct_classes
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.distinct_classes, None, None)?;
            }
            col_writer.close()?;
        }

        // entropy
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(&self.entropies, None, None)?;
            }
            col_writer.close()?;
        }

        if !self.compact {
            // lat
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.lats, None, None)?;
                }
                col_writer.close()?;
            }

            // lng
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(&self.lngs, None, None)?;
                }
                col_writer.close()?;
            }
        }

        row_group_writer.close()?;
        Ok(())
    }

    fn schema_message(compact: bool) -> &'static str {
        if compact {
            "
                message schema {
                    REQUIRED INT64 h3_index;
                    REQUIRED INT64 majority;
                    REQUIRED DOUBLE majority_fraction;
                    REQUIRED DOUBLE pixel_count;
                    REQUIRED INT64 distinct_classes;
                    REQUIRED DOUBLE entropy;
                }
            "
        } else {
            "
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
            "
        }
    }
}

/// Generic double-buffered streaming pipeline from any horizon streamer to Parquet
pub fn run_parquet_streaming_pipeline<S, B, P>(
    mut streamer: S,
    parquet_path: P,
    parquet_config: ParquetExportConfig,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>>
where
    S: ParquetStreamer,
    B: ParquetRowGroupBuffer<Record = S::Record>,
    P: AsRef<Path>,
{
    let compact = parquet_config.compact;
    let row_group_size = parquet_config.row_group_size.max(1);

    let message_type = B::schema_message(compact);
    let schema = Arc::new(parse_message_type(message_type)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_2_0)
            .set_column_encoding(ColumnPath::from("h3_index"), Encoding::DELTA_BINARY_PACKED)
            .set_compression(parquet_config.compression)
            .build(),
    );

    if let Some(parent) = parquet_path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let file = File::create(&parquet_path)?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;

    let (writer_tx, writer_rx) = sync_channel::<B>(2);
    let (recycle_tx, recycle_rx) = sync_channel::<B>(2);

    let buf1 = B::with_capacity(row_group_size, compact);
    let buf2 = B::with_capacity(row_group_size, compact);
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

    loop {
        let space_left = row_group_size.saturating_sub(current_buf.len()).max(1);
        let drained = streamer.drain_completed_into(space_left, |_, record| {
            current_buf.push_record(record);
        });

        if current_buf.len() >= row_group_size {
            if writer_tx.send(current_buf).is_err() {
                break;
            }
            current_buf = match recycle_rx.recv() {
                Ok(b) => b,
                Err(_) => B::with_capacity(row_group_size, compact),
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

//! Native Parquet Streaming Writer for H3 Raster Hexification
//!
//! Streams aggregated H3 records directly from `MultiScanHorizonStreamer` into
//! Snappy- or ZSTD-compressed Parquet row groups without crossing DuckDB SQL/C-FFI boundaries.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;

use h3o::{CellIndex, LatLng};
use parquet::basic::Compression;
use parquet::column::writer::ColumnWriter;
use parquet::data_type::ByteArray;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

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

struct ContinuousRowGroupBuffer {
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

impl ContinuousRowGroupBuffer {
    fn with_capacity(capacity: usize, compact: bool) -> Self {
        Self {
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

    fn clear(&mut self, compact: bool) {
        self.h3_indices.clear();
        self.min_values.clear();
        self.max_values.clear();
        self.sum_values.clear();
        self.avg_values.clear();
        self.pixel_counts.clear();
        if !compact {
            self.h3_hexes.clear();
            self.lats.clear();
            self.lngs.clear();
        }
    }

    fn len(&self) -> usize {
        self.h3_indices.len()
    }

    fn is_empty(&self) -> bool {
        self.h3_indices.is_empty()
    }

    fn sort_by_h3_index(&mut self, compact: bool) {
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
        let mut sorted_hexes = if compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lats = if compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lngs = if compact { Vec::new() } else { Vec::with_capacity(n) };

        for &i in &perm {
            sorted_indices.push(self.h3_indices[i]);
            sorted_min.push(self.min_values[i]);
            sorted_max.push(self.max_values[i]);
            sorted_sum.push(self.sum_values[i]);
            sorted_avg.push(self.avg_values[i]);
            sorted_cnt.push(self.pixel_counts[i]);
            if !compact {
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
        if !compact {
            self.h3_hexes = sorted_hexes;
            self.lats = sorted_lats;
            self.lngs = sorted_lngs;
        }
    }
}

struct CategoricalRowGroupBuffer {
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

impl CategoricalRowGroupBuffer {
    fn with_capacity(capacity: usize, compact: bool) -> Self {
        Self {
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

    fn clear(&mut self, compact: bool) {
        self.h3_indices.clear();
        self.majorities.clear();
        self.fractions.clear();
        self.pixel_counts.clear();
        self.distinct_classes.clear();
        self.entropies.clear();
        if !compact {
            self.h3_hexes.clear();
            self.lats.clear();
            self.lngs.clear();
        }
    }

    fn len(&self) -> usize {
        self.h3_indices.len()
    }

    fn is_empty(&self) -> bool {
        self.h3_indices.is_empty()
    }

    fn sort_by_h3_index(&mut self, compact: bool) {
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
        let mut sorted_hexes = if compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lats = if compact { Vec::new() } else { Vec::with_capacity(n) };
        let mut sorted_lngs = if compact { Vec::new() } else { Vec::with_capacity(n) };

        for &i in &perm {
            sorted_indices.push(self.h3_indices[i]);
            sorted_majorities.push(self.majorities[i]);
            sorted_fractions.push(self.fractions[i]);
            sorted_pixel_counts.push(self.pixel_counts[i]);
            sorted_distinct.push(self.distinct_classes[i]);
            sorted_entropies.push(self.entropies[i]);
            if !compact {
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
        if !compact {
            self.h3_hexes = sorted_hexes;
            self.lats = sorted_lats;
            self.lngs = sorted_lngs;
        }
    }
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
        mut streamer: MultiScanHorizonStreamer,
        parquet_path: P,
        parquet_config: ParquetExportConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let compact = parquet_config.compact;
        let row_group_size = parquet_config.row_group_size.max(1024);

        let message_type = if compact {
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
        };

        let schema = Arc::new(parse_message_type(message_type)?);
        let props = Arc::new(
            WriterProperties::builder()
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

        let (writer_tx, writer_rx) = sync_channel::<ContinuousRowGroupBuffer>(2);
        let (recycle_tx, recycle_rx) = sync_channel::<ContinuousRowGroupBuffer>(2);

        let buf1 = ContinuousRowGroupBuffer::with_capacity(row_group_size, compact);
        let buf2 = ContinuousRowGroupBuffer::with_capacity(row_group_size, compact);
        let _ = recycle_tx.send(buf2);

        let writer_handle = thread::spawn(move || -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            let mut total_rows = 0usize;
            while let Ok(mut buf) = writer_rx.recv() {
                if !buf.is_empty() {
                    buf.sort_by_h3_index(compact);
                    let count = buf.len();
                    Self::flush_continuous_row_group(
                        &mut writer,
                        compact,
                        &buf.h3_indices,
                        &buf.min_values,
                        &buf.max_values,
                        &buf.sum_values,
                        &buf.avg_values,
                        &buf.pixel_counts,
                        &buf.h3_hexes,
                        &buf.lats,
                        &buf.lngs,
                    )?;
                    total_rows += count;
                    buf.clear(compact);
                    let _ = recycle_tx.send(buf);
                }
            }
            writer.close()?;
            Ok(total_rows)
        });

        let mut current_buf = buf1;
        let mut hex_buf = [0u8; 16];

        loop {
            let space_left = row_group_size.saturating_sub(current_buf.len()).max(1);
            let drained = streamer.drain_completed_into(space_left, |_, record: MultiContinuousRecord| {
                let cell_u64 = record.h3_index;
                let acc = record.accumulator;
                current_buf.h3_indices.push(cell_u64 as i64);
                current_buf.min_values.push(acc.min);
                current_buf.max_values.push(acc.max);
                current_buf.sum_values.push(acc.sum);
                current_buf.avg_values.push(acc.mean());
                current_buf.pixel_counts.push(acc.count);

                if !compact {
                    let hex_slice = fast_hex_u64(cell_u64, &mut hex_buf);
                    current_buf.h3_hexes.push(ByteArray::from(hex_slice));

                    if let Ok(cell) = CellIndex::try_from(cell_u64) {
                        let ll: LatLng = cell.into();
                        current_buf.lats.push(ll.lat());
                        current_buf.lngs.push(ll.lng());
                    } else {
                        current_buf.lats.push(0.0);
                        current_buf.lngs.push(0.0);
                    }
                }
            });

            if current_buf.len() >= row_group_size {
                if writer_tx.send(current_buf).is_err() {
                    break;
                }
                current_buf = match recycle_rx.recv() {
                    Ok(b) => b,
                    Err(_) => ContinuousRowGroupBuffer::with_capacity(row_group_size, compact),
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

    fn flush_continuous_row_group(
        writer: &mut SerializedFileWriter<File>,
        compact: bool,
        h3_indices: &[i64],
        min_values: &[f64],
        max_values: &[f64],
        sum_values: &[f64],
        avg_values: &[f64],
        pixel_counts: &[f64],
        h3_hexes: &[ByteArray],
        lats: &[f64],
        lngs: &[f64],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        // Col 0: h3_index
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(h3_indices, None, None)?;
            }
            col_writer.close()?;
        }

        if !compact {
            // Col 1: h3_hex
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::ByteArrayColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(h3_hexes, None, None)?;
                }
                col_writer.close()?;
            }
        }

        // min_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(min_values, None, None)?;
            }
            col_writer.close()?;
        }

        // max_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(max_values, None, None)?;
            }
            col_writer.close()?;
        }

        // sum_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(sum_values, None, None)?;
            }
            col_writer.close()?;
        }

        // avg_value
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(avg_values, None, None)?;
            }
            col_writer.close()?;
        }

        // pixel_count
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(pixel_counts, None, None)?;
            }
            col_writer.close()?;
        }

        if !compact {
            // lat
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(lats, None, None)?;
                }
                col_writer.close()?;
            }

            // lng
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(lngs, None, None)?;
                }
                col_writer.close()?;
            }
        }

        row_group_writer.close()?;
        Ok(())
    }

    /// Stream categorical raster aggregation directly into Parquet
    pub fn write_categorical_streamer_to_parquet<P: AsRef<Path>>(
        mut streamer: MultiCategoricalHorizonStreamer,
        parquet_path: P,
        parquet_config: ParquetExportConfig,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let compact = parquet_config.compact;
        let row_group_size = parquet_config.row_group_size.max(1024);

        let message_type = if compact {
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
        };

        let schema = Arc::new(parse_message_type(message_type)?);
        let props = Arc::new(
            WriterProperties::builder()
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

        let (writer_tx, writer_rx) = sync_channel::<CategoricalRowGroupBuffer>(2);
        let (recycle_tx, recycle_rx) = sync_channel::<CategoricalRowGroupBuffer>(2);

        let buf1 = CategoricalRowGroupBuffer::with_capacity(row_group_size, compact);
        let buf2 = CategoricalRowGroupBuffer::with_capacity(row_group_size, compact);
        let _ = recycle_tx.send(buf2);

        let writer_handle = thread::spawn(move || -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
            let mut total_rows = 0usize;
            while let Ok(mut buf) = writer_rx.recv() {
                if !buf.is_empty() {
                    buf.sort_by_h3_index(compact);
                    let count = buf.len();
                    Self::flush_categorical_row_group(
                        &mut writer,
                        compact,
                        &buf.h3_indices,
                        &buf.majorities,
                        &buf.fractions,
                        &buf.pixel_counts,
                        &buf.distinct_classes,
                        &buf.entropies,
                        &buf.h3_hexes,
                        &buf.lats,
                        &buf.lngs,
                    )?;
                    total_rows += count;
                    buf.clear(compact);
                    let _ = recycle_tx.send(buf);
                }
            }
            writer.close()?;
            Ok(total_rows)
        });

        let mut current_buf = buf1;
        let mut hex_buf = [0u8; 16];

        loop {
            let space_left = row_group_size.saturating_sub(current_buf.len()).max(1);
            let drained = streamer.drain_completed_into(space_left, |_, record: MultiCategoricalRecord| {
                let cell_u64 = record.h3_index;
                let acc = record.accumulator;
                let (maj_cat, _maj_cnt, maj_frac) = acc.majority();
                current_buf.h3_indices.push(cell_u64 as i64);
                current_buf.majorities.push(maj_cat);
                current_buf.fractions.push(maj_frac);
                current_buf.pixel_counts.push(acc.total_count);
                current_buf.distinct_classes.push(acc.unique_classes() as i64);
                current_buf.entropies.push(acc.shannon_entropy());

                if !compact {
                    let hex_slice = fast_hex_u64(cell_u64, &mut hex_buf);
                    current_buf.h3_hexes.push(ByteArray::from(hex_slice));

                    if let Ok(cell) = CellIndex::try_from(cell_u64) {
                        let ll: LatLng = cell.into();
                        current_buf.lats.push(ll.lat());
                        current_buf.lngs.push(ll.lng());
                    } else {
                        current_buf.lats.push(0.0);
                        current_buf.lngs.push(0.0);
                    }
                }
            });

            if current_buf.len() >= row_group_size {
                if writer_tx.send(current_buf).is_err() {
                    break;
                }
                current_buf = match recycle_rx.recv() {
                    Ok(b) => b,
                    Err(_) => CategoricalRowGroupBuffer::with_capacity(row_group_size, compact),
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

    fn flush_categorical_row_group(
        writer: &mut SerializedFileWriter<File>,
        compact: bool,
        h3_indices: &[i64],
        majorities: &[i64],
        fractions: &[f64],
        pixel_counts: &[f64],
        distinct_classes: &[i64],
        entropies: &[f64],
        h3_hexes: &[ByteArray],
        lats: &[f64],
        lngs: &[f64],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut row_group_writer = writer.next_row_group()?;

        // Col 0: h3_index
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(h3_indices, None, None)?;
            }
            col_writer.close()?;
        }

        if !compact {
            // Col 1: h3_hex
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::ByteArrayColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(h3_hexes, None, None)?;
                }
                col_writer.close()?;
            }
        }

        // majority
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(majorities, None, None)?;
            }
            col_writer.close()?;
        }

        // majority_fraction
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(fractions, None, None)?;
            }
            col_writer.close()?;
        }

        // pixel_count
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(pixel_counts, None, None)?;
            }
            col_writer.close()?;
        }

        // distinct_classes
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::Int64ColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(distinct_classes, None, None)?;
            }
            col_writer.close()?;
        }

        // entropy
        if let Some(mut col_writer) = row_group_writer.next_column()? {
            if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                typed.write_batch(entropies, None, None)?;
            }
            col_writer.close()?;
        }

        if !compact {
            // lat
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(lats, None, None)?;
                }
                col_writer.close()?;
            }

            // lng
            if let Some(mut col_writer) = row_group_writer.next_column()? {
                if let ColumnWriter::DoubleColumnWriter(ref mut typed) = col_writer.untyped() {
                    typed.write_batch(lngs, None, None)?;
                }
                col_writer.close()?;
            }
        }

        row_group_writer.close()?;
        Ok(())
    }
}

//! Double-buffered streaming pipeline and row-group buffer abstractions for Parquet export.
//!
//! Provides lock-free double-buffered channel streaming: the aggregation stream drains
//! into the current buffer while a background thread sorts and flushes the previous buffer
//! to disk with zero pipeline stalls.

use std::fs::File;
use std::path::Path;
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread;

use parquet::basic::Encoding;
use parquet::column::writer::ColumnWriter;
use parquet::data_type::ByteArray;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::ColumnPath;

pub use crate::aggregator::multi_horizon::RecordStreamer;
use crate::parquet::geoparquet_metadata::build_geoparquet_metadata;
use crate::parquet::writer::ParquetExportConfig;

/// Streaming source abstraction for Parquet serialization.
///
/// Implemented by streaming raster aggregators producing completed records
/// for row-group batching.
pub trait ParquetStreamer {
    type Record;

    /// Drain up to `max_rows` completed records directly into a consumer closure
    fn drain_completed_into<F>(
        &mut self,
        max_rows: usize,
        consumer: F,
    ) -> crate::error::Result<usize>
    where
        F: FnMut(usize, Self::Record);

    /// Optional spatial bounding box in WGS84 [min_lon, min_lat, max_lon, max_lat]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        None
    }
}

impl<T: RecordStreamer> ParquetStreamer for T {
    type Record = T::Record;

    #[inline(always)]
    fn drain_completed_into<F>(
        &mut self,
        max_rows: usize,
        consumer: F,
    ) -> crate::error::Result<usize>
    where
        F: FnMut(usize, Self::Record),
    {
        RecordStreamer::drain_completed_into(self, max_rows, consumer)
    }

    #[inline]
    fn bounds_wgs84(&self) -> Option<[f64; 4]> {
        RecordStreamer::bounds_wgs84(self)
    }
}

#[inline]
pub(crate) fn compute_sort_permutation(h3_indices: &[i64]) -> Option<Vec<usize>> {
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
pub(crate) fn reorder_by_perm<T: Copy>(vec: &mut Vec<T>, perm: &[usize]) {
    let mut reordered = Vec::with_capacity(perm.len());
    for &i in perm {
        reordered.push(vec[i]);
    }
    *vec = reordered;
}

#[inline]
pub(crate) fn reorder_by_perm_take<T: Default>(vec: &mut Vec<T>, perm: &[usize]) {
    let mut reordered = Vec::with_capacity(perm.len());
    for &i in perm {
        reordered.push(std::mem::take(&mut vec[i]));
    }
    *vec = reordered;
}

#[inline(always)]
pub(crate) fn write_i64_column(
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
pub(crate) fn write_f64_column(
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
pub(crate) fn write_byte_array_column(
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

/// Common trait for Parquet row group column buffers (continuous and categorical)
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
    let compact = parquet_config.should_omit_redundant_columns();
    let geoparquet = parquet_config.geoparquet;
    let row_group_size = parquet_config.row_group_size.max(1);

    let message_type = B::schema_message(compact, geoparquet);
    let schema = Arc::new(parse_message_type(message_type)?);

    let mut props_builder = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_2_0)
        .set_column_encoding(ColumnPath::from("h3_index"), Encoding::DELTA_BINARY_PACKED)
        .set_compression(parquet_config.compression);

    if geoparquet {
        let bbox = streamer
            .bounds_wgs84()
            .unwrap_or([-180.0, -90.0, 180.0, 90.0]);
        let geo_json = build_geoparquet_metadata("geometry", bbox);
        props_builder = props_builder
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".to_string(), geo_json)]));
    }

    let props = Arc::new(props_builder.build());

    if let Some(parent) = parquet_path.as_ref().parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // Publish only a complete export. Failed streams must not replace an existing file
    // with a valid-looking, partial Parquet dataset.
    let parent = parquet_path
        .as_ref()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let output_file = tempfile::NamedTempFile::new_in(parent)?;
    let file = output_file.reopen()?;
    let mut writer = SerializedFileWriter::new(file, schema, props)?;

    let (writer_tx, writer_rx) = sync_channel::<B>(2);
    let (recycle_tx, recycle_rx) = sync_channel::<B>(2);

    let buf1 = B::with_capacity_and_options(row_group_size, compact, geoparquet);
    let buf2 = B::with_capacity_and_options(row_group_size, compact, geoparquet);
    let _ = recycle_tx.send(buf2);

    let writer_handle = thread::spawn(
        move || -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
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
        },
    );

    let mut current_buf = buf1;
    let mut total_drained = 0usize;

    loop {
        let space_left = row_group_size.saturating_sub(current_buf.len()).max(1);
        let drained = match streamer.drain_completed_into(space_left, |_, record| {
            current_buf.push_record(record);
        }) {
            Ok(n) => n,
            Err(error) => {
                // Unblock the writer and join it before returning the stream failure.
                drop(writer_tx);
                drop(recycle_rx);
                let _ = writer_handle.join();
                return Err(error.into());
            }
        };
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
    output_file.persist(&parquet_path)?;
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
    run_parquet_streaming_pipeline_with_progress::<S, B, P, _>(
        streamer,
        parquet_path,
        parquet_config,
        |_| {},
    )
}

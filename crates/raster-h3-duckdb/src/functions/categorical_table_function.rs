use std::sync::{Arc, Mutex};

use crate::aggregator::multi_horizon::{
    CategoricalLongCursor, CategoricalLongRow, MultiCategoricalHorizonStreamer,
    MultiCategoricalRecord, MultiResolutionConfig, ScanPartition,
};
use crate::aggregator::remap::CategoryRemapper;
use crate::ffi::duckdb_c::*;
use crate::ffi::get_vector_size;
use crate::functions::bind_utils::{
    add_named_parameter, BindHelper, ChunkWriter, CommonRasterParams, ConcurrentRecordQueue,
    TableFunctionLocalData,
};
use crate::functions::partition_functions::{
    drain_categorical, parse_partition_param, write_partial_state_column, ScanRow,
    PARTIAL_STATE_COLUMN,
};
use crate::functions::raster_table::{self, add_geom_column, RasterTable, ScanContext};
use crate::grid::GridKind;
use crate::raster::mosaic::MosaicReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalOutputFormat {
    Wide,
    Long,
}

pub struct RasterH3CategoricalBindData {
    pub common: CommonRasterParams,
    pub format: CategoricalOutputFormat,
    pub min_count: Option<f64>,
    pub min_majority_fraction: Option<f64>,
    pub remapper: Option<Arc<CategoryRemapper>>,
    /// `partition`: scan one partition of a plan (wide format), adding `partial_state`.
    pub partition: Option<ScanPartition>,
}

pub struct RasterH3CategoricalGlobalData {
    pub grid: GridKind,
    pub streamer: Mutex<MultiCategoricalHorizonStreamer>,
    pub record_queue: ConcurrentRecordQueue<ScanRow<MultiCategoricalRecord>>,
    pub format: CategoricalOutputFormat,
    pub projected_columns: Vec<usize>,
    /// Long format: the one cell currently being expanded into rows.
    pub long_cursor: Mutex<CategoricalLongCursor>,
    pub emit_geom: bool,
    /// Column index of `partial_state` for a partitioned scan.
    pub partial_state_column: Option<usize>,
}

pub type RasterH3CategoricalLocalData = TableFunctionLocalData;

/// `h3_raster_categorical_aggregate`: per-cell class histograms, majority and
/// entropy over a categorical raster, in wide or long format.
pub(crate) struct CategoricalTable;

impl RasterTable for CategoricalTable {
    type Bind = RasterH3CategoricalBindData;
    type Global = RasterH3CategoricalGlobalData;

    const NAMES: &'static [&'static str] =
        &["h3_raster_categorical_aggregate", "h3_raster_categorical"];
    const FUNCTION: &'static str = "h3_raster_categorical_aggregate";
    const MISSING_BIND_DATA: &'static str = "Missing bind data in categorical init";

    unsafe fn register_named_parameters(tf: duckdb_table_function) {
        // SAFETY: `tf` is a live, unregistered table function (caller contract).
        unsafe {
            add_named_parameter(tf, "chunk_size", DuckDBType::BigInt);
            add_named_parameter(tf, "format", DuckDBType::Varchar);
            add_named_parameter(tf, "min_count", DuckDBType::Double);
            add_named_parameter(tf, "min_majority_fraction", DuckDBType::Double);
            add_named_parameter(tf, "geom", DuckDBType::Boolean);
            add_named_parameter(tf, "remap", DuckDBType::Varchar);
            add_named_parameter(tf, "partition", DuckDBType::Varchar);
        }
    }

    unsafe fn bind(
        bind: &BindHelper,
        common: CommonRasterParams,
    ) -> Option<RasterH3CategoricalBindData> {
        let mut format = CategoricalOutputFormat::Wide;
        if let Some(s) = bind.get_named_string("format") {
            if s.trim().eq_ignore_ascii_case("long") {
                format = CategoricalOutputFormat::Long;
            }
        }

        let min_count = bind.get_named_double("min_count");
        let min_majority_fraction = bind.get_named_double("min_majority_fraction");

        let remapper = match bind.get_named_string("remap") {
            Some(s) => match CategoryRemapper::parse(&s) {
                Ok(rem) => Some(rem.into_arc()),
                Err(e) => {
                    bind.set_error(&format!("Failed to parse remap parameter: {}", e));
                    return None;
                }
            },
            None => None,
        };

        let partition = match parse_partition_param(bind) {
            Ok(p) => p,
            Err(e) => {
                bind.set_error(&format!("Invalid partition parameter: {e}"));
                return None;
            }
        };
        if partition.is_some() && format == CategoricalOutputFormat::Long {
            bind.set_error(
                "partition supports the wide categorical format only; reshape the merged \
                 result to long format afterwards",
            );
            return None;
        }

        bind.add_result_column(common.grid.index_column_name(), DuckDBType::UBigInt);
        bind.add_result_column(common.grid.hex_column_name(), DuckDBType::Varchar);
        match format {
            CategoricalOutputFormat::Wide => {
                bind.add_result_column("majority_class", DuckDBType::BigInt);
                bind.add_result_column("majority_fraction", DuckDBType::Double);
                bind.add_result_column("majority_count", DuckDBType::Double);
                bind.add_result_column("unique_classes", DuckDBType::BigInt);
                bind.add_result_column("total_count", DuckDBType::Double);
                bind.add_result_column("histogram", DuckDBType::Varchar);
                bind.add_result_column("resolution", DuckDBType::UTinyInt);
                bind.add_result_column("shannon_entropy", DuckDBType::Double);
                bind.add_result_column("entropy", DuckDBType::Double);
                bind.add_result_column("distinct_classes", DuckDBType::BigInt);
                bind.add_result_column("wkb", DuckDBType::Blob);
            }
            CategoricalOutputFormat::Long => {
                bind.add_result_column("category", DuckDBType::BigInt);
                bind.add_result_column("count", DuckDBType::Double);
                bind.add_result_column("fraction", DuckDBType::Double);
                bind.add_result_column("total_count", DuckDBType::Double);
                bind.add_result_column("resolution", DuckDBType::UTinyInt);
                bind.add_result_column("shannon_entropy", DuckDBType::Double);
                bind.add_result_column("entropy", DuckDBType::Double);
                bind.add_result_column("distinct_classes", DuckDBType::BigInt);
                bind.add_result_column("unique_classes", DuckDBType::BigInt);
                bind.add_result_column("wkb", DuckDBType::Blob);
            }
        }
        if common.emit_geom {
            // SAFETY: `bind` wraps the live bind info (caller contract).
            unsafe { add_geom_column(bind) };
        }
        if partition.is_some() {
            // Wide format only (checked above).
            bind.add_result_column(PARTIAL_STATE_COLUMN, DuckDBType::Blob);
        }

        Some(RasterH3CategoricalBindData {
            common,
            format,
            min_count,
            min_majority_fraction,
            remapper,
            partition,
        })
    }

    fn common(bind: &RasterH3CategoricalBindData) -> &CommonRasterParams {
        &bind.common
    }

    fn init(
        bind: &RasterH3CategoricalBindData,
        mosaic: Arc<MosaicReader>,
        projected_columns: Vec<usize>,
        mut config: MultiResolutionConfig,
    ) -> Result<RasterH3CategoricalGlobalData, String> {
        config.min_count = bind.min_count;
        config.min_majority_fraction = bind.min_majority_fraction;
        config.remapper = bind.remapper.clone();
        config.partition = bind.partition.clone();

        let streamer = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &config)
            .map_err(|e| format!("Failed to initialize categorical streamer: {}", e))?;

        // Long-format columns 7/8 are the entropy aliases.
        let with_entropy = projected_columns.iter().any(|&c| c == 7 || c == 8);
        Ok(RasterH3CategoricalGlobalData {
            grid: bind.common.grid,
            streamer: Mutex::new(streamer),
            record_queue: ConcurrentRecordQueue::new(),
            format: bind.format,
            projected_columns,
            long_cursor: Mutex::new(CategoricalLongCursor::new(with_entropy)),
            emit_geom: bind.common.emit_geom,
            // Wide columns: 0..=12, then `geom` when emitted, then `partial_state`.
            partial_state_column: bind
                .partition
                .as_ref()
                .map(|_| 13 + usize::from(bind.common.emit_geom)),
        })
    }

    unsafe fn scan(global: &RasterH3CategoricalGlobalData, ctx: &mut ScanContext<'_>) {
        // SAFETY: `ctx` carries the live scan handles (caller contract).
        unsafe {
            match global.format {
                CategoricalOutputFormat::Wide => scan_wide(global, ctx),
                CategoricalOutputFormat::Long => scan_long(global, ctx),
            }
        }
    }
}

/// Wide format: one row per cell.
///
/// # Safety
/// `ctx` must carry the live scan handles, with `output` laid out as the wide
/// columns declared at bind time.
unsafe fn scan_wide(global: &RasterH3CategoricalGlobalData, ctx: &mut ScanContext<'_>) {
    // SAFETY: `ctx` carries the live scan handles (caller contract). `writer` wraps the
    // output chunk, whose columns follow `projected_columns` in order, and each arm
    // writes values of exactly the physical type that column was declared with at bind
    // time (see the column map).
    unsafe {
        let proj_cols = &global.projected_columns;
        let Some((batch, mut writer)) = ctx.next_batch(
            &global.record_queue,
            &global.streamer,
            proj_cols,
            |s, max_rows, f| {
                drain_categorical(s, max_rows, global.partial_state_column.is_some(), f)
            },
        ) else {
            return;
        };

        let batch_len = batch.len();

        let mut out_maj_cls: Option<usize> = None;
        let mut out_maj_frac: Option<usize> = None;
        let mut out_maj_cnt: Option<usize> = None;
        let mut out_uniq: Option<usize> = None;
        let mut out_distinct: Option<usize> = None;
        let mut out_shannon: Option<usize> = None;
        let mut out_entropy: Option<usize> = None;
        let mut out_idx_wkb: Option<usize> = None;
        let mut out_idx_geom: Option<usize> = None;
        let mut out_idx_state: Option<usize> = None;

        // Schema column mapping (Wide):
        // 0: h3_index UBIGINT
        // 1: h3_hex VARCHAR
        // 2: majority_class BIGINT
        // 3: majority_fraction DOUBLE
        // 4: majority_count DOUBLE
        // 5: unique_classes BIGINT
        // 6: total_count DOUBLE
        // 7: histogram VARCHAR
        // 8: resolution UTINYINT
        // 9: shannon_entropy DOUBLE
        // 10: entropy DOUBLE
        // 11: distinct_classes BIGINT
        // 12: wkb BLOB
        // 13: geom GEOMETRY (if emit_geom)
        for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
            match orig_col {
                0 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.h3_index)),
                1 => {
                    writer.write_hex_column(out_idx, batch.iter().map(|r| &r.h3_index), ctx.hex_buf)
                }
                2 => out_maj_cls = Some(out_idx),
                3 => out_maj_frac = Some(out_idx),
                4 => out_maj_cnt = Some(out_idx),
                5 => out_uniq = Some(out_idx),
                6 => writer.fill_column(
                    out_idx,
                    batch_len,
                    batch.iter().map(|r| r.accumulator.total_count),
                ),
                7 => {
                    let mut hist_buf = String::with_capacity(256);
                    for (i, rec) in batch.iter().enumerate() {
                        rec.accumulator.histogram_json_into(&mut hist_buf);
                        writer.set_string_bytes(out_idx, i, hist_buf.as_bytes());
                    }
                }
                8 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.resolution)),
                9 => out_shannon = Some(out_idx),
                10 => out_entropy = Some(out_idx),
                11 => out_distinct = Some(out_idx),
                12 => out_idx_wkb = Some(out_idx),
                c if Some(c) == global.partial_state_column => out_idx_state = Some(out_idx),
                13 if global.emit_geom => out_idx_geom = Some(out_idx),
                _ => {}
            }
        }

        if out_maj_cls.is_some() || out_maj_frac.is_some() || out_maj_cnt.is_some() {
            for (i, rec) in batch.iter().enumerate() {
                let (maj_cls, maj_cnt, maj_frac) = rec.accumulator.majority();
                if let Some(col) = out_maj_cls {
                    writer.set_int64(col, i, maj_cls);
                }
                if let Some(col) = out_maj_frac {
                    writer.set_double(col, i, maj_frac);
                }
                if let Some(col) = out_maj_cnt {
                    writer.set_double(col, i, maj_cnt);
                }
            }
        }

        if out_uniq.is_some() || out_distinct.is_some() {
            for (i, rec) in batch.iter().enumerate() {
                let distinct = rec.accumulator.unique_classes() as i64;
                if let Some(col) = out_uniq {
                    writer.set_int64(col, i, distinct);
                }
                if let Some(col) = out_distinct {
                    writer.set_int64(col, i, distinct);
                }
            }
        }

        if out_shannon.is_some() || out_entropy.is_some() {
            for (i, rec) in batch.iter().enumerate() {
                let ent = rec.accumulator.shannon_entropy();
                if let Some(col) = out_shannon {
                    writer.set_double(col, i, ent);
                }
                if let Some(col) = out_entropy {
                    writer.set_double(col, i, ent);
                }
            }
        }

        writer.write_wkb_and_geom_columns_for(
            global.grid,
            out_idx_wkb,
            out_idx_geom,
            batch.iter().map(|r| &r.h3_index),
            ctx.wkb_buf,
        );
        if let Some(col) = out_idx_state {
            write_partial_state_column(&writer, col, &batch);
        }

        writer.set_size(batch_len);
    }
}

/// Long format: one row per (cell, class), expanded only as far as each chunk needs.
///
/// # Safety
/// `ctx` must carry the live scan handles, with `output` laid out as the long
/// columns declared at bind time.
unsafe fn scan_long(global: &RasterH3CategoricalGlobalData, ctx: &mut ScanContext<'_>) {
    // SAFETY: as for `scan_wide`, with the long-format column map.
    unsafe {
        let proj_cols = &global.projected_columns;
        let vector_size = get_vector_size();
        // Expand one cell at a time and only as far as this chunk
        // needs, so high-cardinality cells never queue more rows
        // than a single output vector.
        let rows: Vec<CategoricalLongRow> = {
            let mut cursor = match global.long_cursor.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let mut streamer_guard = None;
            let mut exhausted = false;
            let mut rows = Vec::with_capacity(vector_size);

            while rows.len() < vector_size {
                if cursor.remaining() > 0 {
                    cursor.emit_into(vector_size - rows.len(), |row| rows.push(row));
                    continue;
                }
                if exhausted {
                    break;
                }
                let streamer = streamer_guard.get_or_insert_with(|| match global.streamer.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                });
                let cursor_ref = &mut *cursor;
                match streamer.drain_completed_into(1, |_i, rec| cursor_ref.load(&rec)) {
                    Ok(0) => exhausted = true,
                    Ok(_) => {}
                    Err(error) => {
                        ctx.fail(&error.to_string());
                        return;
                    }
                }
            }
            rows
        };

        let num_taken = rows.len();
        if num_taken == 0 {
            duckdb_data_chunk_set_size(ctx.output, 0);
            return;
        }

        let mut writer = ChunkWriter::new(ctx.output);

        if proj_cols.is_empty() {
            writer.set_size(num_taken);
            return;
        }

        let mut out_idx_wkb: Option<usize> = None;
        let mut out_idx_geom: Option<usize> = None;

        // Schema column mapping (Long):
        // 0: h3_index UBIGINT
        // 1: h3_hex VARCHAR
        // 2: category BIGINT
        // 3: count DOUBLE
        // 4: fraction DOUBLE
        // 5: total_count DOUBLE
        // 6: resolution UTINYINT
        // 7: shannon_entropy DOUBLE
        // 8: entropy DOUBLE
        // 9: distinct_classes BIGINT
        // 10: unique_classes BIGINT
        // 11: wkb BLOB
        // 12: geom GEOMETRY (if emit_geom)
        for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
            match orig_col {
                0 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.cell_u64)),
                1 => {
                    writer.write_hex_column(out_idx, rows.iter().map(|r| &r.cell_u64), ctx.hex_buf)
                }
                2 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.category)),
                3 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.count)),
                4 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.fraction)),
                5 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.total_count)),
                6 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.resolution)),
                7 | 8 => writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.entropy)),
                9 | 10 => {
                    writer.fill_column(out_idx, num_taken, rows.iter().map(|r| r.distinct_classes))
                }
                11 => out_idx_wkb = Some(out_idx),
                12 if global.emit_geom => out_idx_geom = Some(out_idx),
                _ => {}
            }
        }

        writer.write_wkb_and_geom_columns_for(
            global.grid,
            out_idx_wkb,
            out_idx_geom,
            rows.iter().map(|r| &r.cell_u64),
            ctx.wkb_buf,
        );

        writer.set_size(num_taken);
    }
}

/// `a5_raster_categorical_aggregate`: per-cell class histograms over an A5 pentagon grid.
pub(crate) struct A5CategoricalTable;

impl RasterTable for A5CategoricalTable {
    type Bind = RasterH3CategoricalBindData;
    type Global = RasterH3CategoricalGlobalData;

    const NAMES: &'static [&'static str] = &[
        "a5_raster_categorical_aggregate",
        "a5_raster_categorical",
        "read_raster_a5_categorical",
        "raster_to_a5_categorical",
        "raster_a5_categorical",
    ];
    const FUNCTION: &'static str = "a5_raster_categorical_aggregate";
    const MISSING_BIND_DATA: &'static str = "Missing bind data in categorical init";
    const DEFAULT_GRID: GridKind = GridKind::A5;

    unsafe fn register_named_parameters(tf: duckdb_table_function) {
        // SAFETY: forwarded with the same contract.
        unsafe { CategoricalTable::register_named_parameters(tf) }
    }

    unsafe fn bind(
        bind: &BindHelper,
        common: CommonRasterParams,
    ) -> Option<RasterH3CategoricalBindData> {
        // SAFETY: forwarded with the same contract.
        unsafe { CategoricalTable::bind(bind, common) }
    }

    fn common(bind: &RasterH3CategoricalBindData) -> &CommonRasterParams {
        CategoricalTable::common(bind)
    }

    fn init(
        bind: &RasterH3CategoricalBindData,
        mosaic: Arc<MosaicReader>,
        projected_columns: Vec<usize>,
        config: MultiResolutionConfig,
    ) -> Result<RasterH3CategoricalGlobalData, String> {
        CategoricalTable::init(bind, mosaic, projected_columns, config)
    }

    unsafe fn scan(global: &RasterH3CategoricalGlobalData, ctx: &mut ScanContext<'_>) {
        // SAFETY: forwarded with the same contract.
        unsafe { CategoricalTable::scan(global, ctx) }
    }
}

/// Declare a [`RasterTable`] that is [`CategoricalTable`] with another default grid.
macro_rules! grid_categorical_table {
    ($(#[$doc:meta])* $table:ident, $grid:expr, $function:literal, [$($alias:literal),+ $(,)?]) => {
        $(#[$doc])*
        pub(crate) struct $table;

        impl RasterTable for $table {
            type Bind = RasterH3CategoricalBindData;
            type Global = RasterH3CategoricalGlobalData;

            const NAMES: &'static [&'static str] = &[$function, $($alias),+];
            const FUNCTION: &'static str = $function;
            const MISSING_BIND_DATA: &'static str = "Missing bind data in categorical init";
            const DEFAULT_GRID: GridKind = $grid;

            unsafe fn register_named_parameters(tf: duckdb_table_function) {
                // SAFETY: forwarded with the same contract.
                unsafe { CategoricalTable::register_named_parameters(tf) }
            }

            unsafe fn bind(bind: &BindHelper, common: CommonRasterParams) -> Option<RasterH3CategoricalBindData> {
                // SAFETY: forwarded with the same contract.
                unsafe { CategoricalTable::bind(bind, common) }
            }

            fn common(bind: &RasterH3CategoricalBindData) -> &CommonRasterParams {
                CategoricalTable::common(bind)
            }

            fn init(
                bind: &RasterH3CategoricalBindData,
                mosaic: Arc<MosaicReader>,
                projected_columns: Vec<usize>,
                config: MultiResolutionConfig,
            ) -> Result<RasterH3CategoricalGlobalData, String> {
                CategoricalTable::init(bind, mosaic, projected_columns, config)
            }

            unsafe fn scan(global: &RasterH3CategoricalGlobalData, ctx: &mut ScanContext<'_>) {
                // SAFETY: forwarded with the same contract.
                unsafe { CategoricalTable::scan(global, ctx) }
            }
        }
    };
}

grid_categorical_table!(
    /// `s2_raster_categorical_aggregate`: the categorical aggregation over Google S2 cells.
    S2CategoricalTable,
    GridKind::S2,
    "s2_raster_categorical_aggregate",
    [
        "s2_raster_categorical",
        "read_raster_s2_categorical",
        "raster_to_s2_categorical",
        "raster_s2_categorical",
    ]
);
grid_categorical_table!(
    /// `healpix_raster_categorical_aggregate`: the categorical aggregation over HEALPix cells.
    HealpixCategoricalTable,
    GridKind::Healpix,
    "healpix_raster_categorical_aggregate",
    [
        "healpix_raster_categorical",
        "read_raster_healpix_categorical",
        "raster_to_healpix_categorical",
        "raster_healpix_categorical",
    ]
);

/// Bind callback for categorical aggregation table function
///
/// # Safety
/// `info` must be the valid bind info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_categorical_bind(info: duckdb_bind_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::bind::<CategoricalTable>(info) }
}

/// Global init callback for categorical aggregation
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_categorical_init(info: duckdb_init_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::init::<CategoricalTable>(info) }
}

/// Thread-local init callback for categorical aggregation
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_categorical_init_local(info: duckdb_init_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::init_local(info) }
}

/// Scan callback for categorical aggregation
///
/// # Safety
/// `info` and `output` must be the handles DuckDB passes to this scan
/// callback, with `output` laid out as the columns declared at bind time.
pub unsafe extern "C" fn raster_h3_categorical_scan(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::scan::<CategoricalTable>(info, output) }
}

/// Register `h3_raster_categorical_aggregate` and `h3_raster_categorical` table functions
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn register_categorical_table_function(con: duckdb_connection) -> Result<(), String> {
    // SAFETY: forwarded with the same contract.
    unsafe {
        raster_table::register::<CategoricalTable>(con)?;
        raster_table::register::<A5CategoricalTable>(con)?;
        raster_table::register::<S2CategoricalTable>(con)?;
        raster_table::register::<HealpixCategoricalTable>(con)
    }
}

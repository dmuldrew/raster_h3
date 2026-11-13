use std::sync::{Arc, Mutex};

use crate::aggregator::multi_horizon::{
    MultiContinuousRecord, MultiResolutionConfig, MultiScanHorizonStreamer, QuantilePlan,
    QuantileTarget, ScanPartition, SpectralFormula,
};
use crate::ffi::duckdb_c::*;
use crate::functions::bind_utils::{
    add_named_parameter, BindHelper, CommonRasterParams, ConcurrentRecordQueue,
    TableFunctionLocalData,
};
use crate::functions::partition_functions::{
    drain_continuous, parse_partition_param, write_partial_state_column, ScanRow,
    PARTIAL_STATE_COLUMN,
};
use crate::functions::raster_table::{self, add_geom_column, RasterTable, ScanContext};
use crate::grid::GridKind;
use crate::raster::mosaic::MosaicReader;

/// User-data bound during table function query compilation
pub struct RasterH3BindData {
    pub common: CommonRasterParams,
    pub spectral_formula: Option<SpectralFormula>,
    pub min_count: Option<f64>,
    pub min_mean: Option<f64>,
    pub max_mean: Option<f64>,
    pub quantiles: Vec<QuantileTarget>,
    /// `partition`: scan one partition of a plan, adding a `partial_state` column.
    pub partition: Option<ScanPartition>,
}

/// Global scan state holding the streaming multi-core horizon aggregator and concurrent batch queue
pub struct RasterH3GlobalData {
    pub grid: GridKind,
    pub streamer: Mutex<MultiScanHorizonStreamer>,
    pub record_queue: ConcurrentRecordQueue<ScanRow<MultiContinuousRecord>>,
    pub projected_columns: Vec<usize>,
    pub quantiles: Vec<QuantileTarget>,
    pub emit_geom: bool,
    /// Column index of `partial_state` for a partitioned scan.
    pub partial_state_column: Option<usize>,
}

/// Thread-local state for parallel DuckDB execution threads (scratch buffers for hex string and WKB encoding)
pub type RasterH3LocalData = TableFunctionLocalData;

/// First quantile column: right after `wkb`, or after `geom` when emitted.
fn quantile_start_column(emit_geom: bool) -> usize {
    if emit_geom {
        11
    } else {
        10
    }
}

/// `h3_raster_continuous_aggregate`: per-cell mean/stddev/count/min/max/sum
/// and optional quantiles over a continuous raster.
pub(crate) struct ContinuousTable;

impl RasterTable for ContinuousTable {
    type Bind = RasterH3BindData;
    type Global = RasterH3GlobalData;

    const NAMES: &'static [&'static str] = &[
        "h3_raster_continuous_aggregate",
        "h3_raster_continuous",
        "raster_h3",
    ];
    const FUNCTION: &'static str = "h3_raster_continuous_aggregate";
    const MISSING_BIND_DATA: &'static str = "Missing bind data in init";

    unsafe fn register_named_parameters(tf: duckdb_table_function) {
        // SAFETY: `tf` is a live, unregistered table function (caller contract).
        unsafe {
            add_named_parameter(tf, "chunk_size", DuckDBType::BigInt);
            add_named_parameter(tf, "formula", DuckDBType::Varchar);
            add_named_parameter(tf, "nir_band", DuckDBType::BigInt);
            add_named_parameter(tf, "red_band", DuckDBType::BigInt);
            add_named_parameter(tf, "green_band", DuckDBType::BigInt);
            add_named_parameter(tf, "blue_band", DuckDBType::BigInt);
            add_named_parameter(tf, "swir_band", DuckDBType::BigInt);
            add_named_parameter(tf, "min_count", DuckDBType::Double);
            add_named_parameter(tf, "min_mean", DuckDBType::Double);
            add_named_parameter(tf, "max_mean", DuckDBType::Double);
            add_named_parameter(tf, "geom", DuckDBType::Boolean);
            add_named_parameter(tf, "quantiles", DuckDBType::Varchar);
            add_named_parameter(tf, "percentiles", DuckDBType::Varchar);
            add_named_parameter(tf, "partition", DuckDBType::Varchar);
        }
    }

    unsafe fn bind(bind: &BindHelper, common: CommonRasterParams) -> Option<RasterH3BindData> {
        let band = |name: &str, default: i64| {
            bind.get_named_int(name)
                .filter(|&b| b > 0)
                .unwrap_or(default) as usize
        };
        let (nir, red, green, blue, swir) = (
            band("nir_band", 4),
            band("red_band", 3),
            band("green_band", 2),
            band("blue_band", 1),
            band("swir_band", 6),
        );
        let spectral_formula = bind
            .get_named_string("formula")
            .as_deref()
            .and_then(|f| SpectralFormula::parse(f, nir, red, green, blue, swir));

        let min_count = bind.get_named_double("min_count");
        let min_mean = bind.get_named_double("min_mean");
        let max_mean = bind.get_named_double("max_mean");

        let mut quantiles = Vec::new();
        let q_param_val = bind
            .get_named_string("quantiles")
            .or_else(|| bind.get_named_string("percentiles"));
        if let Some(s) = q_param_val {
            match QuantileTarget::parse_list(&s) {
                Ok(q_targets) => quantiles = q_targets,
                Err(e) => {
                    bind.set_error(&format!("Invalid quantiles parameter: {}", e));
                    return None;
                }
            }
        }

        let partition = match parse_partition_param(bind) {
            Ok(p) => p,
            Err(e) => {
                bind.set_error(&format!("Invalid partition parameter: {e}"));
                return None;
            }
        };
        if partition.is_some() && !quantiles.is_empty() {
            bind.set_error(
                "partition cannot be combined with quantiles: merged cells carry no sketches",
            );
            return None;
        }

        bind.add_result_column(common.grid.index_column_name(), DuckDBType::UBigInt);
        bind.add_result_column(common.grid.hex_column_name(), DuckDBType::Varchar);
        bind.add_result_column("mean", DuckDBType::Double);
        bind.add_result_column("stddev", DuckDBType::Double);
        bind.add_result_column("count", DuckDBType::Double);
        bind.add_result_column("min", DuckDBType::Double);
        bind.add_result_column("max", DuckDBType::Double);
        bind.add_result_column("sum", DuckDBType::Double);
        bind.add_result_column("resolution", DuckDBType::UTinyInt);
        bind.add_result_column("wkb", DuckDBType::Blob);
        if common.emit_geom {
            // SAFETY: `bind` wraps the live bind info (caller contract).
            unsafe { add_geom_column(bind) };
        }
        for target in &quantiles {
            bind.add_result_column(target.column_name(), DuckDBType::Double);
        }
        if partition.is_some() {
            bind.add_result_column(PARTIAL_STATE_COLUMN, DuckDBType::Blob);
        }

        Some(RasterH3BindData {
            common,
            spectral_formula,
            min_count,
            min_mean,
            max_mean,
            quantiles,
            partition,
        })
    }

    fn common(bind: &RasterH3BindData) -> &CommonRasterParams {
        &bind.common
    }

    fn init(
        bind: &RasterH3BindData,
        mosaic: Arc<MosaicReader>,
        projected_columns: Vec<usize>,
        mut config: MultiResolutionConfig,
    ) -> Result<RasterH3GlobalData, String> {
        config.apply_scale_offset = bind.common.scale_offset;
        config.spectral_formula = bind.spectral_formula;
        config.min_count = bind.min_count;
        config.min_mean = bind.min_mean;
        config.max_mean = bind.max_mean;
        // Build per-cell sketches only if the query selects a quantile column;
        // DuckDB includes filter columns in the projection, so WHERE clauses on
        // quantiles still get them.
        let q_start_col = quantile_start_column(bind.common.emit_geom);
        let q_end_col = q_start_col + bind.quantiles.len();
        if projected_columns
            .iter()
            .any(|c| (q_start_col..q_end_col).contains(c))
        {
            config.quantiles = bind.quantiles.clone();
        }
        config.partition = bind.partition.clone();

        let streamer = MultiScanHorizonStreamer::new_mosaic(mosaic, &config)
            .map_err(|e| format!("Failed to initialize multi-streamer: {}", e))?;

        Ok(RasterH3GlobalData {
            grid: bind.common.grid,
            streamer: Mutex::new(streamer),
            record_queue: ConcurrentRecordQueue::new(),
            projected_columns,
            quantiles: bind.quantiles.clone(),
            emit_geom: bind.common.emit_geom,
            partial_state_column: bind.partition.as_ref().map(|_| q_end_col),
        })
    }

    unsafe fn scan(global: &RasterH3GlobalData, ctx: &mut ScanContext<'_>) {
        // SAFETY: `ctx` carries the live scan handles (caller contract). `writer` wraps
        // the output chunk, whose columns follow `projected_columns` in order, and each
        // arm writes values of exactly the physical type that column was declared with
        // at bind time (see the column map).
        unsafe {
            let proj_cols = &global.projected_columns;
            let Some((batch, mut writer)) = ctx.next_batch(
                &global.record_queue,
                &global.streamer,
                proj_cols,
                |s, max_rows, f| {
                    drain_continuous(s, max_rows, global.partial_state_column.is_some(), f)
                },
            ) else {
                return;
            };

            let batch_len = batch.len();
            let q_start_col = quantile_start_column(global.emit_geom);
            let mut out_idx_wkb: Option<usize> = None;
            let mut out_idx_geom: Option<usize> = None;
            let mut out_idx_state: Option<usize> = None;

            // Evaluate every projected quantile column with one sketch sweep
            // per cell, rather than one sweep per (cell, column).
            let mut q_targets: Vec<usize> = proj_cols
                .iter()
                .filter_map(|&c| c.checked_sub(q_start_col))
                .filter(|&q| q < global.quantiles.len())
                .collect();
            q_targets.sort_unstable();
            q_targets.dedup();
            let q_values: Vec<Vec<f64>> = if q_targets.is_empty() {
                Vec::new()
            } else {
                let plan = QuantilePlan::new(q_targets.iter().map(|&i| &global.quantiles[i]));
                let mut columns: Vec<Vec<f64>> = (0..plan.len())
                    .map(|_| Vec::with_capacity(batch_len))
                    .collect();
                let mut row = Vec::with_capacity(plan.len());
                for r in batch.iter() {
                    plan.evaluate_into(&r.accumulator, &mut row);
                    for (column, &v) in columns.iter_mut().zip(&row) {
                        column.push(v);
                    }
                }
                columns
            };

            // Schema column mapping:
            // 0: h3_index UBIGINT
            // 1: h3_hex VARCHAR
            // 2: mean DOUBLE
            // 3: stddev DOUBLE
            // 4: count DOUBLE
            // 5: min DOUBLE
            // 6: max DOUBLE
            // 7: sum DOUBLE
            // 8: resolution UTINYINT
            // 9: wkb BLOB
            // 10: geom GEOMETRY (if emit_geom)
            // 10/11+: quantiles DOUBLE
            // then partial_state BLOB (partitioned scans)
            for (out_idx, &orig_col) in proj_cols.iter().enumerate() {
                match orig_col {
                    0 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.h3_index)),
                    1 => writer.write_hex_column(
                        out_idx,
                        batch.iter().map(|r| &r.h3_index),
                        ctx.hex_buf,
                    ),
                    2 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.mean()),
                    ),
                    3 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.stddev()),
                    ),
                    4 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.count),
                    ),
                    5 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.min),
                    ),
                    6 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.max),
                    ),
                    7 => writer.fill_column(
                        out_idx,
                        batch_len,
                        batch.iter().map(|r| r.accumulator.sum),
                    ),
                    8 => writer.fill_column(out_idx, batch_len, batch.iter().map(|r| r.resolution)),
                    9 => out_idx_wkb = Some(out_idx),
                    10 if global.emit_geom => out_idx_geom = Some(out_idx),
                    c if Some(c) == global.partial_state_column => out_idx_state = Some(out_idx),
                    c if c >= q_start_col => {
                        if let Ok(slot) = q_targets.binary_search(&(c - q_start_col)) {
                            writer.fill_column(out_idx, batch_len, q_values[slot].iter().copied());
                        }
                    }
                    _ => {}
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
}

/// `a5_raster_continuous_aggregate`: per-cell statistics over an A5 pentagon grid.
pub(crate) struct A5ContinuousTable;

impl RasterTable for A5ContinuousTable {
    type Bind = RasterH3BindData;
    type Global = RasterH3GlobalData;

    const NAMES: &'static [&'static str] = &[
        "a5_raster_continuous_aggregate",
        "a5_raster_continuous",
        "read_raster_a5",
        "raster_to_a5",
        "raster_a5",
    ];
    const FUNCTION: &'static str = "a5_raster_continuous_aggregate";
    const MISSING_BIND_DATA: &'static str = "Missing bind data in init";
    const DEFAULT_GRID: GridKind = GridKind::A5;

    unsafe fn register_named_parameters(tf: duckdb_table_function) {
        // SAFETY: forwarded with the same contract.
        unsafe { ContinuousTable::register_named_parameters(tf) }
    }

    unsafe fn bind(bind: &BindHelper, common: CommonRasterParams) -> Option<RasterH3BindData> {
        // SAFETY: forwarded with the same contract.
        unsafe { ContinuousTable::bind(bind, common) }
    }

    fn common(bind: &RasterH3BindData) -> &CommonRasterParams {
        ContinuousTable::common(bind)
    }

    fn init(
        bind: &RasterH3BindData,
        mosaic: Arc<MosaicReader>,
        projected_columns: Vec<usize>,
        config: MultiResolutionConfig,
    ) -> Result<RasterH3GlobalData, String> {
        ContinuousTable::init(bind, mosaic, projected_columns, config)
    }

    unsafe fn scan(global: &RasterH3GlobalData, ctx: &mut ScanContext<'_>) {
        // SAFETY: forwarded with the same contract.
        unsafe { ContinuousTable::scan(global, ctx) }
    }
}

/// Declare a [`RasterTable`] that is [`ContinuousTable`] with another default grid.
macro_rules! grid_continuous_table {
    ($(#[$doc:meta])* $table:ident, $grid:expr, $function:literal, [$($alias:literal),+ $(,)?]) => {
        $(#[$doc])*
        pub(crate) struct $table;

        impl RasterTable for $table {
            type Bind = RasterH3BindData;
            type Global = RasterH3GlobalData;

            const NAMES: &'static [&'static str] = &[$function, $($alias),+];
            const FUNCTION: &'static str = $function;
            const MISSING_BIND_DATA: &'static str = "Missing bind data in init";
            const DEFAULT_GRID: GridKind = $grid;

            unsafe fn register_named_parameters(tf: duckdb_table_function) {
                // SAFETY: forwarded with the same contract.
                unsafe { ContinuousTable::register_named_parameters(tf) }
            }

            unsafe fn bind(bind: &BindHelper, common: CommonRasterParams) -> Option<RasterH3BindData> {
                // SAFETY: forwarded with the same contract.
                unsafe { ContinuousTable::bind(bind, common) }
            }

            fn common(bind: &RasterH3BindData) -> &CommonRasterParams {
                ContinuousTable::common(bind)
            }

            fn init(
                bind: &RasterH3BindData,
                mosaic: Arc<MosaicReader>,
                projected_columns: Vec<usize>,
                config: MultiResolutionConfig,
            ) -> Result<RasterH3GlobalData, String> {
                ContinuousTable::init(bind, mosaic, projected_columns, config)
            }

            unsafe fn scan(global: &RasterH3GlobalData, ctx: &mut ScanContext<'_>) {
                // SAFETY: forwarded with the same contract.
                unsafe { ContinuousTable::scan(global, ctx) }
            }
        }
    };
}

grid_continuous_table!(
    /// `s2_raster_continuous_aggregate`: the continuous aggregation over Google S2 cells.
    S2ContinuousTable,
    GridKind::S2,
    "s2_raster_continuous_aggregate",
    [
        "s2_raster_continuous",
        "read_raster_s2",
        "raster_to_s2",
        "raster_s2",
    ]
);
grid_continuous_table!(
    /// `healpix_raster_continuous_aggregate`: the continuous aggregation over HEALPix cells.
    HealpixContinuousTable,
    GridKind::Healpix,
    "healpix_raster_continuous_aggregate",
    [
        "healpix_raster_continuous",
        "read_raster_healpix",
        "raster_to_healpix",
        "raster_healpix",
    ]
);

/// Bind callback: parses input arguments, defines output columns, and returns bind data
///
/// # Safety
/// `info` must be the valid bind info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_bind(info: duckdb_bind_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::bind::<ContinuousTable>(info) }
}

/// Global init callback: opens the raster mosaic and starts the streaming horizon aggregator
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_init(info: duckdb_init_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::init::<ContinuousTable>(info) }
}

/// Thread-local init callback for multi-threaded parallel DuckDB execution
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub unsafe extern "C" fn raster_h3_init_local(info: duckdb_init_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::init_local(info) }
}

/// Scan callback: streaming vector emission with projection pushdown
///
/// # Safety
/// `info` and `output` must be the handles DuckDB passes to this scan
/// callback, with `output` laid out as the columns declared at bind time.
pub unsafe extern "C" fn raster_h3_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    // SAFETY: forwarded with the same contract.
    unsafe { raster_table::scan::<ContinuousTable>(info, output) }
}

/// Register `h3_raster_continuous_aggregate` and `h3_raster_continuous` table functions
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn register_table_function(con: duckdb_connection) -> Result<(), String> {
    // SAFETY: forwarded with the same contract.
    unsafe {
        raster_table::register::<ContinuousTable>(con)?;
        raster_table::register::<A5ContinuousTable>(con)?;
        raster_table::register::<S2ContinuousTable>(con)?;
        raster_table::register::<HealpixContinuousTable>(con)
    }
}

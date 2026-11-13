//! Shared driver for the raster aggregation table functions.
//!
//! `h3_raster_continuous_aggregate` and `h3_raster_categorical_aggregate` run the
//! same DuckDB lifecycle: parse the common raster parameters, declare columns,
//! open the mosaic, build a `MultiResolutionConfig`, start a streamer, and
//! drain batches into projected output columns. `RasterTable` captures only
//! what differs between them; the generic `extern "C"` callbacks here (`bind`,
//! `init`, `init_local`, `scan`) and `register` own the rest, so the FFI
//! plumbing, null checks and error paths live in one place.

use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};

use crate::aggregator::multi_horizon::MultiResolutionConfig;
use crate::encoding::{WkbBuf, WKB_BUF_LEN};
use crate::ffi::duckdb_c::*;
use crate::ffi::{ffi_bind_guard, ffi_init_guard, ffi_scan_guard, to_c_string};
use crate::functions::bind_utils::{
    add_positional_parameter, delete_boxed, estimate_raster_cardinality_for,
    extract_projected_columns, init_table_function_local, open_mosaic_or_set_error,
    register_common_raster_named_parameters, set_table_function_init_data, BindHelper, ChunkWriter,
    CommonRasterParams, ConcurrentRecordQueue, TableFunctionLocalData,
};
use crate::grid::GridKind;
use crate::raster::mosaic::MosaicReader;

/// What a raster aggregation table function adds on top of the shared driver.
pub(crate) trait RasterTable: 'static {
    /// Bind data handed to DuckDB; read by `init` on another thread.
    type Bind: Send + Sync + 'static;
    /// Global scan state; read concurrently by every scan thread.
    type Global: Send + Sync + 'static;

    /// SQL names the function is registered under.
    const NAMES: &'static [&'static str];
    /// Name used in common-parameter error messages.
    const FUNCTION: &'static str;
    /// Init error when DuckDB hands init no bind data.
    const MISSING_BIND_DATA: &'static str;
    /// Default grid system when `grid` is not explicitly specified.
    const DEFAULT_GRID: GridKind = GridKind::H3;

    /// Register the function-specific named parameters.
    ///
    /// # Safety
    /// `tf` must be a live, unregistered table function.
    unsafe fn register_named_parameters(tf: duckdb_table_function);

    /// Parse function-specific parameters and declare every result column.
    /// Returns `None` after reporting an error through `bind`.
    ///
    /// # Safety
    /// `bind` must wrap the live bind info of the current callback.
    unsafe fn bind(bind: &BindHelper, common: CommonRasterParams) -> Option<Self::Bind>;

    /// The common raster parameters inside the bind data.
    fn common(bind: &Self::Bind) -> &CommonRasterParams;

    /// Finish `config` (already holding the common settings) and start the
    /// streamer. `Err` is the full init error message.
    fn init(
        bind: &Self::Bind,
        mosaic: Arc<MosaicReader>,
        projected_columns: Vec<usize>,
        config: MultiResolutionConfig,
    ) -> Result<Self::Global, String>;

    /// Fill one output chunk.
    ///
    /// # Safety
    /// `ctx` must carry the live handles of the current scan callback, with
    /// `output` laid out as the columns declared at bind time.
    unsafe fn scan(global: &Self::Global, ctx: &mut ScanContext<'_>);
}

/// Handles and per-thread scratch buffers for one scan call.
pub(crate) struct ScanContext<'a> {
    pub info: duckdb_function_info,
    pub output: duckdb_data_chunk,
    pub hex_buf: &'a mut [u8; 16],
    pub wkb_buf: &'a mut WkbBuf,
}

impl ScanContext<'_> {
    /// End the scan with `message` as the query error.
    ///
    /// # Safety
    /// The context handles must be live (struct contract).
    pub unsafe fn fail(&self, message: &str) {
        let message = to_c_string(message);
        // SAFETY: live handles (caller contract); DuckDB copies the message.
        unsafe {
            duckdb_function_set_error(self.info, message.as_ptr());
            duckdb_data_chunk_set_size(self.output, 0);
        }
    }

    /// Pop the next batch, refilling from `streamer` when the queue is empty.
    ///
    /// Returns `None` once the chunk is already complete: the stream failed
    /// (error reported) or ended (size 0), or no column is projected, as in
    /// `SELECT count(*)`, so only the row count is written.
    ///
    /// # Safety
    /// The context handles must be live (struct contract).
    pub unsafe fn next_batch<S, R, F>(
        &self,
        queue: &ConcurrentRecordQueue<R>,
        streamer: &Mutex<S>,
        projected_columns: &[usize],
        drain_into: F,
    ) -> Option<(Vec<R>, ChunkWriter)>
    where
        F: FnMut(&mut S, usize, &mut dyn FnMut(usize, R)) -> crate::error::Result<usize>,
    {
        let batch = match queue.pop_or_refill(streamer, drain_into) {
            Ok(Some(b)) if !b.is_empty() => b,
            Err(error) => {
                // SAFETY: live handles (caller contract).
                unsafe { self.fail(&error.to_string()) };
                return None;
            }
            _ => {
                // SAFETY: live output chunk (caller contract).
                unsafe { duckdb_data_chunk_set_size(self.output, 0) };
                return None;
            }
        };
        // SAFETY: `output` is the live chunk of this scan call.
        let writer = unsafe { ChunkWriter::new(self.output) };
        if projected_columns.is_empty() {
            // SAFETY: `writer` wraps the live output chunk.
            unsafe { writer.set_size(batch.len()) };
            return None;
        }
        Some((batch, writer))
    }
}

/// Declare the optional `geom` GEOMETRY column.
///
/// # Safety
/// `bind` must wrap the live bind info of the current callback.
pub(crate) unsafe fn add_geom_column(bind: &BindHelper) {
    // SAFETY: DuckDB copies the logical type, so it is destroyed once here.
    unsafe {
        let mut type_geom = crate::ffi::create_geometry_logical_type();
        bind.add_custom_result_column("geom", type_geom);
        duckdb_destroy_logical_type(&mut type_geom);
    }
}

/// Bind callback shared by the raster aggregation table functions.
///
/// # Safety
/// `info` must be the valid bind info DuckDB passes to this callback.
pub(crate) unsafe extern "C" fn bind<K: RasterTable>(info: duckdb_bind_info) {
    // SAFETY: DuckDB passes a live `info`; the `BindHelper` wrapping it is used only
    // inside this callback. The boxed `K::Bind` is handed to DuckDB with
    // `delete_boxed::<K::Bind>`, its matching destructor, which DuckDB calls once.
    unsafe {
        ffi_bind_guard(info, || {
            let helper = BindHelper::new(info);
            let Some(common) = helper.parse_common_raster_params_for(K::FUNCTION, K::DEFAULT_GRID)
            else {
                return;
            };
            let Some(bind_data) = K::bind(&helper, common) else {
                return;
            };
            let common = K::common(&bind_data);
            let estimated_cardinality = estimate_raster_cardinality_for(
                common.grid,
                &common.resolved_paths,
                &common.resolutions,
                common.remote_source_factory.as_ref(),
            );
            duckdb_bind_set_cardinality(info, estimated_cardinality as idx_t, false);
            duckdb_bind_set_bind_data(
                info,
                Box::into_raw(Box::new(bind_data)) as *mut c_void,
                Some(delete_boxed::<K::Bind>),
            );
        });
    }
}

/// Global init callback shared by the raster aggregation table functions.
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub(crate) unsafe extern "C" fn init<K: RasterTable>(info: duckdb_init_info) {
    // SAFETY: DuckDB passes a live `info`. The bind-data pointer is the `Box<K::Bind>`
    // installed by `bind::<K>` (null-checked); DuckDB keeps it alive and unmodified for
    // the whole query, so a shared borrow is valid. `K::Global` is `Send + Sync`, as
    // every scan thread reads it.
    unsafe {
        ffi_init_guard(info, || {
            let bind_data_ptr = duckdb_init_get_bind_data(info) as *const K::Bind;
            if bind_data_ptr.is_null() {
                let err_msg = to_c_string(K::MISSING_BIND_DATA);
                duckdb_init_set_error(info, err_msg.as_ptr());
                return;
            }
            let bind_data = &*bind_data_ptr;
            let common = K::common(bind_data);

            let Some(mosaic) = open_mosaic_or_set_error(
                info,
                &common.resolved_paths,
                common.bbox,
                common.source_crs.as_deref(),
                common.overlap_rule,
                common.remote_source_factory.as_ref(),
            ) else {
                return;
            };

            let projected_columns = extract_projected_columns(info);

            let mut config =
                MultiResolutionConfig::new(common.resolutions.clone()).with_grid(common.grid);
            config.overlap_rule = common.overlap_rule;
            config.custom_crs = common.source_crs.clone();
            config.custom_nodata = common.nodata;
            config.bbox = common.bbox;
            config.sampling = common.sampling.clone();
            config.band = common.band as usize;
            config.compact = common.compact;
            config.compact_h3_children = common.compact;
            common.execution.apply(&mut config);

            match K::init(bind_data, mosaic, projected_columns, config) {
                Ok(global) => set_table_function_init_data(info, global),
                Err(message) => {
                    let err_msg = CString::new(message.replace('\0', " "))
                        .expect("interior NUL bytes were replaced");
                    duckdb_init_set_error(info, err_msg.as_ptr());
                }
            }
        });
    }
}

/// Thread-local init callback: per-thread hex and WKB scratch buffers.
///
/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
pub(crate) unsafe extern "C" fn init_local(info: duckdb_init_info) {
    // SAFETY: forwarded with the same contract.
    unsafe { init_table_function_local(info) }
}

/// Scan callback shared by the raster aggregation table functions.
///
/// # Safety
/// `info` and `output` must be the handles DuckDB passes to this scan
/// callback, with `output` laid out as the columns declared at bind time.
pub(crate) unsafe extern "C" fn scan<K: RasterTable>(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) {
    // SAFETY: DuckDB passes live `info`/`output` handles. The init pointer is the
    // `Box<K::Global>` installed by `init::<K>` (null-checked), alive until the query
    // ends and only borrowed immutably; `K::Global` synchronizes its own mutable state.
    // Local scratch data, if present, belongs to the calling DuckDB thread only.
    unsafe {
        ffi_scan_guard(info, output, || {
            let global_ptr = duckdb_function_get_init_data(info) as *const K::Global;
            if global_ptr.is_null() {
                duckdb_data_chunk_set_size(output, 0);
                return;
            }
            let local_ptr =
                duckdb_function_get_local_init_data(info) as *mut TableFunctionLocalData;
            let mut fallback_hex_buf = [0u8; 16];
            let mut fallback_wkb_buf: WkbBuf = [0u8; WKB_BUF_LEN];
            let (hex_buf, wkb_buf) = TableFunctionLocalData::get_scratch_buffers(
                local_ptr,
                &mut fallback_hex_buf,
                &mut fallback_wkb_buf,
            );
            let mut ctx = ScanContext {
                info,
                output,
                hex_buf,
                wkb_buf,
            };
            K::scan(&*global_ptr, &mut ctx);
        });
    }
}

/// Register `K` under each of its `RasterTable::NAMES`.
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub(crate) unsafe fn register<K: RasterTable>(con: duckdb_connection) -> Result<(), String> {
    // SAFETY: `con` is live (caller contract). Each table function gets a
    // NUL-terminated name that outlives the calls, parameter types DuckDB copies,
    // and callbacks with the C ABI DuckDB expects. It is destroyed exactly once
    // after `duckdb_register_table_function` has copied it into the catalog.
    unsafe {
        for name in K::NAMES {
            let fn_name = to_c_string(name);
            let tf = duckdb_create_table_function();
            duckdb_table_function_set_name(tf, fn_name.as_ptr());

            // Positional parameter 0: file_path (VARCHAR).
            add_positional_parameter(tf, DuckDBType::Varchar);
            register_common_raster_named_parameters(tf);
            K::register_named_parameters(tf);

            duckdb_table_function_set_bind(tf, bind::<K>);
            duckdb_table_function_set_init(tf, init::<K>);
            duckdb_table_function_set_local_init(tf, init_local);
            duckdb_table_function_set_function(tf, scan::<K>);
            duckdb_table_function_supports_projection_pushdown(tf, true);

            let state = duckdb_register_table_function(con, tf);
            let mut tf_mut = tf;
            duckdb_destroy_table_function(&mut tf_mut);

            if state != DuckDBState::Success {
                return Err(format!("Failed to register {} table function", name));
            }
        }
        Ok(())
    }
}

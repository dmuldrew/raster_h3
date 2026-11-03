//! Partitioned scans in DuckDB (see `raster_h3_core::aggregator::multi_horizon::partition`):
//!
//! - `h3_raster_plan_partitions(path, target_chunks := 256, ...)` splits a raster scan into
//!   partitions from metadata only, one row (`partition_index`, `partition_count`, `spec`) per
//!   partition. It accepts the scans' raster parameters (`crs`, `bbox`, `overlap_rule`, ...),
//!   which must match theirs.
//! - `partition := spec` on `h3_raster_continuous_aggregate` / `h3_raster_categorical_aggregate`
//!   scans one partition and appends a `partial_state BLOB` column: NULL for final cells, set
//!   for this partition's share of a cell that other partitions also reach.
//! - The aggregates `h3_merge_partials(partial_state)` and
//!   `h3_merge_categorical_partials(partial_state)` merge every partition's rows of a cell
//!   (`GROUP BY h3_index, resolution`) into a STRUCT of the scan's statistics columns,
//!   computed exactly as the scans compute them. NULL states are skipped.

use std::ffi::{c_char, c_void, CString};
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::aggregator::accumulator::H3Accumulator;
use crate::aggregator::categorical::CategoricalAccumulator;
use crate::aggregator::multi_horizon::{
    decode_partial_state, encode_partial_state, plan_partitions, MultiCategoricalHorizonStreamer,
    MultiCategoricalRecord, MultiContinuousRecord, MultiScanHorizonStreamer, ScanPartition,
};
use crate::error::Result;
use crate::ffi::duckdb_c::*;
use crate::ffi::{ffi_bind_guard, ffi_init_guard, ffi_scan_guard, to_c_string};
use crate::functions::bind_utils::{
    add_named_parameter, add_positional_parameter, delete_boxed,
    register_common_raster_named_parameters, set_table_function_init_data, BindHelper, ChunkWriter,
};
use crate::raster::mosaic::MosaicReader;

/// Name of the column a partitioned scan appends.
pub const PARTIAL_STATE_COLUMN: &str = "partial_state";

/// Upper bound on one decoded partial state, guarding against corrupt input.
const STATE_LIMIT: usize = 64 << 20;

// -----------------------------------------------------------------------------------------
// Scan support
// -----------------------------------------------------------------------------------------

/// One queued output row of a raster scan: the record, plus its encoded state when it is a
/// partial (boundary) cell of a partitioned scan. Dereferences to the record.
pub struct ScanRow<R> {
    pub record: R,
    pub partial_state: Option<Box<[u8]>>,
}

impl<R> Deref for ScanRow<R> {
    type Target = R;

    #[inline(always)]
    fn deref(&self) -> &R {
        &self.record
    }
}

/// The `partition` named parameter, or `Err` with a message for an invalid spec.
pub fn parse_partition_param(
    bind: &BindHelper,
) -> std::result::Result<Option<ScanPartition>, String> {
    match bind.get_named_string("partition") {
        None => Ok(None),
        Some(spec) => ScanPartition::from_json(&spec)
            .map(Some)
            .map_err(|e| e.to_string()),
    }
}

/// Drain completed rows of a continuous scan into `f` (records, then partials).
pub fn drain_continuous(
    streamer: &mut MultiScanHorizonStreamer,
    max_rows: usize,
    partitioned: bool,
    f: &mut dyn FnMut(usize, ScanRow<MultiContinuousRecord>),
) -> Result<usize> {
    if !partitioned {
        return streamer.drain_completed_into(max_rows, |i, record| {
            f(
                i,
                ScanRow {
                    record,
                    partial_state: None,
                },
            )
        });
    }
    let mut partials = Vec::new();
    let n = streamer.drain_partitioned_into(
        max_rows,
        |i, record| {
            f(
                i,
                ScanRow {
                    record,
                    partial_state: None,
                },
            )
        },
        |p| partials.push(p),
    )?;
    for (i, p) in partials.into_iter().enumerate() {
        let state = encode_partial_state(&p.accumulator).into_boxed_slice();
        let record = MultiContinuousRecord {
            resolution: p.resolution,
            h3_index: p.h3_index,
            accumulator: p.accumulator,
        };
        f(
            i,
            ScanRow {
                record,
                partial_state: Some(state),
            },
        );
    }
    Ok(n)
}

/// Drain completed rows of a (wide) categorical scan into `f` (records, then partials).
pub fn drain_categorical(
    streamer: &mut MultiCategoricalHorizonStreamer,
    max_rows: usize,
    partitioned: bool,
    f: &mut dyn FnMut(usize, ScanRow<MultiCategoricalRecord>),
) -> Result<usize> {
    if !partitioned {
        return streamer.drain_completed_into(max_rows, |i, record| {
            f(
                i,
                ScanRow {
                    record,
                    partial_state: None,
                },
            )
        });
    }
    let mut partials = Vec::new();
    let n = streamer.drain_partitioned_into(
        max_rows,
        |i, record| {
            f(
                i,
                ScanRow {
                    record,
                    partial_state: None,
                },
            )
        },
        |p| partials.push(p),
    )?;
    for (i, p) in partials.into_iter().enumerate() {
        let state = encode_partial_state(&p.accumulator).into_boxed_slice();
        let record = MultiCategoricalRecord {
            resolution: p.resolution,
            h3_index: p.h3_index,
            accumulator: p.accumulator,
        };
        f(
            i,
            ScanRow {
                record,
                partial_state: Some(state),
            },
        );
    }
    Ok(n)
}

/// Write the `partial_state` column of a scan batch.
///
/// # Safety
/// `writer` must wrap a live output chunk whose column `out_idx` is BLOB.
pub unsafe fn write_partial_state_column<R>(
    writer: &ChunkWriter,
    out_idx: usize,
    batch: &[ScanRow<R>],
) {
    for (i, row) in batch.iter().enumerate() {
        // SAFETY: column `out_idx` of the live chunk is BLOB (caller contract), and `i` is
        // below the batch length, which never exceeds the vector size.
        unsafe {
            match &row.partial_state {
                Some(state) => writer.set_string_bytes(out_idx, i, state),
                None => writer.set_null(out_idx, i),
            }
        }
    }
}

// -----------------------------------------------------------------------------------------
// h3_raster_plan_partitions
// -----------------------------------------------------------------------------------------

struct PlanBindData {
    specs: Vec<String>,
}

struct PlanGlobalData {
    next: AtomicUsize,
}

/// # Safety
/// `info` must be the valid bind info DuckDB passes to this callback.
unsafe extern "C" fn plan_bind(info: duckdb_bind_info) {
    // SAFETY: DuckDB passes this bind callback a live `info`, wrapped by a `BindHelper`
    // used only inside the callback. The boxed `PlanBindData` is handed to DuckDB with its
    // matching destructor, which DuckDB calls exactly once.
    unsafe {
        ffi_bind_guard(info, || {
            let bind = BindHelper::new(info);
            let Some(common) = bind.parse_common_raster_params("h3_raster_plan_partitions") else {
                return;
            };
            let target = bind.get_named_int("target_chunks").unwrap_or(256);
            if target < 1 {
                bind.set_error("h3_raster_plan_partitions: target_chunks must be at least 1");
                return;
            }
            // Open the mosaic exactly as the scans do, so the fingerprints match.
            let plan = MosaicReader::open_with_remote_source_factory(
                &common.resolved_paths,
                common.bbox,
                common.source_crs.as_deref(),
                common.overlap_rule,
                common.remote_source_factory.as_ref(),
            )
            .and_then(|mosaic| plan_partitions(&mosaic, target as usize));
            let plan = match plan {
                Ok(plan) => plan,
                Err(e) => {
                    bind.set_error(&format!("h3_raster_plan_partitions: {e}"));
                    return;
                }
            };
            bind.add_result_column("partition_index", DuckDBType::BigInt);
            bind.add_result_column("partition_count", DuckDBType::BigInt);
            bind.add_result_column("spec", DuckDBType::Varchar);
            duckdb_bind_set_cardinality(info, plan.len() as idx_t, true);
            let data = Box::new(PlanBindData {
                specs: plan.iter().map(ScanPartition::to_json).collect(),
            });
            duckdb_bind_set_bind_data(
                info,
                Box::into_raw(data) as *mut c_void,
                Some(delete_boxed::<PlanBindData>),
            );
        });
    }
}

/// # Safety
/// `info` must be the valid init info DuckDB passes to this callback.
unsafe extern "C" fn plan_init(info: duckdb_init_info) {
    // SAFETY: DuckDB passes this init callback a live `info`; the global data is `Send + Sync`.
    unsafe {
        ffi_init_guard(info, || {
            set_table_function_init_data(
                info,
                PlanGlobalData {
                    next: AtomicUsize::new(0),
                },
            );
        });
    }
}

/// # Safety
/// `info` and `output` must be the handles DuckDB passes to this scan callback, with the
/// three columns declared at bind time (no projection pushdown).
unsafe extern "C" fn plan_scan(info: duckdb_function_info, output: duckdb_data_chunk) {
    // SAFETY: the bind/init pointers are the boxes installed by `plan_bind` / `plan_init`
    // (null-checked), alive for the whole query and only borrowed immutably. Column types
    // match the declared BIGINT, BIGINT, VARCHAR.
    unsafe {
        ffi_scan_guard(info, output, || {
            let bind = duckdb_function_get_bind_data(info) as *const PlanBindData;
            let global = duckdb_function_get_init_data(info) as *const PlanGlobalData;
            if bind.is_null() || global.is_null() {
                duckdb_data_chunk_set_size(output, 0);
                return;
            }
            let (bind, global) = (&*bind, &*global);
            let writer = ChunkWriter::new(output);
            let capacity = get_vector_size();
            let start = global
                .next
                .fetch_add(capacity, Ordering::Relaxed)
                .min(bind.specs.len());
            let end = (start + capacity).min(bind.specs.len());
            let count = bind.specs.len() as i64;
            for (row, i) in (start..end).enumerate() {
                writer.set_int64(0, row, i as i64);
                writer.set_int64(1, row, count);
                writer.set_string(2, row, &bind.specs[i]);
            }
            writer.set_size(end - start);
        });
    }
}

/// Register `h3_raster_plan_partitions`.
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn register_plan_partitions_function(
    con: duckdb_connection,
) -> std::result::Result<(), String> {
    // SAFETY: `con` is live (caller contract); the table function is configured with
    // NUL-terminated names that outlive the calls and C-ABI callbacks, and destroyed once
    // after registration copied it.
    unsafe {
        let name = to_c_string("h3_raster_plan_partitions");
        let tf = duckdb_create_table_function();
        duckdb_table_function_set_name(tf, name.as_ptr());
        add_positional_parameter(tf, DuckDBType::Varchar);
        register_common_raster_named_parameters(tf);
        add_named_parameter(tf, "target_chunks", DuckDBType::BigInt);
        duckdb_table_function_set_bind(tf, plan_bind);
        duckdb_table_function_set_init(tf, plan_init);
        duckdb_table_function_set_function(tf, plan_scan);
        let state = duckdb_register_table_function(con, tf);
        let mut tf = tf;
        duckdb_destroy_table_function(&mut tf);
        if state != DuckDBState::Success {
            return Err("Failed to register h3_raster_plan_partitions".into());
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------------------
// h3_merge_partials / h3_merge_categorical_partials
// -----------------------------------------------------------------------------------------

/// Columns of the returned STRUCT: name, type.
const CONTINUOUS_FIELDS: &[(&str, DuckDBType)] = &[
    ("mean", DuckDBType::Double),
    ("stddev", DuckDBType::Double),
    ("count", DuckDBType::Double),
    ("min", DuckDBType::Double),
    ("max", DuckDBType::Double),
    ("sum", DuckDBType::Double),
];

const CATEGORICAL_FIELDS: &[(&str, DuckDBType)] = &[
    ("majority_class", DuckDBType::BigInt),
    ("majority_fraction", DuckDBType::Double),
    ("majority_count", DuckDBType::Double),
    ("unique_classes", DuckDBType::BigInt),
    ("total_count", DuckDBType::Double),
    ("histogram", DuckDBType::Varchar),
    ("shannon_entropy", DuckDBType::Double),
    ("entropy", DuckDBType::Double),
    ("distinct_classes", DuckDBType::BigInt),
];

/// A merged cell, boxed behind the pointer stored in DuckDB's aggregate state.
trait MergeState: Sized {
    const FIELDS: &'static [(&'static str, DuckDBType)];
    fn decode(bytes: &[u8]) -> Result<Self>;
    fn merge(&mut self, other: &Self);
    /// Write this cell's statistics into row `row` of the struct's child vectors.
    ///
    /// # Safety
    /// `children` are the live child vectors of the result struct, typed as `FIELDS`, with
    /// capacity above `row`.
    unsafe fn write(&self, children: &[duckdb_vector], row: usize);
}

/// # Safety
/// `v` must be a live, writable vector of `T` with capacity above `row`.
unsafe fn put<T>(v: duckdb_vector, row: usize, value: T) {
    // SAFETY: caller contract.
    unsafe { *(duckdb_vector_get_data(v) as *mut T).add(row) = value };
}

impl MergeState for H3Accumulator {
    const FIELDS: &'static [(&'static str, DuckDBType)] = CONTINUOUS_FIELDS;

    fn decode(bytes: &[u8]) -> Result<Self> {
        decode_partial_state(bytes, STATE_LIMIT)
    }

    fn merge(&mut self, other: &Self) {
        H3Accumulator::merge(self, other);
    }

    unsafe fn write(&self, c: &[duckdb_vector], row: usize) {
        // SAFETY: the children are DOUBLE vectors (`CONTINUOUS_FIELDS`) with capacity above
        // `row` (caller contract). Values match the continuous scan's columns.
        unsafe {
            put(c[0], row, self.mean());
            put(c[1], row, self.stddev());
            put(c[2], row, self.count);
            put(c[3], row, self.min);
            put(c[4], row, self.max);
            put(c[5], row, self.sum);
        }
    }
}

impl MergeState for CategoricalAccumulator {
    const FIELDS: &'static [(&'static str, DuckDBType)] = CATEGORICAL_FIELDS;

    fn decode(bytes: &[u8]) -> Result<Self> {
        decode_partial_state(bytes, STATE_LIMIT)
    }

    fn merge(&mut self, other: &Self) {
        CategoricalAccumulator::merge(self, other);
    }

    unsafe fn write(&self, c: &[duckdb_vector], row: usize) {
        let (class, count, fraction) = self.majority();
        let distinct = self.unique_classes() as i64;
        let entropy = self.shannon_entropy();
        let mut histogram = String::with_capacity(64);
        self.histogram_json_into(&mut histogram);
        // SAFETY: the children have the `CATEGORICAL_FIELDS` types and capacity above `row`
        // (caller contract); DuckDB copies the histogram bytes. Values match the wide
        // categorical scan's columns.
        unsafe {
            put(c[0], row, class);
            put(c[1], row, fraction);
            put(c[2], row, count);
            put(c[3], row, distinct);
            put(c[4], row, self.total_count);
            duckdb_vector_assign_string_element_len(
                c[5],
                row as idx_t,
                histogram.as_ptr() as *const c_char,
                histogram.len() as idx_t,
            );
            put(c[6], row, entropy);
            put(c[7], row, entropy);
            put(c[8], row, distinct);
        }
    }
}

/// The pointer slot DuckDB allocates for each group's state.
type Slot<S> = *mut *mut S;

/// Report a failure from an aggregate callback (errors and panics).
///
/// # Safety
/// `info` must be the function info DuckDB passed to the running callback.
unsafe fn agg_guard(info: duckdb_function_info, body: impl FnOnce() -> Result<()>) {
    let message = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(Ok(())) => return,
        Ok(Err(e)) => e.to_string(),
        Err(_) => "panic in raster_h3 partial-state merge".to_string(),
    };
    let message = CString::new(message).unwrap_or_default();
    // SAFETY: `info` is live (caller contract); DuckDB copies the message.
    unsafe { duckdb_aggregate_function_set_error(info, message.as_ptr()) };
}

unsafe extern "C" fn state_size(_info: duckdb_function_info) -> idx_t {
    size_of::<*mut c_void>() as idx_t
}

unsafe extern "C" fn state_init<S: MergeState>(
    _info: duckdb_function_info,
    state: duckdb_aggregate_state,
) {
    // SAFETY: `state` points to `state_size` (one pointer) writable bytes owned by DuckDB.
    unsafe { *(state as Slot<S>) = std::ptr::null_mut() };
}

unsafe extern "C" fn state_destroy<S: MergeState>(
    states: *mut duckdb_aggregate_state,
    count: idx_t,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for i in 0..count as usize {
            // SAFETY: `states` holds `count` initialized slots; each non-null pointer is a
            // `Box<S>` we own, freed here exactly once (combine nulls moved-out slots).
            unsafe {
                let slot = *states.add(i) as Slot<S>;
                let boxed = std::mem::replace(&mut *slot, std::ptr::null_mut());
                if !boxed.is_null() {
                    drop(Box::from_raw(boxed));
                }
            }
        }
    }));
}

unsafe extern "C" fn state_update<S: MergeState>(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    states: *mut duckdb_aggregate_state,
) {
    // SAFETY: DuckDB passes a live input chunk with one BLOB column and one state slot per
    // row; BLOB values are `duckdb_string_t`s valid for the duration of the call.
    unsafe {
        agg_guard(info, || {
            let rows = duckdb_data_chunk_get_size(input) as usize;
            let vector = duckdb_data_chunk_get_vector(input, 0);
            let data = duckdb_vector_get_data(vector) as *const duckdb_string_t;
            let validity = duckdb_vector_get_validity(vector);
            for row in 0..rows {
                if !duckdb_validity_is_valid(validity, row as idx_t) {
                    continue;
                }
                let cell = S::decode((*data.add(row)).as_bytes())?;
                let slot = *states.add(row) as Slot<S>;
                match (*slot).as_mut() {
                    Some(acc) => acc.merge(&cell),
                    None => *slot = Box::into_raw(Box::new(cell)),
                }
            }
            Ok(())
        });
    }
}

unsafe extern "C" fn state_combine<S: MergeState>(
    info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    target: *mut duckdb_aggregate_state,
    count: idx_t,
) {
    // SAFETY: both arrays hold `count` initialized slots. A moved-out source pointer is
    // nulled, so each box is owned by exactly one slot.
    unsafe {
        agg_guard(info, || {
            for i in 0..count as usize {
                let src = *source.add(i) as Slot<S>;
                let tgt = *target.add(i) as Slot<S>;
                let moved = std::mem::replace(&mut *src, std::ptr::null_mut());
                if moved.is_null() {
                    continue;
                }
                match (*tgt).as_mut() {
                    Some(acc) => {
                        let other = Box::from_raw(moved);
                        acc.merge(&other);
                    }
                    None => *tgt = moved,
                }
            }
            Ok(())
        });
    }
}

unsafe extern "C" fn state_finalize<S: MergeState>(
    info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    result: duckdb_vector,
    count: idx_t,
    offset: idx_t,
) {
    // SAFETY: `result` is the live STRUCT result vector with `S::FIELDS` children and
    // capacity for rows `offset..offset + count`; `source` holds `count` initialized slots.
    unsafe {
        agg_guard(info, || {
            let children: Vec<duckdb_vector> = (0..S::FIELDS.len())
                .map(|k| duckdb_struct_vector_get_child(result, k as idx_t))
                .collect();
            for i in 0..count as usize {
                let row = offset as usize + i;
                let slot = *source.add(i) as Slot<S>;
                match (*slot).as_ref() {
                    Some(acc) => acc.write(&children, row),
                    None => {
                        duckdb_vector_set_row_invalid(result, row as idx_t);
                        for &child in &children {
                            duckdb_vector_set_row_invalid(child, row as idx_t);
                        }
                    }
                }
            }
            Ok(())
        });
    }
}

/// # Safety
/// `con` must be a valid, open DuckDB connection.
unsafe fn register_merge<S: MergeState>(
    con: duckdb_connection,
    name: &str,
) -> std::result::Result<(), String> {
    // SAFETY: `con` is live (caller contract). Logical types are destroyed after DuckDB
    // copied them; names are NUL-terminated and outlive the calls; the aggregate is
    // destroyed once after registration copied it.
    unsafe {
        let mut member_types: Vec<duckdb_logical_type> = S::FIELDS
            .iter()
            .map(|(_, t)| duckdb_create_logical_type(*t))
            .collect();
        let member_names: Vec<CString> = S::FIELDS.iter().map(|(n, _)| to_c_string(n)).collect();
        let mut member_name_ptrs: Vec<*const c_char> =
            member_names.iter().map(|n| n.as_ptr()).collect();
        let mut return_type = duckdb_create_struct_type(
            member_types.as_mut_ptr(),
            member_name_ptrs.as_mut_ptr(),
            member_types.len() as idx_t,
        );
        let mut blob = duckdb_create_logical_type(DuckDBType::Blob);

        let fn_name = to_c_string(name);
        let agg = duckdb_create_aggregate_function();
        duckdb_aggregate_function_set_name(agg, fn_name.as_ptr());
        duckdb_aggregate_function_add_parameter(agg, blob);
        duckdb_aggregate_function_set_return_type(agg, return_type);
        duckdb_aggregate_function_set_functions(
            agg,
            state_size,
            state_init::<S>,
            state_update::<S>,
            state_combine::<S>,
            state_finalize::<S>,
        );
        duckdb_aggregate_function_set_destructor(agg, state_destroy::<S>);
        let state = duckdb_register_aggregate_function(con, agg);

        let mut agg = agg;
        duckdb_destroy_aggregate_function(&mut agg);
        duckdb_destroy_logical_type(&mut blob);
        duckdb_destroy_logical_type(&mut return_type);
        for t in &mut member_types {
            duckdb_destroy_logical_type(t);
        }
        if state != DuckDBState::Success {
            return Err(format!("Failed to register {name}"));
        }
        Ok(())
    }
}

/// Register `h3_merge_partials` and `h3_merge_categorical_partials`.
///
/// # Safety
/// `con` must be a valid, open DuckDB connection.
pub unsafe fn register_merge_functions(con: duckdb_connection) -> std::result::Result<(), String> {
    // SAFETY: forwards the caller's live connection.
    unsafe {
        register_merge::<H3Accumulator>(con, "h3_merge_partials")?;
        register_merge::<CategoricalAccumulator>(con, "h3_merge_categorical_partials")
    }
}

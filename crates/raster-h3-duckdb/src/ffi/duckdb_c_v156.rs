//! Wrappers for the DuckDB stable C extension API **v1.5.6** surface.
//!
//! These entry points only exist in hosts that supply a `duckdb_ext_api_v1`
//! table of at least [`super::duckdb_c::API_TABLE_LEN`] entries (548 in the
//! v1.5.6 table shipped with DuckDB 2.0.0). The whole module is compiled only
//! for the `duckdb-api-v1_5_6` feature build, which requests exactly that API
//! version at load time.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::atomic::AtomicPtr;

use super::duckdb_c::*;

/// Declares DuckDB C API functions as wrappers that call through the
/// `duckdb_ext_api_v1` table, like `duckdb_api!` in `duckdb_c.rs` but for the
/// v1.5.6 table positions transcribed from DuckDB v2.0.0's
/// `duckdb_extension.h`.
macro_rules! duckdb_api_v156 {
    ($(
        [$index:literal] pub fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?;
    )*) => {
        $(
            /// # Safety
            /// Same contract as the DuckDB C API function of this name.
            #[inline(always)]
            pub unsafe fn $name($($arg: $ty),*) $(-> $ret)? {
                static FALLBACK: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
                // SAFETY: `resolve_api_fn` never returns null (it panics
                // instead), and slot `$index` of `duckdb_ext_api_v1` v1.5.6
                // (or the exported symbol of the same name) has exactly this C
                // signature, as transcribed from `duckdb_extension.h`. Calling
                // it is then sound under this wrapper's own contract, which is
                // DuckDB's.
                unsafe {
                    let func: unsafe extern "C" fn($($ty),*) $(-> $ret)? = std::mem::transmute(
                        super::duckdb_c::resolve_api_fn(
                            $index,
                            concat!(stringify!($name), "\0"),
                            &FALLBACK,
                        ),
                    );
                    func($($arg),*)
                }
            }
        )*

        /// Field positions of the wrapped functions in the v1.5.6
        /// `duckdb_ext_api_v1`, for building a substitute table (test mocks).
        #[allow(non_upper_case_globals)]
        pub mod api_index_v156 {
            $(pub const $name: usize = $index;)*
        }
    };
}

duckdb_api_v156! {
    // Session configuration options
    [423] pub fn duckdb_create_config_option() -> duckdb_config_option;
    [424] pub fn duckdb_destroy_config_option(option: *mut duckdb_config_option);
    [425] pub fn duckdb_config_option_set_name(option: duckdb_config_option, name: *const c_char);
    [426] pub fn duckdb_config_option_set_type(
        option: duckdb_config_option,
        type_: duckdb_logical_type,
    );
    [427] pub fn duckdb_config_option_set_default_value(
        option: duckdb_config_option,
        default_value: duckdb_value,
    );
    [428] pub fn duckdb_config_option_set_default_scope(
        option: duckdb_config_option,
        default_scope: DuckDBConfigOptionScope,
    );
    [429] pub fn duckdb_config_option_set_description(
        option: duckdb_config_option,
        description: *const c_char,
    );
    [430] pub fn duckdb_register_config_option(
        con: duckdb_connection,
        option: duckdb_config_option,
    ) -> DuckDBState;
    [431] pub fn duckdb_client_context_get_config_option(
        context: duckdb_client_context,
        name: *const c_char,
        out_scope: *mut DuckDBConfigOptionScope,
    ) -> duckdb_value;

    // COPY functions
    [432] pub fn duckdb_create_copy_function() -> duckdb_copy_function;
    [433] pub fn duckdb_copy_function_set_name(
        copy_function: duckdb_copy_function,
        name: *const c_char,
    );
    [435] pub fn duckdb_register_copy_function(
        con: duckdb_connection,
        copy_function: duckdb_copy_function,
    ) -> DuckDBState;
    [436] pub fn duckdb_destroy_copy_function(copy_function: *mut duckdb_copy_function);
    [437] pub fn duckdb_copy_function_set_bind(
        copy_function: duckdb_copy_function,
        bind: duckdb_copy_function_bind_t,
    );
    [438] pub fn duckdb_copy_function_bind_set_error(
        info: duckdb_copy_function_bind_info,
        error: *const c_char,
    );
    [440] pub fn duckdb_copy_function_bind_get_client_context(info: duckdb_copy_function_bind_info) -> duckdb_client_context;
    [441] pub fn duckdb_copy_function_bind_get_column_count(
        info: duckdb_copy_function_bind_info,
    ) -> idx_t;
    [442] pub fn duckdb_copy_function_bind_get_column_type(
        info: duckdb_copy_function_bind_info,
        column_idx: idx_t,
    ) -> duckdb_logical_type;
    [443] pub fn duckdb_copy_function_bind_get_options(
        info: duckdb_copy_function_bind_info,
    ) -> duckdb_value;
    [444] pub fn duckdb_copy_function_bind_set_bind_data(
        info: duckdb_copy_function_bind_info,
        bind_data: *mut c_void,
        callback: Option<duckdb_delete_callback_t>,
    );
    [445] pub fn duckdb_copy_function_set_global_init(
        copy_function: duckdb_copy_function,
        init: duckdb_copy_function_global_init_t,
    );
    [446] pub fn duckdb_copy_function_global_init_set_error(
        info: duckdb_copy_function_global_init_info,
        error: *const c_char,
    );
    [448] pub fn duckdb_copy_function_global_init_get_client_context(info: duckdb_copy_function_global_init_info) -> duckdb_client_context;
    [449] pub fn duckdb_copy_function_global_init_get_bind_data(
        info: duckdb_copy_function_global_init_info,
    ) -> *mut c_void;
    [450] pub fn duckdb_copy_function_global_init_set_global_state(
        info: duckdb_copy_function_global_init_info,
        global_state: *mut c_void,
        callback: Option<duckdb_delete_callback_t>,
    );
    [451] pub fn duckdb_copy_function_global_init_get_file_path(
        info: duckdb_copy_function_global_init_info,
    ) -> *const c_char;
    [452] pub fn duckdb_copy_function_set_sink(
        copy_function: duckdb_copy_function,
        sink: duckdb_copy_function_sink_t,
    );
    [453] pub fn duckdb_copy_function_sink_set_error(
        info: duckdb_copy_function_sink_info,
        error: *const c_char,
    );
    [455] pub fn duckdb_copy_function_sink_get_client_context(info: duckdb_copy_function_sink_info) -> duckdb_client_context;
    [456] pub fn duckdb_copy_function_sink_get_bind_data(
        info: duckdb_copy_function_sink_info,
    ) -> *mut c_void;
    [457] pub fn duckdb_copy_function_sink_get_global_state(
        info: duckdb_copy_function_sink_info,
    ) -> *mut c_void;
    [458] pub fn duckdb_copy_function_set_finalize(
        copy_function: duckdb_copy_function,
        finalize: duckdb_copy_function_finalize_t,
    );
    [459] pub fn duckdb_copy_function_finalize_set_error(
        info: duckdb_copy_function_finalize_info,
        error: *const c_char,
    );
    [461] pub fn duckdb_copy_function_finalize_get_client_context(info: duckdb_copy_function_finalize_info) -> duckdb_client_context;
    [462] pub fn duckdb_copy_function_finalize_get_bind_data(
        info: duckdb_copy_function_finalize_info,
    ) -> *mut c_void;
    [463] pub fn duckdb_copy_function_finalize_get_global_state(
        info: duckdb_copy_function_finalize_info,
    ) -> *mut c_void;

    // Error data
    [469] pub fn duckdb_destroy_error_data(error_data: *mut duckdb_error_data);
    [471] pub fn duckdb_error_data_message(error_data: duckdb_error_data) -> *const c_char;
    [472] pub fn duckdb_error_data_has_error(error_data: duckdb_error_data) -> bool;

    // Expressions (constant folding in scalar binds)
    [473] pub fn duckdb_destroy_expression(expr: *mut duckdb_expression);
    [474] pub fn duckdb_expression_return_type(expr: duckdb_expression) -> duckdb_logical_type;
    [475] pub fn duckdb_expression_is_foldable(expr: duckdb_expression) -> bool;
    [476] pub fn duckdb_expression_fold(
        context: duckdb_client_context,
        expr: duckdb_expression,
        out_value: *mut duckdb_value,
    ) -> duckdb_error_data;

    // File system
    [477] pub fn duckdb_client_context_get_file_system(
        context: duckdb_client_context,
    ) -> duckdb_file_system;
    [478] pub fn duckdb_destroy_file_system(file_system: *mut duckdb_file_system);
    [479] pub fn duckdb_file_system_open(
        file_system: duckdb_file_system,
        path: *const c_char,
        options: duckdb_file_open_options,
        out_handle: *mut duckdb_file_handle,
    ) -> DuckDBState;
    [480] pub fn duckdb_file_system_error_data(
        file_system: duckdb_file_system,
    ) -> duckdb_error_data;
    [481] pub fn duckdb_create_file_open_options() -> duckdb_file_open_options;
    [482] pub fn duckdb_file_open_options_set_flag(
        options: duckdb_file_open_options,
        flag: DuckDBFileFlag,
        value: bool,
    ) -> DuckDBState;
    [483] pub fn duckdb_destroy_file_open_options(
        file_open_options: *mut duckdb_file_open_options,
    );
    [484] pub fn duckdb_destroy_file_handle(file_handle: *mut duckdb_file_handle);
    [485] pub fn duckdb_file_handle_error_data(
        file_handle: duckdb_file_handle,
    ) -> duckdb_error_data;
    [486] pub fn duckdb_file_handle_close(file_handle: duckdb_file_handle) -> DuckDBState;
    [487] pub fn duckdb_file_handle_read(
        file_handle: duckdb_file_handle,
        buffer: *mut c_void,
        size: i64,
    ) -> i64;
    [488] pub fn duckdb_file_handle_write(
        file_handle: duckdb_file_handle,
        buffer: *const c_void,
        size: i64,
    ) -> i64;
    [489] pub fn duckdb_file_handle_seek(file_handle: duckdb_file_handle, location: i64)
        -> DuckDBState;
    [490] pub fn duckdb_file_handle_tell(file_handle: duckdb_file_handle) -> i64;
    [491] pub fn duckdb_file_handle_sync(file_handle: duckdb_file_handle) -> DuckDBState;
    [492] pub fn duckdb_file_handle_size(file_handle: duckdb_file_handle) -> i64;

    // Native geometry
    [493] pub fn duckdb_geometry_type_get_crs(
        geometry_type: duckdb_logical_type,
    ) -> *mut c_char;

    // Client contexts
    [501] pub fn duckdb_destroy_client_context(context: *mut duckdb_client_context);
    [502] pub fn duckdb_connection_get_client_context(
        connection: duckdb_connection,
        out_context: *mut duckdb_client_context,
    );

    // Scalar function bind / init (v1.5.6)
    [511] pub fn duckdb_scalar_function_set_bind(
        function: duckdb_scalar_function,
        bind: duckdb_scalar_function_bind_t,
    );
    [512] pub fn duckdb_scalar_function_bind_set_error(
        info: duckdb_bind_info,
        error: *const c_char,
    );
    [513] pub fn duckdb_scalar_function_get_client_context(
        info: duckdb_function_info,
        out_context: *mut duckdb_client_context,
    );
    [514] pub fn duckdb_scalar_function_set_bind_data(
        info: duckdb_bind_info,
        bind_data: *mut c_void,
        callback: Option<duckdb_delete_callback_t>,
    );
    [515] pub fn duckdb_scalar_function_get_bind_data(info: duckdb_function_info)
        -> *mut c_void;
    [517] pub fn duckdb_scalar_function_bind_get_argument_count(
        info: duckdb_bind_info,
    ) -> idx_t;
    [518] pub fn duckdb_scalar_function_bind_get_argument(
        info: duckdb_bind_info,
        index: idx_t,
    ) -> duckdb_expression;
    [519] pub fn duckdb_scalar_function_set_bind_data_copy(
        function: duckdb_scalar_function,
        callback: Option<duckdb_copy_callback_t>,
    );
    [520] pub fn duckdb_scalar_function_get_state(info: duckdb_function_info) -> *mut c_void;
    [521] pub fn duckdb_scalar_function_set_init(
        function: duckdb_scalar_function,
        init: duckdb_scalar_function_init_t,
    );
    [522] pub fn duckdb_scalar_function_init_set_error(
        info: duckdb_init_info,
        error: *const c_char,
    );
    [523] pub fn duckdb_scalar_function_init_set_state(
        info: duckdb_init_info,
        state: *mut c_void,
        callback: Option<duckdb_delete_callback_t>,
    );
    [527] pub fn duckdb_value_to_string(value: duckdb_value) -> *mut c_char;

    // Table function client context (bind-time)
    [531] pub fn duckdb_table_function_get_client_context(
        info: duckdb_bind_info,
        out_context: *mut duckdb_client_context,
    );
}

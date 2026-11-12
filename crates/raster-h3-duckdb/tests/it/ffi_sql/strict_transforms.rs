//! `strict_transforms` failures in row scans and exports.

use std::collections::HashMap;
use std::ptr;

use raster_h3_duckdb::ffi::duckdb_c::*;
use raster_h3_duckdb::functions::parquet_table_function::{
    parquet_bind, parquet_init, parquet_scan,
};

use super::mock_api::install_mock_api;
use super::{MockBindContext, MockDataChunk, MockFuncContext, MockInitContext, MockVal};
use crate::helpers::TestGeoTiffBuilder;

/// Bind, init and scan one table function until it ends or fails; returns the
/// scan error, if any.
///
/// # Safety
/// The callbacks must be one table function's bind/init/local-init/scan set,
/// with the mock C API installed.
unsafe fn scan_to_end(
    named: HashMap<String, MockVal>,
    path: &std::path::Path,
    bind: unsafe extern "C" fn(duckdb_bind_info),
    init: unsafe extern "C" fn(duckdb_init_info),
    init_local: unsafe extern "C" fn(duckdb_init_info),
    scan: unsafe extern "C" fn(duckdb_function_info, duckdb_data_chunk),
) -> Option<String> {
    // SAFETY: the contexts are live mock contexts that the installed mock C API treats as
    // the bind/init/function infos, and bind data stays owned by `bind_ctx` throughout.
    unsafe {
        let mut bind_ctx = MockBindContext::new(
            vec![MockVal::Str(path.to_string_lossy().into_owned())],
            named,
        );
        bind(&mut bind_ctx as *mut MockBindContext as duckdb_bind_info);
        assert!(bind_ctx.error.is_none(), "{:?}", bind_ctx.error);
        let cols = bind_ctx.result_columns.len();
        let mut init_ctx = MockInitContext::new(bind_ctx.bind_data, (0..cols).collect());
        let mut local_ctx = MockInitContext::new(bind_ctx.bind_data, Vec::new());
        init(&mut init_ctx as *mut MockInitContext as duckdb_init_info);
        init_local(&mut local_ctx as *mut MockInitContext as duckdb_init_info);
        assert!(init_ctx.error.is_none(), "{:?}", init_ctx.error);
        let mut func = MockFuncContext {
            bind_data: bind_ctx.bind_data,
            init_data: init_ctx.init_data,
            local_init_data: local_ctx.init_data,
            error: None,
        };
        loop {
            let mut chunk = MockDataChunk::new(cols, 2048);
            scan(
                &mut func as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
            if func.error.is_some() || chunk.size == 0 {
                return func.error;
            }
        }
    }
}

#[test]
fn test_strict_transforms_fails_row_scans() {
    install_mock_api();
    // 10 x 20 one-degree pixels from 95N: the top 5 rows lie beyond the pole.
    let (_tmp, path) = TestGeoTiffBuilder::new(10, 20)
        .origin(0.0, 95.0)
        .pixel_size(1.0)
        .create_constant_tempfile(3.0f32);
    let named = |strict: Option<bool>| {
        let mut named = HashMap::new();
        named.insert("resolution".to_string(), MockVal::Int(3));
        named.insert("geom".to_string(), MockVal::Bool(false));
        if let Some(strict) = strict {
            named.insert("strict_transforms".to_string(), MockVal::Bool(strict));
        }
        named
    };

    // SAFETY: each call passes one table function's matching callback set.
    unsafe {
        for strict in [None, Some(false)] {
            let continuous = scan_to_end(
                named(strict),
                &path,
                raster_h3_bind,
                raster_h3_init,
                raster_h3_init_local,
                raster_h3_scan,
            );
            assert_eq!(continuous, None, "lenient scans skip the failed samples");
        }
        let continuous = scan_to_end(
            named(Some(true)),
            &path,
            raster_h3_bind,
            raster_h3_init,
            raster_h3_init_local,
            raster_h3_scan,
        )
        .expect("strict continuous scan must fail");
        assert!(continuous.contains("strict_transforms"), "{continuous}");
        let categorical = scan_to_end(
            named(Some(true)),
            &path,
            raster_h3_categorical_bind,
            raster_h3_categorical_init,
            raster_h3_categorical_init_local,
            raster_h3_categorical_scan,
        )
        .expect("strict categorical scan must fail");
        assert!(categorical.contains("strict_transforms"), "{categorical}");
    }
}

#[test]
fn test_strict_transforms_fails_exports_with_the_count() {
    install_mock_api();
    let (_tmp, tif_path) = TestGeoTiffBuilder::new(10, 20)
        .origin(0.0, 95.0)
        .pixel_size(1.0)
        .create_constant_tempfile(3.0f32);
    let out_dir = tempfile::tempdir().unwrap();
    let parquet_path = out_dir.path().join("strict.parquet");

    let mut named = HashMap::new();
    named.insert("resolution".to_string(), MockVal::Int(3));
    named.insert("strict_transforms".to_string(), MockVal::Bool(true));
    let mut bind = MockBindContext::new(
        vec![
            MockVal::Str(tif_path.to_string_lossy().into_owned()),
            MockVal::Str(parquet_path.to_string_lossy().into_owned()),
        ],
        named,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        parquet_bind(&mut bind as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind.error.is_none());
    let mut init = MockInitContext::new(bind.bind_data, (0..7).collect());
    // SAFETY: the context is a live `MockInitContext` carrying the bind data installed
    // by the bind callback, which is still owned by its bind context.
    unsafe {
        parquet_init(&mut init as *mut MockInitContext as duckdb_init_info);
    }
    let mut func = MockFuncContext {
        bind_data: bind.bind_data,
        init_data: init.init_data,
        local_init_data: ptr::null_mut(),
        error: None,
    };
    let mut chunk = MockDataChunk::new(7, 2048);
    // SAFETY: the function context carries bind/init data installed above (still owned
    // by their live contexts), and the chunk has one 2048-row vector per column.
    unsafe {
        parquet_scan(
            &mut func as *mut MockFuncContext as duckdb_function_info,
            &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
        );
    }
    assert_eq!(chunk.size, 1);
    let status = std::str::from_utf8(&chunk.vectors[5].strings[0]).unwrap();
    assert!(
        status.starts_with("ERROR: ") && status.contains("strict_transforms"),
        "{status}"
    );
    // SAFETY: column 6 is the BIGINT `transform_failures`, written for row 0 above.
    let failures = unsafe { *(chunk.vectors[6].data.as_ptr() as *const i64) };
    assert!(failures > 0);
}

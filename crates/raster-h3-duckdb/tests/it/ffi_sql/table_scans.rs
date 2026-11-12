// =============================================================================
// 3. Continuous & Categorical (Wide + Long) Table Function Execution via FFI
// =============================================================================

use std::collections::HashMap;

use raster_h3_duckdb::ffi::duckdb_c::*;
use raster_h3_duckdb::functions::categorical_table_function::{
    raster_h3_categorical_bind, raster_h3_categorical_init, raster_h3_categorical_init_local,
    raster_h3_categorical_scan,
};
use raster_h3_duckdb::functions::table_function::{
    raster_h3_bind, raster_h3_init, raster_h3_init_local, raster_h3_scan, RasterH3GlobalData,
};

use super::mock_api::install_mock_api;
use super::{MockBindContext, MockDataChunk, MockFuncContext, MockInitContext, MockVal};
use crate::helpers::TestGeoTiffBuilder;

#[test]
fn test_continuous_table_function_full_scan_and_projection_pushdown() {
    install_mock_api();
    let (_tmp, path) = TestGeoTiffBuilder::new(32, 32)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| (c + r + 10) as f32);

    let mut named = HashMap::new();
    named.insert("resolution".to_string(), MockVal::Int(8));
    named.insert("geom".to_string(), MockVal::Bool(true));
    named.insert("quantiles".to_string(), MockVal::Str("p50,iqr".to_string()));
    named.insert("min_count".to_string(), MockVal::Double(0.5));
    named.insert("workers".to_string(), MockVal::UInt(2));

    let mut bind_ctx = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        raster_h3_bind(&mut bind_ctx as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(
        bind_ctx.error.is_none(),
        "Bind failed: {:?}",
        bind_ctx.error
    );
    // 10 base columns + 1 geom + 2 quantiles = 13 columns
    assert_eq!(bind_ctx.result_columns.len(), 13);
    assert_eq!(bind_ctx.result_columns[10].0, "geom");
    assert_eq!(bind_ctx.result_columns[11].0, "p50");
    assert_eq!(bind_ctx.result_columns[12].0, "iqr");

    let all_cols: Vec<usize> = (0..13).collect();
    let mut init_ctx = MockInitContext::new(bind_ctx.bind_data, all_cols);
    let mut local_ctx = MockInitContext::new(bind_ctx.bind_data, Vec::new());
    // SAFETY: the contexts are live `MockInitContext`s that carry the bind data installed
    // by the bind callback, which is still owned by its bind context.
    unsafe {
        raster_h3_init(&mut init_ctx as *mut MockInitContext as duckdb_init_info);
        raster_h3_init_local(&mut local_ctx as *mut MockInitContext as duckdb_init_info);
    }
    assert!(init_ctx.error.is_none());

    let mut func_ctx = MockFuncContext {
        bind_data: bind_ctx.bind_data,
        init_data: init_ctx.init_data,
        local_init_data: local_ctx.init_data,
        error: None,
    };

    let mut total_rows = 0usize;
    let mut total_pixel_count = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(13, 2048);
        // SAFETY: the function context carries bind/init/local data installed above (still
        // owned by their live contexts), and the chunk has one 2048-row mock vector per
        // projected column.
        unsafe {
            raster_h3_scan(
                &mut func_ctx as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        assert!(func_ctx.error.is_none());
        if chunk.size == 0 {
            break;
        }
        total_rows += chunk.size;
        // SAFETY: the callbacks above wrote values of these columns' declared types for
        // rows `0..size` into 8-aligned buffers with room for at least that many rows.
        unsafe {
            let counts = chunk.vectors[4].data.as_ptr() as *const f64;
            let p50s = chunk.vectors[11].data.as_ptr() as *const f64;
            let iqrs = chunk.vectors[12].data.as_ptr() as *const f64;
            for i in 0..chunk.size {
                total_pixel_count += *counts.add(i);
                assert!((*p50s.add(i)).is_finite());
                assert!((*iqrs.add(i)) >= 0.0);
                assert_eq!(chunk.vectors[9].strings[i].len(), 125);
                assert_eq!(chunk.vectors[10].strings[i].len(), 125);
            }
        }
    }
    assert!(total_rows > 0);
    assert!((total_pixel_count - 1024.0).abs() < 1e-6);

    // Zero-column projection pushdown (SELECT count(*))
    let mut init_zero = MockInitContext::new(bind_ctx.bind_data, Vec::new());
    // SAFETY: the contexts are live `MockInitContext`s that carry the bind data installed
    // by the bind callback, which is still owned by its bind context.
    unsafe {
        raster_h3_init(&mut init_zero as *mut MockInitContext as duckdb_init_info);
    }
    let mut func_zero = MockFuncContext {
        bind_data: bind_ctx.bind_data,
        init_data: init_zero.init_data,
        local_init_data: local_ctx.init_data,
        error: None,
    };
    let mut zero_col_rows = 0usize;
    loop {
        let mut chunk = MockDataChunk::new(0, 2048);
        // SAFETY: the function context carries bind/init/local data installed above (still
        // owned by their live contexts), and the chunk has one 2048-row mock vector per
        // projected column.
        unsafe {
            raster_h3_scan(
                &mut func_zero as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        zero_col_rows += chunk.size;
    }
    assert_eq!(zero_col_rows, total_rows);
}

#[test]
fn test_continuous_quantile_pushdown_and_batched_columns() {
    install_mock_api();
    // Negative, zero and positive values so sketches use every bin family.
    let (_tmp, path) = TestGeoTiffBuilder::new(32, 32)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .create_f32_tempfile(|c, r| ((c * 7 + r * 3) % 50) as f32 - 10.0);

    let mut named = HashMap::new();
    named.insert("resolution".to_string(), MockVal::Int(8));
    named.insert("geom".to_string(), MockVal::Bool(true));
    named.insert(
        "quantiles".to_string(),
        MockVal::Str("p50,p90,iqr".to_string()),
    );
    let mut bind_ctx = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        raster_h3_bind(&mut bind_ctx as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(
        bind_ctx.error.is_none(),
        "Bind failed: {:?}",
        bind_ctx.error
    );
    // 10 base columns + geom (10) + p50 (11), p90 (12), iqr (13).
    assert_eq!(bind_ctx.result_columns.len(), 14);
    let bind_data = bind_ctx.bind_data;

    let tracks_quantiles = |projection: Vec<usize>| -> bool {
        let mut init = MockInitContext::new(bind_data, projection);
        // SAFETY: the context is a live `MockInitContext` carrying the bind data installed
        // by the bind callback, which is still owned by its bind context.
        unsafe {
            raster_h3_init(&mut init as *mut MockInitContext as duckdb_init_info);
        }
        assert!(init.error.is_none(), "Init failed: {:?}", init.error);
        // SAFETY: init installed a `RasterH3GlobalData` as `init_data`; it stays alive
        // until `init` is dropped below, after the guard is released.
        let global = unsafe { &*(init.init_data as *const RasterH3GlobalData) };
        let tracked = global.streamer.lock().unwrap().track_quantiles();
        drop(init);
        tracked
    };
    assert!(!tracks_quantiles(vec![0, 2, 4]), "no quantile columns");
    assert!(
        !tracks_quantiles(vec![0, 9, 10]),
        "wkb and geom are not quantiles"
    );
    assert!(!tracks_quantiles(Vec::new()), "count(*)");
    assert!(tracks_quantiles(vec![0, 13]), "iqr alone needs sketches");
    assert!(tracks_quantiles((0..14).collect()));

    // Scan `projection` (h3_index first, then f64 columns) into h3_index -> values.
    let scan = |projection: Vec<usize>| -> HashMap<u64, Vec<f64>> {
        let ncols = projection.len();
        let mut init = MockInitContext::new(bind_data, projection);
        let mut local = MockInitContext::new(bind_data, Vec::new());
        // SAFETY: the contexts are live `MockInitContext`s that carry the bind data
        // installed by the bind callback, which is still owned by its bind context.
        unsafe {
            raster_h3_init(&mut init as *mut MockInitContext as duckdb_init_info);
            raster_h3_init_local(&mut local as *mut MockInitContext as duckdb_init_info);
        }
        assert!(init.error.is_none(), "Init failed: {:?}", init.error);
        let mut func = MockFuncContext {
            bind_data,
            init_data: init.init_data,
            local_init_data: local.init_data,
            error: None,
        };
        let mut out = HashMap::new();
        loop {
            let mut chunk = MockDataChunk::new(ncols, 2048);
            // SAFETY: the function context carries bind/init/local data installed above
            // (still owned by their live contexts), and the chunk has one 2048-row mock
            // vector per projected column.
            unsafe {
                raster_h3_scan(
                    &mut func as *mut MockFuncContext as duckdb_function_info,
                    &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
                );
            }
            assert!(func.error.is_none(), "Scan failed: {:?}", func.error);
            if chunk.size == 0 {
                break;
            }
            // SAFETY: column 0 holds UBIGINT h3 indices and the rest DOUBLEs, written for
            // rows `0..size` into 8-aligned buffers with room for that many rows.
            unsafe {
                let cells = chunk.vectors[0].data.as_ptr();
                for i in 0..chunk.size {
                    let row = (1..ncols)
                        .map(|c| *(chunk.vectors[c].data.as_ptr() as *const f64).add(i))
                        .collect();
                    assert!(out.insert(*cells.add(i), row).is_none(), "duplicate cell");
                }
            }
        }
        out
    };

    // Reference: every DOUBLE column in schema order. Row positions:
    // mean 0, stddev 1, count 2, min 3, max 4, sum 5, p50 6, p90 7, iqr 8.
    let full = scan(vec![0, 2, 3, 4, 5, 6, 7, 11, 12, 13]);
    // Quantiles reordered and repeated, mixed with a base column: iqr, p50, p90, p50, mean.
    let got = scan(vec![0, 13, 11, 12, 11, 2]);
    assert!(!got.is_empty());
    assert_eq!(got.len(), full.len());

    let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
    for (cell, row) in &got {
        let f = &full[cell];
        let want = [f[8], f[6], f[7], f[6], f[0]];
        for (k, (&g, &w)) in row.iter().zip(&want).enumerate() {
            assert!(close(g, w), "cell {cell} column {k}: {g} vs {w}");
        }
        let (min, max, p50, p90, iqr) = (f[3], f[4], f[6], f[7], f[8]);
        assert!(
            (min - 1e-9..=p90 + 1e-9).contains(&p50),
            "p50 {p50} outside [{min}, {p90}]"
        );
        assert!(p90 <= max + 1e-9, "p90 {p90} above max {max}");
        assert!(iqr >= 0.0);
    }
}

#[test]
fn test_categorical_table_function_wide_and_long_formats() {
    install_mock_api();
    let (_tmp, path) = TestGeoTiffBuilder::new(32, 32)
        .origin(-122.45, 37.80)
        .pixel_size(0.001)
        .create_tempfile::<u8, _>(|c, r| ((c + r) % 3 + 1) as u8);

    // 0. Invalid remap syntax sets bind error cleanly
    let mut bad_remap = HashMap::new();
    bad_remap.insert(
        "remap".to_string(),
        MockVal::Str("invalid_without_colon".to_string()),
    );
    let mut bind_bad = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        bad_remap,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        raster_h3_categorical_bind(&mut bind_bad as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_bad.error.is_some());

    // 1. Wide Format with remap
    let mut named_wide = HashMap::new();
    named_wide.insert("resolution".to_string(), MockVal::Int(8));
    named_wide.insert("format".to_string(), MockVal::Str("wide".to_string()));
    named_wide.insert(
        "remap".to_string(),
        MockVal::Str("1: 10, 2: 20, 3: 30".to_string()),
    );
    named_wide.insert("geom".to_string(), MockVal::Bool(true));

    let mut bind_wide = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named_wide,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        raster_h3_categorical_bind(&mut bind_wide as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_wide.error.is_none());
    assert_eq!(bind_wide.result_columns.len(), 14);

    let wide_cols: Vec<usize> = (0..14).collect();
    let mut init_wide = MockInitContext::new(bind_wide.bind_data, wide_cols);
    let mut local_wide = MockInitContext::new(bind_wide.bind_data, Vec::new());
    // SAFETY: the contexts are live `MockInitContext`s that carry the bind data installed
    // by the bind callback, which is still owned by its bind context.
    unsafe {
        raster_h3_categorical_init(&mut init_wide as *mut MockInitContext as duckdb_init_info);
        raster_h3_categorical_init_local(
            &mut local_wide as *mut MockInitContext as duckdb_init_info,
        );
    }
    let mut func_wide = MockFuncContext {
        bind_data: bind_wide.bind_data,
        init_data: init_wide.init_data,
        local_init_data: local_wide.init_data,
        error: None,
    };

    let mut wide_cells = 0usize;
    let mut wide_total_pixels = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(14, 2048);
        // SAFETY: the function context carries bind/init/local data installed above (still
        // owned by their live contexts), and the chunk has one 2048-row mock vector per
        // projected column.
        unsafe {
            raster_h3_categorical_scan(
                &mut func_wide as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        wide_cells += chunk.size;
        // SAFETY: the callbacks above wrote values of these columns' declared types for
        // rows `0..size` into 8-aligned buffers with room for at least that many rows.
        unsafe {
            let maj_classes = chunk.vectors[2].data.as_ptr() as *const i64;
            let totals = chunk.vectors[6].data.as_ptr() as *const f64;
            for (i, hist) in chunk.vectors[7].strings[..chunk.size].iter().enumerate() {
                let mc = *maj_classes.add(i);
                assert!(
                    mc == 10 || mc == 20 || mc == 30,
                    "Remapped majority class must be in {{10, 20, 30}}, got {mc}"
                );
                wide_total_pixels += *totals.add(i);
                let hist = std::str::from_utf8(hist).unwrap();
                assert!(hist.starts_with('{') && hist.ends_with('}'));
            }
        }
    }
    assert!(wide_cells > 0);
    assert!((wide_total_pixels - 1024.0).abs() < 1e-6);

    // 2. Long Format
    let mut named_long = HashMap::new();
    named_long.insert("resolution".to_string(), MockVal::Int(8));
    named_long.insert("format".to_string(), MockVal::Str("long".to_string()));
    named_long.insert(
        "remap".to_string(),
        MockVal::Str("1: 10, 2: 20, 3: 30".to_string()),
    );
    named_long.insert("geom".to_string(), MockVal::Bool(true));

    let mut bind_long = MockBindContext::new(
        vec![MockVal::Str(path.to_string_lossy().into_owned())],
        named_long,
    );
    // SAFETY: the context is a live `MockBindContext`, which the installed mock C API
    // treats as the bind info.
    unsafe {
        raster_h3_categorical_bind(&mut bind_long as *mut MockBindContext as duckdb_bind_info);
    }
    assert!(bind_long.error.is_none());
    assert_eq!(bind_long.result_columns.len(), 13);

    let long_cols: Vec<usize> = (0..13).collect();
    let mut init_long = MockInitContext::new(bind_long.bind_data, long_cols);
    let mut local_long = MockInitContext::new(bind_long.bind_data, Vec::new());
    // SAFETY: the contexts are live `MockInitContext`s that carry the bind data installed
    // by the bind callback, which is still owned by its bind context.
    unsafe {
        raster_h3_categorical_init(&mut init_long as *mut MockInitContext as duckdb_init_info);
        raster_h3_categorical_init_local(
            &mut local_long as *mut MockInitContext as duckdb_init_info,
        );
    }
    let mut func_long = MockFuncContext {
        bind_data: bind_long.bind_data,
        init_data: init_long.init_data,
        local_init_data: local_long.init_data,
        error: None,
    };

    let mut long_rows = 0usize;
    let mut long_total_pixels = 0.0f64;
    loop {
        let mut chunk = MockDataChunk::new(13, 2048);
        // SAFETY: the function context carries bind/init/local data installed above (still
        // owned by their live contexts), and the chunk has one 2048-row mock vector per
        // projected column.
        unsafe {
            raster_h3_categorical_scan(
                &mut func_long as *mut MockFuncContext as duckdb_function_info,
                &mut chunk as *mut MockDataChunk as duckdb_data_chunk,
            );
        }
        if chunk.size == 0 {
            break;
        }
        long_rows += chunk.size;
        // SAFETY: the callbacks above wrote values of these columns' declared types for
        // rows `0..size` into 8-aligned buffers with room for at least that many rows.
        unsafe {
            let cats = chunk.vectors[2].data.as_ptr() as *const i64;
            let counts = chunk.vectors[3].data.as_ptr() as *const f64;
            let fracs = chunk.vectors[4].data.as_ptr() as *const f64;
            for i in 0..chunk.size {
                let c = *cats.add(i);
                assert!(c == 10 || c == 20 || c == 30);
                assert!(*fracs.add(i) > 0.0 && *fracs.add(i) <= 1.0);
                long_total_pixels += *counts.add(i);
                assert_eq!(chunk.vectors[11].strings[i].len(), 125);
                assert_eq!(chunk.vectors[12].strings[i].len(), 125);
            }
        }
    }
    assert!(
        long_rows > wide_cells,
        "Long format must expand multi-class cells into multiple rows"
    );
    assert!((long_total_pixels - 1024.0).abs() < 1e-6);
}

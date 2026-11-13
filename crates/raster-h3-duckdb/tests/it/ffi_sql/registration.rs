// =============================================================================
// 1. Extension Initialization & Function Registration Tests
// =============================================================================

use std::ffi::CStr;

use raster_h3_duckdb::ffi::duckdb_c::*;
use raster_h3_duckdb::functions::scalar::*;

use super::mock_api::install_mock_api;
use super::MockConnection;

#[test]
fn test_extension_init_and_registration_completeness() {
    install_mock_api();
    // SAFETY: `raster_h3_version` returns a static NUL-terminated literal.
    let ver = unsafe { CStr::from_ptr(raster_h3_duckdb::raster_h3_version()).to_string_lossy() };
    assert_eq!(ver, "v0.0.1");

    let mut dummy_db = 1usize;
    let db_ptr = &mut dummy_db as *mut usize as duckdb_database;
    // SAFETY: the mock C API is installed, and the mock `duckdb_connect` only needs a non-
    // null database handle.
    assert!(unsafe { raster_h3_duckdb::raster_h3_init(db_ptr) });

    // Also test C API entrypoint
    unsafe extern "C" fn get_db(info: duckdb_extension_info) -> *mut duckdb_database {
        info as *mut duckdb_database
    }
    let mut db_handle = db_ptr;
    let access = duckdb_extension_access {
        set_error: None,
        get_database: Some(get_db),
        get_api: None,
    };
    // SAFETY: `access` is a fully initialized table on the stack and `get_db` returns the
    // extension-info pointer, which points to `db_handle`, a valid `duckdb_database`.
    assert!(unsafe {
        raster_h3_duckdb::raster_h3_init_c_api(
            &mut db_handle as *mut duckdb_database as duckdb_extension_info,
            &access,
        )
    });

    // Inspect registered functions on a connection
    let mut con = MockConnection::default();
    let con_ptr = &mut con as *mut MockConnection as duckdb_connection;
    // SAFETY: `con_ptr` points to a live `MockConnection`, which is what the installed mock
    // C API expects as a connection.
    unsafe {
        raster_h3_duckdb::functions::register_table_function(con_ptr).unwrap();
        raster_h3_duckdb::functions::register_categorical_table_function(con_ptr).unwrap();
        register_scalar_functions(con_ptr).unwrap();
        raster_h3_duckdb::functions::register_pmtiles_table_function(con_ptr).unwrap();
        raster_h3_duckdb::functions::register_parquet_table_function(con_ptr).unwrap();
        raster_h3_duckdb::functions::register_plan_partitions_function(con_ptr).unwrap();
        raster_h3_duckdb::functions::register_merge_functions(con_ptr).unwrap();
    }

    // Partitioned scans: the planner, the `partition` parameter and the merge aggregates.
    assert!(con
        .table_functions
        .iter()
        .any(|f| f.name == "h3_raster_plan_partitions"
            && f.named_params.get("target_chunks") == Some(&DuckDBType::BigInt)));
    for name in [
        "h3_raster_continuous_aggregate",
        "h3_raster_categorical_aggregate",
    ] {
        let tf = con.table_functions.iter().find(|f| f.name == name).unwrap();
        assert_eq!(
            tf.named_params.get("partition"),
            Some(&DuckDBType::Varchar),
            "{name}"
        );
    }
    let merges: Vec<&str> = con
        .aggregate_functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(
        merges,
        ["h3_merge_partials", "h3_merge_categorical_partials"]
    );
    for f in &con.aggregate_functions {
        assert_eq!(f.params, [DuckDBType::Blob]);
        assert_eq!(f.ret, DuckDBType::Struct);
        assert!(f.has_functions && f.has_destructor, "{}", f.name);
    }

    // The mock reports DuckDB v1.2.0, so {h3,a5}_polygon_to_cells register only their BLOB overloads.
    // 17 H3 scalar overloads + 20 A5 scalar overloads = 37 total.
    assert_eq!(con.scalar_functions.len(), 37);
    for poly_name in ["h3_polygon_to_cells", "a5_polygon_to_cells"] {
        let polygon_fns: Vec<_> = con
            .scalar_functions
            .iter()
            .filter(|f| f.name == poly_name)
            .collect();
        assert_eq!(polygon_fns.len(), 2, "{poly_name}");
        for sf in &polygon_fns {
            assert_eq!(sf.params[..2], [DuckDBType::Blob, DuckDBType::BigInt]);
            assert_eq!(sf.ret, DuckDBType::List);
        }
        assert_eq!(polygon_fns[1].params.get(2), Some(&DuckDBType::Varchar));
    }
    let a5_lonlat: Vec<_> = con
        .scalar_functions
        .iter()
        .filter(|f| f.name == "a5_lonlat_to_cell")
        .collect();
    assert_eq!(a5_lonlat.len(), 1);
    assert_eq!(
        a5_lonlat[0].params,
        [DuckDBType::Double, DuckDBType::Double, DuckDBType::BigInt]
    );
    assert_eq!(a5_lonlat[0].ret, DuckDBType::UBigInt);
    for geom_name in ["h3_cell_to_geometry", "a5_cell_to_geometry"] {
        let geometry_aliases: Vec<_> = con
            .scalar_functions
            .iter()
            .filter(|f| f.name == geom_name)
            .collect();
        assert_eq!(geometry_aliases.len(), 2, "{geom_name}");
        assert!(geometry_aliases
            .iter()
            .any(|f| f.params == [DuckDBType::UBigInt]));
        assert!(geometry_aliases
            .iter()
            .any(|f| f.params == [DuckDBType::Varchar]));
        assert!(geometry_aliases
            .iter()
            .all(|f| f.ret != DuckDBType::Invalid));
    }
    for sf in &con.scalar_functions {
        assert!(!sf.name.is_empty());
        assert!(!sf.params.is_empty());
        assert_ne!(sf.ret, DuckDBType::Invalid);
    }
    let tf_names: Vec<&str> = con
        .table_functions
        .iter()
        .map(|f| f.name.as_str())
        .collect();
    for expected in [
        "h3_raster_continuous_aggregate",
        "h3_raster_continuous",
        "raster_h3",
        "a5_raster_continuous_aggregate",
        "a5_raster_continuous",
        "raster_a5",
        "h3_raster_categorical_aggregate",
        "h3_raster_categorical",
        "a5_raster_categorical_aggregate",
        "a5_raster_categorical",
        "h3_raster_to_pmtiles",
        "a5_raster_to_pmtiles",
        "h3_parquet_to_pmtiles",
        "h3_raster_to_parquet",
        "a5_raster_to_parquet",
    ] {
        let tf = con
            .table_functions
            .iter()
            .find(|f| f.name == expected)
            .unwrap_or_else(|| panic!("Missing table function {expected}"));
        assert!(!tf.positional_params.is_empty());
        if expected.contains("aggregate") || expected == "raster_h3" || expected == "raster_a5" {
            assert!(tf.projection_pushdown);
        }
        assert!(tf_names.contains(&expected));
    }

    // Verify export functions register source_crs/crs, grid, and overlap_rule
    for export_fn in [
        "h3_raster_to_parquet",
        "a5_raster_to_parquet",
        "h3_raster_to_pmtiles",
        "a5_raster_to_pmtiles",
    ] {
        let tf = con
            .table_functions
            .iter()
            .find(|f| f.name == export_fn)
            .unwrap();
        for param in [
            "grid",
            "source_crs",
            "crs",
            "overlap_rule",
            "workers",
            "decode_workers",
            "fetch_workers",
            "decode_budget_mb",
        ] {
            assert!(
                tf.named_params.contains_key(param),
                "{export_fn} missing registered named parameter '{param}'"
            );
        }
    }
}

//! `fmgr` (V1 calling convention) entry points run in process against the server
//! stand-ins in [`server`]: text/bytea scalars and the categorical/continuous
//! value-per-call SRFs, compared with `h3o` and core readers.

mod server;

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::path::Path;

use h3o::{CellIndex, LatLng, Resolution};
use raster_h3_core::aggregator::multi_horizon::MultiResolutionConfig;
use raster_h3_core::scalar_ops::h3_to_wkb_u64;
use raster_h3_pg::fmgr::{
    pg_finfo_pg_h3_is_valid_text, pg_finfo_pg_h3_raster_categorical_aggregate,
    pg_finfo_pg_h3_raster_continuous_aggregate, pg_finfo_pg_h3_to_string, pg_finfo_pg_h3_to_wkb,
    pg_finfo_pg_string_to_h3, pg_h3_is_valid_text, pg_h3_raster_categorical_aggregate,
    pg_h3_raster_continuous_aggregate, pg_h3_to_string, pg_h3_to_wkb, pg_string_to_h3,
    Pg_magic_func,
};
use raster_h3_pg::{
    int64_to_datum, Datum, FunctionCallInfoBaseData, NullableDatum, PgContinuousSrf,
};

use self::server::{FmgrInfo, ReturnSetInfo, EXPR_END_RESULT, EXPR_MULTIPLE_RESULT, SERVER};
use crate::srf::{classes_tif, core_categorical, f, gradient_tif, pg_categorical};

const NULL: NullableDatum = NullableDatum {
    value: 0,
    isnull: true,
};

fn val(value: Datum) -> NullableDatum {
    NullableDatum {
        value,
        isnull: false,
    }
}

/// A `text`/`bytea` argument with a 4-byte varlena header. The buffer is leaked so the
/// Datum stays valid for the whole test.
fn varlena(bytes: &[u8]) -> NullableDatum {
    let mut buf = (((bytes.len() + 4) as u32) << 2).to_ne_bytes().to_vec();
    buf.extend_from_slice(bytes);
    val(Box::leak(buf.into_boxed_slice()).as_ptr() as Datum)
}

/// A short `text` argument with a 1-byte varlena header (as stored inline in tuples).
fn short_varlena(bytes: &[u8]) -> NullableDatum {
    assert!(bytes.len() < 127);
    let mut buf = vec![(((bytes.len() + 1) as u8) << 1) | 1];
    buf.extend_from_slice(bytes);
    val(Box::leak(buf.into_boxed_slice()).as_ptr() as Datum)
}

/// Payload of a varlena Datum returned by the extension (always a 4-byte header).
///
/// # Safety
/// `d` must be a varlena Datum built by the extension's `varlena_datum`.
unsafe fn read_varlena(d: Datum) -> Vec<u8> {
    let p = d as *const u8;
    // SAFETY: `varlena_datum` writes a 4-byte length header followed by `total - 4`
    // payload bytes into a (never freed) mock `palloc` buffer.
    unsafe {
        let total = ((p as *const u32).read_unaligned() >> 2) as usize;
        std::slice::from_raw_parts(p.add(4), total - 4).to_vec()
    }
}

/// Calls a scalar entry point, returning `None` when it set `isnull`.
fn call(
    f: unsafe extern "C" fn(raster_h3_pg::pg_func_call_info) -> Datum,
    args: &[NullableDatum],
) -> Option<Datum> {
    let mut fc = FunctionCallInfoBaseData::with_args(args);
    // SAFETY: `fc` is a fully initialized local call info with all 8 argument slots, and
    // `args` matches the SQL signature the test calls `f` with.
    let d = unsafe { f(&mut fc) };
    (!fc.isnull).then_some(d)
}

fn sample_cells() -> Vec<CellIndex> {
    let mut out: Vec<CellIndex> = [(37.77, -122.42), (-41.3, 174.8), (0.0, 179.9999)]
        .iter()
        .flat_map(|&(lat, lng)| {
            let ll = LatLng::new(lat, lng).unwrap();
            [Resolution::Zero, Resolution::Eight, Resolution::Fifteen].map(|r| ll.to_cell(r))
        })
        .collect();
    let pentagon = CellIndex::base_cells().find(|c| c.is_pentagon()).unwrap();
    out.push(pentagon.center_child(Resolution::One).unwrap());
    out.push(pentagon.center_child(Resolution::Ten).unwrap());
    out
}

#[test]
fn finfo_records_and_magic_block() {
    for finfo in [
        pg_finfo_pg_h3_to_string,
        pg_finfo_pg_string_to_h3,
        pg_finfo_pg_h3_is_valid_text,
        pg_finfo_pg_h3_to_wkb,
        pg_finfo_pg_h3_raster_continuous_aggregate,
        pg_finfo_pg_h3_raster_categorical_aggregate,
    ] {
        // SAFETY: each `pg_finfo_*` returns a pointer to the static `PG_FINFO_V1`.
        assert_eq!(unsafe { (*finfo()).api_version }, 1);
    }
    assert!(!Pg_magic_func().is_null());
    // The magic block starts with its own size and the major version * 100.
    let words = Pg_magic_func() as *const c_void as *const i32;
    let major: i32 = env!("RASTER_H3_PG_MAJOR").parse().unwrap();
    // SAFETY: the magic block is a static `repr(C)` struct of at least 24 bytes that starts
    // with two `c_int`s (`len`, `version`), so both reads are in bounds and aligned.
    unsafe {
        assert!(*words >= 24);
        assert_eq!(*words.add(1), major * 100);
    }
}

#[test]
fn h3_to_string_and_string_to_h3_round_trip() {
    for cell in sample_cells() {
        let as_int = int64_to_datum(u64::from(cell) as i64);
        let text = call(pg_h3_to_string, &[val(as_int)]).expect("valid cell");
        // SAFETY: `pg_h3_to_string` returns a `varlena_datum`.
        let text = unsafe { read_varlena(text) };
        assert_eq!(String::from_utf8(text.clone()).unwrap(), cell.to_string());

        // Both varlena header forms parse back to the same int8.
        for arg in [varlena(&text), short_varlena(&text)] {
            assert_eq!(call(pg_string_to_h3, &[arg]), Some(as_int), "{cell}");
        }
        let upper = cell.to_string().to_uppercase();
        assert_eq!(
            call(pg_string_to_h3, &[varlena(upper.as_bytes())]),
            Some(as_int)
        );
    }

    // NULL in, NULL out; 0 is not a cell.
    assert_eq!(call(pg_h3_to_string, &[NULL]), None);
    assert_eq!(call(pg_h3_to_string, &[val(0)]), None);
    assert_eq!(call(pg_h3_to_string, &[]), None);
    assert_eq!(call(pg_string_to_h3, &[NULL]), None);
    for bad in [&b"not hex"[..], b"", b"0x", "\u{e9}".as_bytes()] {
        assert_eq!(call(pg_string_to_h3, &[varlena(bad)]), None, "{bad:?}");
    }
}

#[test]
fn h3_is_valid_text_agrees_with_h3o() {
    for cell in sample_cells() {
        let s = cell.to_string();
        assert_eq!(call(pg_h3_is_valid_text, &[varlena(s.as_bytes())]), Some(1));
        assert_eq!(
            call(pg_h3_is_valid_text, &[short_varlena(s.as_bytes())]),
            Some(1)
        );
    }
    for bad in ["", "zzz", "0", "ffffffffffffffff", "8828308281fffff0"] {
        assert_eq!(bad.parse::<CellIndex>().ok(), None);
        assert_eq!(
            call(pg_h3_is_valid_text, &[varlena(bad.as_bytes())]),
            Some(0),
            "'{bad}'"
        );
    }
    // Invalid UTF-8 is not a valid cell either.
    assert_eq!(
        call(pg_h3_is_valid_text, &[varlena(&[0xff, 0xfe])]),
        Some(0)
    );
    assert_eq!(call(pg_h3_is_valid_text, &[NULL]), None);
}

#[test]
fn h3_to_wkb_returns_core_wkb_including_pentagons() {
    for cell in sample_cells() {
        let d = call(pg_h3_to_wkb, &[val(int64_to_datum(u64::from(cell) as i64))]).unwrap();
        // SAFETY: `pg_h3_to_wkb` returns a `varlena_datum`.
        let wkb = unsafe { read_varlena(d) };
        assert_eq!(Some(wkb.clone()), h3_to_wkb_u64(u64::from(cell)), "{cell}");
        let lngs: Vec<f64> = cell.boundary().iter().map(|v| v.lng()).collect();
        let span = lngs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
            - lngs.iter().cloned().fold(f64::INFINITY, f64::min);
        let kind = u32::from_le_bytes(wkb[1..5].try_into().unwrap());
        if span > 180.0 {
            // Antimeridian cells are MultiPolygons (6); pole cells are polar-cap Polygons (3).
            assert!(kind == 6 || kind == 3, "{cell}: type {kind}");
        } else {
            // Polygon with one closed ring of boundary vertices.
            assert_eq!(kind, 3, "{cell}");
            assert_eq!(wkb.len(), 13 + 16 * (lngs.len() + 1), "{cell}");
        }
    }
    assert_eq!(call(pg_h3_to_wkb, &[NULL]), None);
    assert_eq!(call(pg_h3_to_wkb, &[val(0)]), None);
    assert_eq!(call(pg_h3_to_wkb, &[val(usize::MAX)]), None);
}

/// Calls `srf` until it reports `ExprEndResult`, returning the rows it built.
fn run_srf(
    srf: unsafe extern "C" fn(raster_h3_pg::pg_func_call_info) -> Datum,
    natts: usize,
    args: &[NullableDatum],
    stop_after: Option<usize>,
) -> Vec<Vec<Option<String>>> {
    server::reset(natts);
    let mut flinfo = FmgrInfo::default();
    let mut rsi = ReturnSetInfo::default();
    let mut fc = FunctionCallInfoBaseData::with_args(args);
    fc.flinfo = &mut flinfo as *mut FmgrInfo as *mut c_void;
    fc.resultinfo = &mut rsi as *mut ReturnSetInfo as *mut c_void;

    let mut calls = 0;
    loop {
        fc.isnull = false;
        // SAFETY: `fc` is a fully initialized call info whose `flinfo`/`resultinfo` point to
        // the live locals above, as the value-per-call protocol requires.
        let d = unsafe { srf(&mut fc) };
        calls += 1;
        if rsi.is_done == EXPR_END_RESULT {
            assert!(fc.isnull, "the final call returns NULL");
            assert!(
                flinfo.fn_extra.is_null(),
                "end_MultiFuncCall clears fn_extra"
            );
            break;
        }
        assert_eq!(rsi.is_done, EXPR_MULTIPLE_RESULT);
        assert!(!fc.isnull);
        // The Datum is the tuple recorded by BuildTupleFromCStrings.
        assert_eq!(d, SERVER.with_borrow(|s| s.rows.len()));
        if stop_after == Some(calls) {
            // Early termination (e.g. LIMIT): the executor deletes the multi-call
            // context without a final call, which must release the SRF state.
            // SAFETY: `fn_extra` is the mock `FuncCallContext` box, whose fifth pointer-sized
            // field is `multi_call_memory_ctx`.
            let ctx = unsafe { *((flinfo.fn_extra as *const *mut c_void).add(4)) };
            server::reset_context(ctx);
            break;
        }
    }
    SERVER.with_borrow(|s| {
        assert_eq!(s.callbacks_run, 1, "the SRF state is dropped exactly once");
        s.rows.clone()
    })
}

fn srf_args(
    path: &Path,
    res: Option<i32>,
    crs: Option<&str>,
    nodata: Option<f64>,
    bbox: Option<&str>,
) -> Vec<NullableDatum> {
    vec![
        varlena(path.to_str().unwrap().as_bytes()),
        res.map_or(NULL, |r| val(r as Datum)),
        crs.map_or(NULL, |c| varlena(c.as_bytes())),
        nodata.map_or(NULL, |n| val(n.to_bits() as Datum)),
        bbox.map_or(NULL, |b| short_varlena(b.as_bytes())),
    ]
}

#[test]
fn categorical_aggregate_srf_matches_core() {
    let (_tmp, path) = classes_tif();
    let cases = [
        (None, None, None, None),
        (Some(7), Some("EPSG:4326"), None, None),
        (
            Some(9),
            None,
            Some(3.0),
            Some("-122.45,37.775,-122.425,37.795"),
        ),
    ];
    for (res, crs, nodata, bbox) in cases {
        let rows = run_srf(
            pg_h3_raster_categorical_aggregate,
            11,
            &srf_args(&path, res, crs, nodata, bbox),
            None,
        );
        let mut cfg = MultiResolutionConfig::new(vec![res.unwrap_or(8) as u8]);
        cfg.custom_crs = crs.map(String::from);
        cfg.custom_nodata = nodata;
        cfg.bbox = bbox.map(|b| raster_h3_core::scalar_ops::parse_bbox_str(b).unwrap());
        let want = core_categorical(&path, &cfg);

        // Text columns as `BuildTupleFromCStrings` receives them; floats re-rounded.
        let got: BTreeMap<i64, Vec<String>> = rows
            .iter()
            .map(|r| {
                let t = |i: usize| r[i].clone().expect("categorical columns are never NULL");
                let fl = |i: usize| f(t(i).parse().unwrap());
                let wkb = t(10);
                let wkb_bytes: Vec<u8> = (2..wkb.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&wkb[i..i + 2], 16).unwrap())
                    .collect();
                assert!(wkb.starts_with("\\x"));
                (
                    t(0).parse().unwrap(),
                    vec![
                        t(1),
                        t(2),
                        fl(3),
                        fl(4),
                        t(5),
                        fl(6),
                        t(7),
                        t(8),
                        fl(9),
                        format!("{wkb_bytes:?}"),
                    ],
                )
            })
            .collect();
        assert_eq!(rows.len(), got.len(), "no duplicate cells");
        assert!(!got.is_empty());
        assert_eq!(
            got, want,
            "res {res:?} crs {crs:?} nodata {nodata:?} bbox {bbox:?}"
        );
    }

    // Same rows as the Rust categorical SRF iterator.
    let rust: BTreeMap<i64, Vec<String>> =
        raster_h3_pg::PgCategoricalSrf::open(path.to_str().unwrap(), vec![8], None, None, None)
            .unwrap()
            .map(|r| pg_categorical(&r.unwrap()))
            .collect();
    assert_eq!(
        rust,
        core_categorical(&path, &MultiResolutionConfig::new(vec![8]))
    );
}

#[test]
fn categorical_aggregate_srf_releases_state_on_early_termination() {
    let (_tmp, path) = classes_tif();
    let rows = run_srf(
        pg_h3_raster_categorical_aggregate,
        11,
        &srf_args(&path, Some(9), None, None, None),
        Some(3),
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn continuous_aggregate_srf_matches_rust_srf() {
    let (_tmp, path) = gradient_tif();
    let rows = run_srf(
        pg_h3_raster_continuous_aggregate,
        10,
        &srf_args(&path, Some(8), None, Some(5.0), None),
        None,
    );
    let rust: Vec<_> =
        PgContinuousSrf::open(path.to_str().unwrap(), vec![8], None, Some(5.0), None)
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
    let mut got: Vec<Vec<Option<String>>> = rows;
    got.sort();
    let mut want: Vec<Vec<Option<String>>> = rust
        .iter()
        .map(|r| {
            vec![
                Some(r.h3_index.to_string()),
                Some(r.h3_hex.clone()),
                Some(r.mean.to_string()),
                r.stddev.map(|s| s.to_string()),
                Some(r.count.to_string()),
                Some(r.min.to_string()),
                Some(r.max.to_string()),
                Some(r.sum.to_string()),
                Some(r.resolution.to_string()),
                Some(format!(
                    "\\x{}",
                    r.wkb.iter().map(|b| format!("{b:02x}")).collect::<String>()
                )),
            ]
        })
        .collect();
    want.sort();
    // Compare everything but the float columns exactly; floats to 12 digits.
    let round = |rows: &mut Vec<Vec<Option<String>>>| {
        for r in rows.iter_mut() {
            for i in [2, 3, 4, 5, 6, 7] {
                r[i] = r[i].as_ref().map(|v| f(v.parse().unwrap()));
            }
        }
    };
    round(&mut got);
    round(&mut want);
    assert_eq!(got, want);
}

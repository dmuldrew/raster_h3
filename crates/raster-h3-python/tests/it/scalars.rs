//! String variants of the scalar C-ABI helpers: `raster_h3_py_h3_is_valid_str` and
//! `raster_h3_py_h3_cell_to_parent_str`, checked against `h3o`.

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use h3o::{CellIndex, LatLng, Resolution};
use raster_h3_python::{
    raster_h3_py_free_string, raster_h3_py_h3_cell_to_parent_str,
    raster_h3_py_h3_cell_to_parent_u64, raster_h3_py_h3_is_valid_str, raster_h3_py_h3_is_valid_u64,
};

fn is_valid(s: &str) -> i32 {
    let c = CString::new(s).unwrap();
    unsafe { raster_h3_py_h3_is_valid_str(c.as_ptr()) }
}

fn parent(s: &str, res: i64) -> Option<String> {
    let c = CString::new(s).unwrap();
    unsafe { owned(raster_h3_py_h3_cell_to_parent_str(c.as_ptr(), res)) }
}

unsafe fn owned(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let s = CStr::from_ptr(p).to_str().unwrap().to_string();
    raster_h3_py_free_string(p);
    Some(s)
}

fn cells() -> Vec<CellIndex> {
    let mut out: Vec<CellIndex> = [(37.77, -122.42), (-33.86, 151.21), (64.1, -21.9)]
        .iter()
        .flat_map(|&(lat, lng)| {
            let ll = LatLng::new(lat, lng).unwrap();
            [Resolution::Zero, Resolution::Five, Resolution::Fifteen].map(|r| ll.to_cell(r))
        })
        .collect();
    out.push(
        CellIndex::base_cells()
            .find(|c| c.is_pentagon())
            .unwrap()
            .center_child(Resolution::Nine)
            .unwrap(),
    );
    out
}

#[test]
fn is_valid_str_agrees_with_h3o_and_the_u64_variant() {
    for cell in cells() {
        let hex = cell.to_string();
        assert_eq!(is_valid(&hex), 1, "{hex}");
        assert_eq!(is_valid(&hex.to_uppercase()), 1, "{hex}");
        assert_eq!(raster_h3_py_h3_is_valid_u64(u64::from(cell)), 1);
    }
    // A res-15 cell whose last digit is 7, which is not a valid direction.
    let invalid_u64 = u64::from(cells()[2]) | 0x7;
    for s in [
        "",
        "zzz",
        "0",
        "ffffffffffffffff",
        "8828308281fffff0",
        &format!("{invalid_u64:x}"),
    ] {
        let expect = i32::from(s.parse::<CellIndex>().is_ok());
        assert_eq!(is_valid(s), expect, "'{s}'");
        assert_eq!(expect, 0, "'{s}' should be invalid");
    }
    assert_eq!(unsafe { raster_h3_py_h3_is_valid_str(ptr::null()) }, 0);
}

#[test]
fn cell_to_parent_str_agrees_with_h3o_and_the_u64_variant() {
    for cell in cells() {
        let hex = cell.to_string();
        let res = u8::from(cell.resolution());
        for target in 0..=res {
            let want = cell.parent(Resolution::try_from(target).unwrap()).unwrap();
            assert_eq!(
                parent(&hex, i64::from(target)),
                Some(want.to_string()),
                "{hex} -> res {target}"
            );
            assert_eq!(
                raster_h3_py_h3_cell_to_parent_u64(u64::from(cell), i64::from(target)),
                u64::from(want)
            );
        }
        // Finer or out-of-range resolutions have no parent.
        for bad in [i64::from(res) + 1, 16, -1, i64::MAX] {
            if bad <= i64::from(res) {
                continue;
            }
            assert_eq!(parent(&hex, bad), None, "{hex} -> res {bad}");
            assert_eq!(raster_h3_py_h3_cell_to_parent_u64(u64::from(cell), bad), 0);
        }
        assert_eq!(parent(&hex, -1), None);
    }
    assert_eq!(parent("not-hex", 3), None);
    assert_eq!(parent("", 3), None);
    assert!(unsafe { raster_h3_py_h3_cell_to_parent_str(ptr::null(), 3) }.is_null());
}

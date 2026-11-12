use super::*;
use crate::ffi::args::build_py_scan_config;
use std::ffi::{c_char, CStr, CString};
use std::fs::File;
use std::ptr;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;

fn write_test_tif(width: u32, height: u32, val: f32) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let file = File::create(tmp.path()).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
    img.encoder()
        .write_tag(
            tiff::tags::Tag::Unknown(33550),
            &[0.001f64, 0.001f64, 0.0f64][..],
        )
        .unwrap();
    img.encoder()
        .write_tag(
            tiff::tags::Tag::Unknown(33922),
            &[0.0f64, 0.0f64, 0.0f64, -122.45f64, 37.80f64, 0.0f64][..],
        )
        .unwrap();
    let data = vec![val; (width * height) as usize];
    img.write_data(&data).unwrap();
    tmp
}

#[test]
fn test_py_scan_config_rejects_malformed_bbox() {
    let path_c = CString::new("/tmp/does_not_matter.tif").unwrap();
    let bad_bbox = CString::new("1,garbage,2,3,4").unwrap();
    let good_bbox = CString::new("-122.5,37.5,-122.0,38.0").unwrap();
    // SAFETY: `path_c` and the bbox strings are live CStrings (or null) for every call.
    unsafe {
        let build = |bbox: *const c_char| {
            build_py_scan_config(
                path_c.as_ptr(),
                ptr::null(),
                0,
                ptr::null(),
                ptr::null(),
                0,
                0.0,
                bbox,
                ptr::null(),
                -1,
            )
        };
        assert!(build(bad_bbox.as_ptr()).is_err());
        assert_eq!(
            build(good_bbox.as_ptr()).unwrap().bbox,
            Some([-122.5, 37.5, -122.0, 38.0])
        );
        assert_eq!(build(ptr::null()).unwrap().bbox, None);
    }
}

#[test]
#[cfg_attr(
    miri,
    ignore = "reads a memory-mapped raster, which Miri cannot follow"
)]
fn test_py_scanner_arrow_c_stream_and_c_abi() {
    let tif = write_test_tif(16, 16, 25.5);
    let path_c = CString::new(tif.path().to_string_lossy().as_bytes()).unwrap();
    let res_c = CString::new("8").unwrap();
    let crs_c = CString::new("EPSG:4326").unwrap();
    let cols_c = CString::new("h3_index,h3_hex,mean").unwrap();

    // SAFETY: string arguments are live `CString`s owned by this frame (or null), and
    // the returned stream is a fresh box that is drained and then freed exactly once.
    unsafe {
        let mut err: *mut c_char = ptr::null_mut();
        let stream_ptr = raster_h3_py_create_arrow_stream(
            path_c.as_ptr(),
            res_c.as_ptr(),
            0,
            ptr::null(),
            crs_c.as_ptr(),
            0,
            0.0,
            ptr::null(),
            cols_c.as_ptr(),
            5,
            &mut err,
        );
        assert!(err.is_null());
        assert!(!stream_ptr.is_null());

        let (schema, batches) = (*stream_ptr).collect_batches().unwrap();
        assert_eq!(schema.fields.len(), 3);
        let rows: usize = batches.iter().map(|b| b.num_rows).sum();
        assert!(rows > 0 && rows <= 5);
        raster_h3_py_free_arrow_stream(stream_ptr);

        let json_ptr = raster_h3_py_scan_json(
            path_c.as_ptr(),
            res_c.as_ptr(),
            0,
            ptr::null(),
            crs_c.as_ptr(),
            0,
            0.0,
            ptr::null(),
            cols_c.as_ptr(),
            3,
            &mut err,
        );
        assert!(!json_ptr.is_null());
        let json_s = CStr::from_ptr(json_ptr).to_string_lossy().into_owned();
        assert!(json_s.contains("\"h3_index\""));
        assert!(json_s.contains("25.5"));
        raster_h3_py_free_string(json_ptr);
    }
}

#[test]
fn test_py_h3_to_wkb_fits_antimeridian_multipolygon() {
    // Res-5 cell crossing 180°: a two-part MultiPolygon larger than 192 bytes.
    let cell = 0x0859_b406_3fff_ffffu64;
    let mut buf = vec![0u8; raster_h3_py_wkb_buf_len()];
    // SAFETY: `buf` provides `buf.len()` writable bytes.
    let written = unsafe { raster_h3_py_h3_to_wkb(cell, buf.as_mut_ptr(), buf.len()) };
    assert!(written > 192, "wrote {written} bytes");
    assert_eq!(u32::from_le_bytes(buf[1..5].try_into().unwrap()), 6);
}

#[test]
fn test_py_h3_to_wkb_fits_odd_res_pentagon() {
    // Res-1 pentagon: 10 boundary vertices -> 13 + 16 * 11 = 189 bytes of WKB.
    let cell = 0x0810_83ff_ffff_ffffu64;
    let n = h3o::CellIndex::try_from(cell).unwrap().boundary().len();
    assert!(n > 6, "expected >6 boundary vertices, got {n}");
    let mut buf = [0u8; raster_h3_core::encoding::WKB_BUF_LEN];
    // SAFETY: `buf` provides `buf.len()` writable bytes.
    let written = unsafe { raster_h3_py_h3_to_wkb(cell, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(written, 13 + 16 * (n + 1));
    assert!(written > 128);
    let mut small = [0u8; 128];
    assert_eq!(
        // SAFETY: `small` provides `small.len()` writable bytes.
        unsafe { raster_h3_py_h3_to_wkb(cell, small.as_mut_ptr(), small.len()) },
        0
    );
}

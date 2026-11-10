#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::raster::http_range::{parse_content_range, validate_range_response};

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    if let Some((start, end, total)) = parse_content_range(s) {
        assert!(
            start <= end && end < total,
            "parse_content_range accepted an out-of-order range"
        );
    }
    let _ = validate_range_response(206, Some(s), 0, 9, None, 10);
});

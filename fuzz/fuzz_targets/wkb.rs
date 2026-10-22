#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::encoding::wkb_reader::wkb_to_multipolygon;

fuzz_target!(|data: &[u8]| {
    let _ = wkb_to_multipolygon(data);
});

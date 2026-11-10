#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::aggregator::remap::CategoryRemapper;

fuzz_target!(|data: &[u8]| {
    let Ok(spec) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(remapper) = CategoryRemapper::parse(spec) {
        for v in [i64::MIN, -1, 0, 1, 255, i64::MAX] {
            let _ = remapper.remap(v);
        }
    }
});

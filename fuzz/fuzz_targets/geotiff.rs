#![no_main]

use std::io::Write;

use libfuzzer_sys::fuzz_target;
use raster_h3_core::raster::geotiff::GeoTiffStreamReader;

fuzz_target!(|data: &[u8]| {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(data).unwrap();
    // Exercise the core metadata path without decoding unbounded image dimensions.
    let _ = GeoTiffStreamReader::open(file.path());
});

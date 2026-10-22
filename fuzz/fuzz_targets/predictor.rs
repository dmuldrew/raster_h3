#![no_main]

use libfuzzer_sys::fuzz_target;
use raster_h3_core::raster::geotiff::TiffByteOrder;
use raster_h3_core::raster::predictor::{
    unpack_f32, unpack_f64, unpack_integer_samples, ChunkEncoding,
};
use tiff::tags::{PhotometricInterpretation, Predictor};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let encoding = ChunkEncoding {
        tile_w: usize::from(data[0] % 32) + 1,
        data_w: usize::from(data[1] % 32) + 1,
        data_h: usize::from(data[2] % 32) + 1,
        spp: usize::from(data[3] % 4) + 1,
        byte_order: if data[4] & 1 == 0 {
            TiffByteOrder::LittleEndian
        } else {
            TiffByteOrder::BigEndian
        },
        predictor: match (data[4] >> 1) % 3 {
            0 => Predictor::None,
            1 => Predictor::Horizontal,
            _ => Predictor::FloatingPoint,
        },
        photometric: if data[4] & 8 == 0 {
            PhotometricInterpretation::BlackIsZero
        } else {
            PhotometricInterpretation::WhiteIsZero
        },
    };
    let size = encoding.data_w * encoding.data_h * encoding.spp;
    let mut bytes = data[5..].to_vec();
    let _ = unpack_integer_samples(&bytes, &mut vec![0u16; size], encoding);
    let _ = unpack_f32(&mut bytes, &mut vec![0f32; size], encoding);
    let _ = unpack_f64(&mut bytes, &mut vec![0f64; size], encoding);
});

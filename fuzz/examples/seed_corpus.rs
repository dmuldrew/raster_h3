#[path = "../../crates/raster-h3-core/tests/it/helpers.rs"]
mod helpers;

use std::fs;
use std::path::Path;

fn main() -> std::io::Result<()> {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    for target in ["wkb", "geotiff", "predictor"] {
        fs::create_dir_all(corpus.join(target))?;
    }
    // Little-endian Polygon with one closed ring; include big-endian and EWKB SRID variants.
    for (name, big_endian, ewkb) in [
        ("polygon", false, false),
        ("polygon-be", true, false),
        ("ewkb", false, true),
    ] {
        let mut bytes = vec![u8::from(!big_endian)];
        let word = |v: u32| {
            if big_endian {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        bytes.extend(word(if ewkb { 0x2000_0003 } else { 3 }));
        if ewkb {
            bytes.extend(word(4326));
        }
        bytes.extend(word(1));
        bytes.extend(word(5));
        for (x, y) in [(0f64, 0f64), (1., 0.), (1., 1.), (0., 1.), (0., 0.)] {
            for value in [x, y] {
                bytes.extend(if big_endian {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                });
            }
        }
        fs::write(corpus.join("wkb").join(name), bytes)?;
    }
    helpers::TestGeoTiffBuilder::new(8, 8)
        .write_f32_fn(corpus.join("geotiff/float"), |c, r| (c + r) as f32)?;
    helpers::TestGeoTiffBuilder::new(8, 8)
        .write_fn(corpus.join("geotiff/integer"), |c, r| (c + r) as u16)?;
    fs::write(corpus.join("predictor/basic"), [0u8; 69])?;
    Ok(())
}

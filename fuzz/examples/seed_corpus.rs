#[path = "../../crates/raster-h3-core/tests/it/helpers.rs"]
mod helpers;

use std::fs;
use std::path::Path;

fn main() -> std::io::Result<()> {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus");
    for target in [
        "wkb",
        "geotiff",
        "predictor",
        "ipc",
        "arrow_import",
        "remap",
        "content_range",
        "partial_state",
    ] {
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

    // A minimal valid IPC stream: one Utf8 column, two rows.
    {
        use raster_h3_core::columnar::ipc::write_ipc_stream;
        use raster_h3_core::columnar::{
            ArrowDataType, ColumnArray, ColumnField, ColumnSchema, RecordBatch,
        };
        use std::sync::Arc;
        let schema = ColumnSchema::new(vec![ColumnField::new("s", ArrowDataType::Utf8, false)]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![ColumnArray::Utf8 {
                values: vec!["a".into(), "bb".into()],
                validity: None,
            }],
        )
        .unwrap();
        let mut buf = Vec::new();
        write_ipc_stream(&mut buf, &schema, vec![Ok(batch)]).unwrap();
        fs::write(corpus.join("ipc/valid-stream"), buf)?;
    }

    // N=4: five little-endian i32 offsets [0,1,2,3,4] plus parent offset 0, length 4.
    {
        let mut buf = Vec::new();
        for v in 0i32..=4 {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf.extend_from_slice(&[0u8, 4]);
        fs::write(corpus.join("arrow_import/monotonic-offsets"), buf)?;
    }

    for (name, spec) in [
        ("pairs", "1: 10, 2: 20"),
        ("ranges", "{101..105: 1, 121..=124: 2, 141-143: 3}"),
        ("arrow", "[10, 20, 30] => 1; 40 -> 2"),
    ] {
        fs::write(corpus.join("remap").join(name), spec)?;
    }

    fs::write(corpus.join("content_range/basic"), "bytes 0-9/100")?;

    {
        use raster_h3_core::aggregator::multi_horizon::encode_partial_state;
        use raster_h3_core::aggregator::{CategoricalAccumulator, H3Accumulator};
        let mut continuous = H3Accumulator::with_quantiles();
        for v in [-3.5, 0.0, 2.0, 1e-9, 4096.0] {
            continuous.update_weighted(v, 0.5);
        }
        let mut categorical = CategoricalAccumulator::new();
        for class in -20..20 {
            categorical.update_weighted(class, 0.25);
        }
        let dir = corpus.join("partial_state");
        fs::write(dir.join("continuous"), encode_partial_state(&continuous))?;
        fs::write(dir.join("categorical"), encode_partial_state(&categorical))?;
    }
    Ok(())
}

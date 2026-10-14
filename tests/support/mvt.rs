//! Minimal independent protobuf/MVT decoder for semantic regression assertions.
use raster_h3::pmtiles::mvt::MvtValue;
use std::collections::HashMap;

pub fn varint(bytes: &mut &[u8]) -> u64 {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = bytes[0];
        *bytes = &bytes[1..];
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return value;
        }
    }
    panic!("invalid varint")
}

enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed64(f64),
    Fixed32(f32),
}
fn fields(mut bytes: &[u8]) -> Vec<(u64, Field<'_>)> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let tag = varint(&mut bytes);
        let value = match tag & 7 {
            0 => Field::Int(varint(&mut bytes)),
            1 => {
                let v = f64::from_le_bytes(bytes[..8].try_into().unwrap());
                bytes = &bytes[8..];
                Field::Fixed64(v)
            }
            2 => {
                let n = varint(&mut bytes) as usize;
                let v = &bytes[..n];
                bytes = &bytes[n..];
                Field::Bytes(v)
            }
            5 => {
                let v = f32::from_le_bytes(bytes[..4].try_into().unwrap());
                bytes = &bytes[4..];
                Field::Fixed32(v)
            }
            _ => panic!("unsupported protobuf wire type"),
        };
        result.push((tag >> 3, value));
    }
    result
}

#[derive(Debug)]
pub struct Feature {
    pub id: u64,
    pub properties: HashMap<String, MvtValue>,
    pub rings: Vec<Vec<(i32, i32)>>,
}

fn geometry(mut bytes: &[u8]) -> Vec<Vec<(i32, i32)>> {
    let mut rings = Vec::new();
    let (mut x, mut y) = (0, 0);
    while !bytes.is_empty() {
        let cmd = varint(&mut bytes);
        for _ in 0..cmd >> 3 {
            match cmd & 7 {
                1 | 2 => {
                    let dx = varint(&mut bytes) as i32;
                    let dy = varint(&mut bytes) as i32;
                    x += (dx >> 1) ^ -(dx & 1);
                    y += (dy >> 1) ^ -(dy & 1);
                    if cmd & 7 == 1 {
                        rings.push(Vec::new());
                    }
                    rings.last_mut().unwrap().push((x, y));
                }
                7 => {}
                _ => panic!("unexpected geometry command"),
            }
        }
    }
    rings
}

pub fn decode(tile: &[u8]) -> Vec<Feature> {
    let mut result = Vec::new();
    for field in fields(tile) {
        let (3, Field::Bytes(layer)) = field else {
            continue;
        };
        let mut keys = Vec::new();
        let mut values = Vec::new();
        let mut features = Vec::new();
        for (tag, field) in fields(layer) {
            match (tag, field) {
                (2, Field::Bytes(f)) => features.push(f),
                (3, Field::Bytes(k)) => keys.push(std::str::from_utf8(k).unwrap().to_owned()),
                (4, Field::Bytes(v)) => {
                    let value = fields(v)
                        .into_iter()
                        .find_map(|f| match f {
                            (1, Field::Bytes(s)) => {
                                Some(MvtValue::String(std::str::from_utf8(s).unwrap().to_owned()))
                            }
                            (2, Field::Fixed32(x)) => Some(MvtValue::Float(x)),
                            (3, Field::Fixed64(x)) => Some(MvtValue::Double(x)),
                            (4, Field::Int(x)) => Some(MvtValue::Int(x as i64)),
                            (5, Field::Int(x)) => Some(MvtValue::UInt(x)),
                            (7, Field::Int(x)) => Some(MvtValue::Bool(x != 0)),
                            _ => None,
                        })
                        .expect("MVT value");
                    values.push(value);
                }
                _ => {}
            }
        }
        for raw in features {
            let mut feature = Feature {
                id: 0,
                properties: HashMap::new(),
                rings: Vec::new(),
            };
            for (tag, field) in fields(raw) {
                match (tag, field) {
                    (1, Field::Int(id)) => feature.id = id,
                    (2, Field::Bytes(mut tags)) => {
                        while !tags.is_empty() {
                            let key = varint(&mut tags) as usize;
                            let value = varint(&mut tags) as usize;
                            assert!(feature
                                .properties
                                .insert(keys[key].clone(), values[value].clone())
                                .is_none());
                        }
                    }
                    (4, Field::Bytes(g)) => feature.rings = geometry(g),
                    _ => {}
                }
            }
            result.push(feature);
        }
    }
    result
}

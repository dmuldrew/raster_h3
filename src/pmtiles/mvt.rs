//! Mapbox Vector Tile (MVT v2) Protobuf Encoder
//!
//! Encodes H3 hexagonal geometries and statistical properties directly into
//! standard MVT protocol buffer byte streams without intermediate GIS allocations.

use h3o::LatLng;

/// Command integers in Mapbox Vector Tile specification
const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

/// ZigZag encoding for signed 32-bit integers into unsigned integers
#[inline(always)]
fn zigzag_encode(val: i32) -> u32 {
    ((val << 1) ^ (val >> 31)) as u32
}

/// Encode a Protobuf varint into a byte buffer
#[inline(always)]
fn write_varint(buf: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        buf.push(((val & 0x7F) | 0x80) as u8);
        val >>= 7;
    }
    buf.push((val & 0x7F) as u8);
}

/// Write a protobuf tag: (field_number << 3) | wire_type
#[inline(always)]
fn write_tag(buf: &mut Vec<u8>, field_number: u32, wire_type: u32) {
    write_varint(buf, ((field_number << 3) | wire_type) as u64);
}

/// Write a string field (wire type 2 - length delimited)
fn write_string_field(buf: &mut Vec<u8>, field_number: u32, s: &str) {
    write_tag(buf, field_number, 2);
    write_varint(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

/// Property value in an MVT feature
#[derive(Debug, Clone, PartialEq)]
pub enum MvtValue {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    Bool(bool),
}

impl MvtValue {
    /// Serialize an MVT Value message into protobuf bytes
    fn write_to(&self, buf: &mut Vec<u8>) {
        match self {
            MvtValue::String(s) => write_string_field(buf, 1, s),
            MvtValue::Float(f) => {
                write_tag(buf, 2, 5); // 32-bit fixed
                buf.extend_from_slice(&f.to_le_bytes());
            }
            MvtValue::Double(d) => {
                write_tag(buf, 3, 1); // 64-bit fixed
                buf.extend_from_slice(&d.to_le_bytes());
            }
            MvtValue::Int(i) => {
                write_tag(buf, 4, 0); // varint
                write_varint(buf, *i as u64);
            }
            MvtValue::UInt(u) => {
                write_tag(buf, 5, 0); // varint
                write_varint(buf, *u);
            }
            MvtValue::Bool(b) => {
                write_tag(buf, 7, 0); // varint
                write_varint(buf, if *b { 1 } else { 0 });
            }
        }
    }
}

/// A single feature inside an MVT layer
#[derive(Debug, Clone)]
pub struct MvtFeature {
    pub id: u64,
    /// Properties as (key_name, value)
    pub properties: Vec<(String, MvtValue)>,
    /// Boundary vertices in tile-local [0, 4096] integer coordinate space
    pub geometry_polygon: Vec<(i32, i32)>,
}

/// Mapbox Vector Tile layer builder
pub struct MvtLayer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<MvtFeature>,
}

impl MvtLayer {
    /// Create a new MVT layer with extent 4096 (standard vector tile grid)
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            extent: 4096,
            features: Vec::new(),
        }
    }

    /// Add an H3 hexagon feature with its boundary vertices converted to tile [0, 4096] coordinates
    pub fn add_hexagon(
        &mut self,
        id: u64,
        vertices: &[LatLng],
        tile_min_lon: f64,
        tile_max_lon: f64,
        tile_min_lat: f64,
        tile_max_lat: f64,
        properties: Vec<(String, MvtValue)>,
    ) {
        if vertices.len() < 3 {
            return;
        }

        let extent_f = self.extent as f64;
        let lon_span = tile_max_lon - tile_min_lon;
        let lat_span = tile_max_lat - tile_min_lat;

        if lon_span <= 0.0 || lat_span <= 0.0 {
            return;
        }

        let mut polygon = Vec::with_capacity(vertices.len());
        for v in vertices {
            // Map longitude to [0, extent]
            let px = ((v.lng() - tile_min_lon) / lon_span * extent_f).round() as i32;
            // Map latitude to [0, extent] (North-to-South in tile space: 0 is top/North)
            let py = ((tile_max_lat - v.lat()) / lat_span * extent_f).round() as i32;
            polygon.push((px, py));
        }

        self.features.push(MvtFeature {
            id,
            properties,
            geometry_polygon: polygon,
        });
    }

    /// Encode this layer and its features into MVT Protobuf binary bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<MvtValue> = Vec::new();

        // 1. Build dictionary for property keys and values
        let mut encoded_features: Vec<Vec<u8>> = Vec::with_capacity(self.features.len());

        for feat in &self.features {
            let mut tags: Vec<u32> = Vec::with_capacity(feat.properties.len() * 2);
            for (k, v) in &feat.properties {
                let key_idx = match keys.iter().position(|x| x == k) {
                    Some(idx) => idx as u32,
                    None => {
                        keys.push(k.clone());
                        (keys.len() - 1) as u32
                    }
                };
                let val_idx = match values.iter().position(|x| x == v) {
                    Some(idx) => idx as u32,
                    None => {
                        values.push(v.clone());
                        (values.len() - 1) as u32
                    }
                };
                tags.push(key_idx);
                tags.push(val_idx);
            }

            // Encode geometry commands: MoveTo(1) -> LineTo(N-1) -> ClosePath(1)
            let mut geom_cmds: Vec<u32> = Vec::new();
            if !feat.geometry_polygon.is_empty() {
                let p0 = feat.geometry_polygon[0];
                geom_cmds.push((CMD_MOVE_TO & 0x7) | (1 << 3));
                geom_cmds.push(zigzag_encode(p0.0));
                geom_cmds.push(zigzag_encode(p0.1));

                let line_count = (feat.geometry_polygon.len() - 1) as u32;
                if line_count > 0 {
                    geom_cmds.push((CMD_LINE_TO & 0x7) | (line_count << 3));
                    let mut prev_x = p0.0;
                    let mut prev_y = p0.1;
                    for p in &feat.geometry_polygon[1..] {
                        let dx = p.0 - prev_x;
                        let dy = p.1 - prev_y;
                        geom_cmds.push(zigzag_encode(dx));
                        geom_cmds.push(zigzag_encode(dy));
                        prev_x = p.0;
                        prev_y = p.1;
                    }
                }
                geom_cmds.push((CMD_CLOSE_PATH & 0x7) | (1 << 3));
            }

            // Serialize feature protobuf
            let mut feat_buf = Vec::new();
            if feat.id != 0 {
                write_tag(&mut feat_buf, 1, 0);
                write_varint(&mut feat_buf, feat.id);
            }
            // Tags (field 2)
            if !tags.is_empty() {
                let mut tag_bytes = Vec::new();
                for &t in &tags {
                    write_varint(&mut tag_bytes, t as u64);
                }
                write_tag(&mut feat_buf, 2, 2);
                write_varint(&mut feat_buf, tag_bytes.len() as u64);
                feat_buf.extend_from_slice(&tag_bytes);
            }
            // Geom type = 3 (POLYGON)
            write_tag(&mut feat_buf, 3, 0);
            write_varint(&mut feat_buf, 3);
            // Geometry command sequence (field 4)
            if !geom_cmds.is_empty() {
                let mut geom_bytes = Vec::new();
                for &cmd in &geom_cmds {
                    write_varint(&mut geom_bytes, cmd as u64);
                }
                write_tag(&mut feat_buf, 4, 2);
                write_varint(&mut feat_buf, geom_bytes.len() as u64);
                feat_buf.extend_from_slice(&geom_bytes);
            }

            encoded_features.push(feat_buf);
        }

        // 2. Build Layer Message
        let mut layer_buf = Vec::new();
        // Version 2
        write_tag(&mut layer_buf, 15, 0);
        write_varint(&mut layer_buf, 2);
        // Name (field 1)
        write_string_field(&mut layer_buf, 1, &self.name);
        // Features (field 2)
        for f_bytes in encoded_features {
            write_tag(&mut layer_buf, 2, 2);
            write_varint(&mut layer_buf, f_bytes.len() as u64);
            layer_buf.extend_from_slice(&f_bytes);
        }
        // Keys (field 3)
        for k in keys {
            write_string_field(&mut layer_buf, 3, &k);
        }
        // Values (field 4)
        for v in values {
            let mut val_bytes = Vec::new();
            v.write_to(&mut val_bytes);
            write_tag(&mut layer_buf, 4, 2);
            write_varint(&mut layer_buf, val_bytes.len() as u64);
            layer_buf.extend_from_slice(&val_bytes);
        }
        // Extent (field 5)
        write_tag(&mut layer_buf, 5, 0);
        write_varint(&mut layer_buf, self.extent as u64);

        // 3. Wrap in Tile Message (field 3 = layers)
        let mut tile_buf = Vec::new();
        write_tag(&mut tile_buf, 3, 2);
        write_varint(&mut tile_buf, layer_buf.len() as u64);
        tile_buf.extend_from_slice(&layer_buf);

        tile_buf
    }
}

//! Mapbox Vector Tile (MVT v2) Protobuf Encoder
//!
//! Encodes H3 hexagonal geometries and statistical properties directly into
//! standard MVT protocol buffer byte streams without intermediate GIS allocations.

use std::borrow::Cow;
use h3o::LatLng;

/// Normalized Web Mercator point with coordinates in [0.0, 1.0]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MercatorPoint {
    pub x: f64,
    pub y: f64,
}

impl MercatorPoint {
    #[inline(always)]
    pub fn from_lat_lng(lat: f64, lng: f64) -> Self {
        let x = (lng + 180.0) / 360.0;
        let lat_clamped = lat.max(-85.05112878).min(85.05112878);
        let lat_rad = lat_clamped.to_radians();
        let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0;
        Self {
            x: x.max(0.0).min(1.0),
            y: y.max(0.0).min(1.0),
        }
    }
}

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
    if val < 0x80 {
        buf.push(val as u8);
        return;
    }
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
#[derive(Debug, Clone)]
pub enum MvtValue {
    String(String),
    HexStr([u8; 16], u8),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    Bool(bool),
}

impl PartialEq for MvtValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MvtValue::String(a), MvtValue::String(b)) => a == b,
            (MvtValue::HexStr(a, len_a), MvtValue::HexStr(b, len_b)) => {
                len_a == len_b && &a[..*len_a as usize] == &b[..*len_b as usize]
            }
            (MvtValue::String(a), MvtValue::HexStr(b, len_b)) => {
                a.as_bytes() == &b[..*len_b as usize]
            }
            (MvtValue::HexStr(a, len_a), MvtValue::String(b)) => {
                &a[..*len_a as usize] == b.as_bytes()
            }
            (MvtValue::Float(a), MvtValue::Float(b)) => a.to_bits() == b.to_bits(),
            (MvtValue::Double(a), MvtValue::Double(b)) => a.to_bits() == b.to_bits(),
            (MvtValue::Int(a), MvtValue::Int(b)) => a == b,
            (MvtValue::UInt(a), MvtValue::UInt(b)) => a == b,
            (MvtValue::Bool(a), MvtValue::Bool(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for MvtValue {}

impl std::hash::Hash for MvtValue {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            MvtValue::String(s) => {
                0u8.hash(state);
                s.hash(state);
            }
            MvtValue::HexStr(bytes, len) => {
                0u8.hash(state);
                if let Ok(s) = std::str::from_utf8(&bytes[..*len as usize]) {
                    s.hash(state);
                } else {
                    bytes[..*len as usize].hash(state);
                }
            }
            MvtValue::Float(f) => {
                1u8.hash(state);
                f.to_bits().hash(state);
            }
            MvtValue::Double(d) => {
                2u8.hash(state);
                d.to_bits().hash(state);
            }
            MvtValue::Int(i) => {
                3u8.hash(state);
                i.hash(state);
            }
            MvtValue::UInt(u) => {
                4u8.hash(state);
                u.hash(state);
            }
            MvtValue::Bool(b) => {
                5u8.hash(state);
                b.hash(state);
            }
        }
    }
}

impl MvtValue {
    /// Construct a zero-allocation hex string MvtValue directly from an H3 cell integer
    #[inline(always)]
    pub fn from_hex_u64(h3_index: u64) -> Self {
        let mut buf = [0u8; 16];
        let bytes = crate::functions::fast_hex::fast_hex_u64(h3_index, &mut buf);
        let len = bytes.len() as u8;
        MvtValue::HexStr(buf, len)
    }

    /// Serialize an MVT Value message into protobuf bytes
    fn write_to(&self, buf: &mut Vec<u8>) {
        match self {
            MvtValue::String(s) => write_string_field(buf, 1, s),
            MvtValue::HexStr(bytes, len) => {
                if let Ok(s) = std::str::from_utf8(&bytes[..*len as usize]) {
                    write_string_field(buf, 1, s);
                }
            }
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

/// Zero-allocation feature properties supporting fixed continuous/categorical schemas on the stack
#[derive(Debug, Clone)]
pub enum FeatureProperties {
    Continuous {
        h3_index: u64,
        resolution: u8,
        mean: f64,
        sum: f64,
        stddev: f64,
        count: f64,
        min: f64,
        max: f64,
    },
    Categorical {
        h3_index: u64,
        resolution: u8,
        majority: i64,
        majority_fraction: f64,
        distinct_classes: u32,
        entropy: f64,
        count: f64,
    },
    Generic(Vec<(Cow<'static, str>, MvtValue)>),
}

impl Default for FeatureProperties {
    fn default() -> Self {
        FeatureProperties::Generic(Vec::new())
    }
}

impl FeatureProperties {
    #[inline(always)]
    pub fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&str, &MvtValue),
    {
        match self {
            FeatureProperties::Continuous {
                h3_index,
                resolution,
                mean,
                sum,
                stddev,
                count,
                min,
                max,
            } => {
                f("h3_index", &MvtValue::UInt(*h3_index));
                f("h3_hex", &MvtValue::from_hex_u64(*h3_index));
                f("resolution", &MvtValue::UInt(*resolution as u64));
                f("mean", &MvtValue::Double(*mean));
                f("sum", &MvtValue::Double(*sum));
                f("stddev", &MvtValue::Double(*stddev));
                f("count", &MvtValue::Double(*count));
                f("min", &MvtValue::Double(*min));
                f("max", &MvtValue::Double(*max));
            }
            FeatureProperties::Categorical {
                h3_index,
                resolution,
                majority,
                majority_fraction,
                distinct_classes,
                entropy,
                count,
            } => {
                f("h3_index", &MvtValue::UInt(*h3_index));
                f("h3_hex", &MvtValue::from_hex_u64(*h3_index));
                f("resolution", &MvtValue::UInt(*resolution as u64));
                f("majority", &MvtValue::Int(*majority));
                f("majority_fraction", &MvtValue::Double(*majority_fraction));
                f("distinct_classes", &MvtValue::UInt(*distinct_classes as u64));
                f("entropy", &MvtValue::Double(*entropy));
                f("count", &MvtValue::Double(*count));
            }
            FeatureProperties::Generic(props) => {
                for (k, v) in props {
                    f(k.as_ref(), v);
                }
            }
        }
    }
}

impl From<Vec<(Cow<'static, str>, MvtValue)>> for FeatureProperties {
    #[inline(always)]
    fn from(v: Vec<(Cow<'static, str>, MvtValue)>) -> Self {
        FeatureProperties::Generic(v)
    }
}

/// A single feature inside an MVT layer with stack-allocated polygon geometry
#[derive(Debug, Clone)]
pub struct MvtFeature {
    pub id: u64,
    /// Properties
    pub properties: FeatureProperties,
    /// Boundary vertices in tile-local [0, 4096] integer coordinate space (stack allocated)
    pub polygon_x: [i32; 8],
    pub polygon_y: [i32; 8],
    pub num_points: u8,
}

impl MvtFeature {
    /// Construct an MVT feature from normalized Mercator coordinates projected to tile space
    #[inline(always)]
    pub fn from_mercator<P: Into<FeatureProperties>>(
        id: u64,
        vertices: &[MercatorPoint],
        z: u8,
        tx: u32,
        ty: u32,
        extent: u32,
        properties: P,
    ) -> Self {
        let n = (1u32 << z) as f64;
        let extent_f = extent as f64;
        let tile_x_min = (tx as f64) / n;
        let tile_y_min = (ty as f64) / n;
        let tile_span = 1.0 / n;

        let count = vertices.len().min(8);
        let mut px = [0i32; 8];
        let mut py = [0i32; 8];
        for (i, v) in vertices.iter().take(count).enumerate() {
            px[i] = ((v.x - tile_x_min) / tile_span * extent_f).round() as i32;
            py[i] = ((v.y - tile_y_min) / tile_span * extent_f).round() as i32;
        }

        Self {
            id,
            properties: properties.into(),
            polygon_x: px,
            polygon_y: py,
            num_points: count as u8,
        }
    }
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

    /// Add an MVT feature if not already present in this layer
    #[inline(always)]
    pub fn add_or_merge_feature(&mut self, feature: MvtFeature) {
        if self.features.iter().any(|f| f.id == feature.id) {
            return;
        }
        self.features.push(feature);
    }

    /// Add an H3 hexagon feature with precalculated normalized Mercator coordinates
    pub fn add_hexagon_mercator<P: Into<FeatureProperties>>(
        &mut self,
        id: u64,
        vertices: &[MercatorPoint],
        z: u8,
        tx: u32,
        ty: u32,
        properties: P,
    ) {
        if vertices.len() < 3 {
            return;
        }

        self.features.push(MvtFeature::from_mercator(
            id,
            vertices,
            z,
            tx,
            ty,
            self.extent,
            properties,
        ));
    }

    /// Add an H3 parent hexagon feature in the layer if not already present, avoiding duplicate polygons
    pub fn add_or_merge_hexagon_mercator<P: Into<FeatureProperties>>(
        &mut self,
        id: u64,
        vertices: &[MercatorPoint],
        z: u8,
        tx: u32,
        ty: u32,
        properties: P,
    ) {
        if self.features.iter().any(|f| f.id == id) {
            return;
        }
        self.add_hexagon_mercator(id, vertices, z, tx, ty, properties);
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
        properties: Vec<(Cow<'static, str>, MvtValue)>,
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

        let max_lat_rad = tile_max_lat.to_radians();
        let min_lat_rad = tile_min_lat.to_radians();
        let y_merc_top = (1.0 - (max_lat_rad.tan() + 1.0 / max_lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0;
        let y_merc_bottom = (1.0 - (min_lat_rad.tan() + 1.0 / min_lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0;
        let merc_span = y_merc_bottom - y_merc_top;

        let count = vertices.len().min(8);
        let mut px = [0i32; 8];
        let mut py = [0i32; 8];
        for (i, v) in vertices.iter().take(count).enumerate() {
            px[i] = ((v.lng() - tile_min_lon) / lon_span * extent_f).round() as i32;
            let lat_clamped = v.lat().max(-85.05112878).min(85.05112878);
            let lat_rad = lat_clamped.to_radians();
            let y_merc = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0;
            py[i] = if merc_span > 0.0 {
                ((y_merc - y_merc_top) / merc_span * extent_f).round() as i32
            } else {
                ((tile_max_lat - v.lat()) / lat_span * extent_f).round() as i32
            };
        }

        self.features.push(MvtFeature {
            id,
            properties: properties.into(),
            polygon_x: px,
            polygon_y: py,
            num_points: count as u8,
        });
    }

    /// Encode this layer and its features into MVT Protobuf binary bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut key_map: fxhash::FxHashMap<Cow<'static, str>, u32> = fxhash::FxHashMap::default();
        let mut val_map: fxhash::FxHashMap<MvtValue, u32> = fxhash::FxHashMap::default();
        let mut keys: Vec<String> = Vec::new();
        let mut values: Vec<MvtValue> = Vec::new();

        // Reusable scratch buffers across features
        let mut tag_bytes: Vec<u8> = Vec::with_capacity(64);
        let mut geom_bytes: Vec<u8> = Vec::with_capacity(128);
        let mut feat_buf: Vec<u8> = Vec::with_capacity(256);
        let mut all_features_buf: Vec<u8> = Vec::with_capacity(self.features.len() * 128);

        for feat in &self.features {
            tag_bytes.clear();
            feat.properties.for_each(|k, v| {
                let key_idx = match key_map.get(k) {
                    Some(&idx) => idx,
                    None => {
                        let idx = keys.len() as u32;
                        keys.push(k.to_string());
                        key_map.insert(Cow::Owned(k.to_string()), idx);
                        idx
                    }
                };
                let val_idx = if k == "h3_index" || k == "h3_hex" {
                    let idx = values.len() as u32;
                    values.push(v.clone());
                    idx
                } else {
                    match val_map.get(v) {
                        Some(&idx) => idx,
                        None => {
                            let idx = values.len() as u32;
                            values.push(v.clone());
                            val_map.insert(v.clone(), idx);
                            idx
                        }
                    }
                };
                write_varint(&mut tag_bytes, key_idx as u64);
                write_varint(&mut tag_bytes, val_idx as u64);
            });

            // Encode geometry commands: MoveTo(1) -> LineTo(N-1) -> ClosePath(1) directly to geom_bytes
            geom_bytes.clear();
            if feat.num_points >= 3 {
                let p0_x = feat.polygon_x[0];
                let p0_y = feat.polygon_y[0];
                write_varint(&mut geom_bytes, ((CMD_MOVE_TO & 0x7) | (1 << 3)) as u64);
                write_varint(&mut geom_bytes, zigzag_encode(p0_x) as u64);
                write_varint(&mut geom_bytes, zigzag_encode(p0_y) as u64);

                let line_count = (feat.num_points - 1) as u32;
                write_varint(&mut geom_bytes, ((CMD_LINE_TO & 0x7) | (line_count << 3)) as u64);
                let mut prev_x = p0_x;
                let mut prev_y = p0_y;
                for i in 1..feat.num_points as usize {
                    let px = feat.polygon_x[i];
                    let py = feat.polygon_y[i];
                    let dx = px - prev_x;
                    let dy = py - prev_y;
                    write_varint(&mut geom_bytes, zigzag_encode(dx) as u64);
                    write_varint(&mut geom_bytes, zigzag_encode(dy) as u64);
                    prev_x = px;
                    prev_y = py;
                }
                write_varint(&mut geom_bytes, ((CMD_CLOSE_PATH & 0x7) | (1 << 3)) as u64);
            }

            // Serialize feature protobuf into feat_buf
            feat_buf.clear();
            if feat.id != 0 {
                write_tag(&mut feat_buf, 1, 0);
                write_varint(&mut feat_buf, feat.id);
            }
            // Tags (field 2)
            if !tag_bytes.is_empty() {
                write_tag(&mut feat_buf, 2, 2);
                write_varint(&mut feat_buf, tag_bytes.len() as u64);
                feat_buf.extend_from_slice(&tag_bytes);
            }
            // Geom type = 3 (POLYGON)
            write_tag(&mut feat_buf, 3, 0);
            write_varint(&mut feat_buf, 3);
            // Geometry command sequence (field 4)
            if !geom_bytes.is_empty() {
                write_tag(&mut feat_buf, 4, 2);
                write_varint(&mut feat_buf, geom_bytes.len() as u64);
                feat_buf.extend_from_slice(&geom_bytes);
            }

            // Append feature to all_features_buf with layer field 2 tag
            write_tag(&mut all_features_buf, 2, 2);
            write_varint(&mut all_features_buf, feat_buf.len() as u64);
            all_features_buf.extend_from_slice(&feat_buf);
        }

        // 2. Build Layer Message
        let mut layer_buf = Vec::with_capacity(all_features_buf.len() + 1024);
        // Version 2
        write_tag(&mut layer_buf, 15, 0);
        write_varint(&mut layer_buf, 2);
        // Name (field 1)
        write_string_field(&mut layer_buf, 1, &self.name);
        // Features (field 2)
        layer_buf.extend_from_slice(&all_features_buf);
        // Keys (field 3)
        for k in keys {
            write_string_field(&mut layer_buf, 3, &k);
        }
        // Values (field 4)
        let mut val_bytes = Vec::with_capacity(32);
        for v in values {
            val_bytes.clear();
            v.write_to(&mut val_bytes);
            write_tag(&mut layer_buf, 4, 2);
            write_varint(&mut layer_buf, val_bytes.len() as u64);
            layer_buf.extend_from_slice(&val_bytes);
        }
        // Extent (field 5)
        write_tag(&mut layer_buf, 5, 0);
        write_varint(&mut layer_buf, self.extent as u64);

        // 3. Wrap in Tile Message (field 3 = layers)
        let mut tile_buf = Vec::with_capacity(layer_buf.len() + 16);
        write_tag(&mut tile_buf, 3, 2);
        write_varint(&mut tile_buf, layer_buf.len() as u64);
        tile_buf.extend_from_slice(&layer_buf);

        tile_buf
    }
}

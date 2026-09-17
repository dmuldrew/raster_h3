//! Parquet to PMTiles v3 Transcoding Engine
//!
//! Streams any H3-indexed Parquet dataset directly into an optimized,
//! multi-zoom PMTiles v3 vector hexagon archive with streaming latitude
//! horizon eviction to maintain a bounded memory footprint.

use h3o::{CellIndex, LatLng};
use parquet::file::reader::{FileReader, RowGroupReader, SerializedFileReader};
use serde_json::json;
use std::borrow::Cow;
use std::fs::File;
use std::path::Path;

use crate::encoding::parse_hex_u64;
use crate::pmtiles::features::{
    build_pmtiles_metadata, PmtilesExportSummary, TilePyramidAccumulator,
};
use crate::pmtiles::mvt::{MercatorPoint, MvtValue};
use crate::pmtiles::pyramid::{cell_boundary_mercator, h3_res_to_zoom, max_hex_radius_deg};
use crate::pmtiles::writer::PmtilesWriter;

/// Extent and statistics for a Parquet row group discovered during pre-scan
#[derive(Debug, Clone, Copy)]
pub struct RowGroupExtent {
    pub rg_idx: usize,
    pub min_lat: f64,
    pub max_lat: f64,
    pub min_lon: f64,
    pub max_lon: f64,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_res: u8,
}

/// Pre-scanned geographical and resolution extent of a Parquet row group
pub fn scan_row_group_h3_extent(
    rg: &dyn RowGroupReader,
    h3_idx: usize,
    rg_idx: usize,
) -> Result<Option<RowGroupExtent>, Box<dyn std::error::Error + Send + Sync>> {
    let col_reader = match rg.get_column_reader(h3_idx) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };

    let mut min_lat = 90.0f64;
    let mut max_lat = -90.0f64;
    let mut min_lon = 180.0f64;
    let mut max_lon = -180.0f64;
    let mut min_zoom = 255u8;
    let mut max_zoom = 0u8;
    let mut min_res = 255u8;
    let mut count = 0usize;

    let mut process_h3 = |h3: u64| {
        if let Ok(cell) = CellIndex::try_from(h3) {
            let center: LatLng = cell.into();
            let lat = center.lat();
            let lon = center.lng();
            if lat < min_lat {
                min_lat = lat;
            }
            if lat > max_lat {
                max_lat = lat;
            }
            if lon < min_lon {
                min_lon = lon;
            }
            if lon > max_lon {
                max_lon = lon;
            }
            let res: u8 = cell.resolution().into();
            if res < min_res {
                min_res = res;
            }
            let zoom = h3_res_to_zoom(res);
            if zoom < min_zoom {
                min_zoom = zoom;
            }
            if zoom > max_zoom {
                max_zoom = zoom;
            }
            count += 1;
        }
    };

    match col_reader {
        parquet::column::reader::ColumnReader::Int64ColumnReader(mut r) => {
            let mut vals = Vec::with_capacity(8192);
            loop {
                vals.clear();
                let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                if read == 0 {
                    break;
                }
                for &v in &vals {
                    process_h3(v as u64);
                }
            }
        }
        parquet::column::reader::ColumnReader::Int32ColumnReader(mut r) => {
            let mut vals = Vec::with_capacity(8192);
            loop {
                vals.clear();
                let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                if read == 0 {
                    break;
                }
                for &v in &vals {
                    process_h3(v as u64);
                }
            }
        }
        parquet::column::reader::ColumnReader::ByteArrayColumnReader(mut r) => {
            let mut vals = Vec::with_capacity(8192);
            loop {
                vals.clear();
                let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                if read == 0 {
                    break;
                }
                for v in &vals {
                    if let Ok(s) = std::str::from_utf8(v.data()) {
                        if let Some(h3) = parse_hex_u64(s) {
                            process_h3(h3);
                        }
                    }
                }
            }
        }
        parquet::column::reader::ColumnReader::FixedLenByteArrayColumnReader(mut r) => {
            let mut vals = Vec::with_capacity(8192);
            loop {
                vals.clear();
                let (read, _, _) = r.read_records(8192, None, None, &mut vals)?;
                if read == 0 {
                    break;
                }
                for v in &vals {
                    if let Ok(s) = std::str::from_utf8(v.data()) {
                        if let Some(h3) = parse_hex_u64(s) {
                            process_h3(h3);
                        }
                    }
                }
            }
        }
        _ => return Ok(None),
    }

    if count == 0 {
        Ok(None)
    } else {
        Ok(Some(RowGroupExtent {
            rg_idx,
            min_lat,
            max_lat,
            min_lon,
            max_lon,
            min_zoom,
            max_zoom,
            min_res,
        }))
    }
}

/// Convert any H3-indexed Parquet file directly into a PMTiles v3 archive with streaming horizon eviction
pub fn process_parquet_to_pmtiles<P1: AsRef<Path>, P2: AsRef<Path>>(
    parquet_path: P1,
    pmtiles_path: P2,
    h3_column_name: Option<&str>,
) -> Result<PmtilesExportSummary, Box<dyn std::error::Error + Send + Sync>> {
    let file = File::open(parquet_path)?;
    let reader = SerializedFileReader::new(file)?;
    let num_rgs = reader.num_row_groups();
    let schema = reader.metadata().file_metadata().schema_descr();

    // Identify H3 index column
    let mut h3_col_idx = None;
    if let Some(target) = h3_column_name {
        for (idx, field) in schema.columns().iter().enumerate() {
            if field.name().eq_ignore_ascii_case(target) {
                h3_col_idx = Some(idx);
                break;
            }
        }
    }

    if h3_col_idx.is_none() {
        // Auto-detect common H3 column names: h3_index, h3_hex, h3, cell, hex, or column 0
        for (idx, field) in schema.columns().iter().enumerate() {
            let name = field.name().to_ascii_lowercase();
            if name == "h3_index"
                || name == "h3_hex"
                || name == "h3"
                || name == "cell"
                || name == "hex"
            {
                h3_col_idx = Some(idx);
                break;
            }
        }
    }

    let h3_idx = h3_col_idx.unwrap_or(0);

    // Pre-scan row group extents to compute global bounds and row group horizons
    let mut rg_extents = Vec::with_capacity(num_rgs);
    for rg_i in 0..num_rgs {
        if let Ok(rg) = reader.get_row_group(rg_i) {
            if let Ok(Some(ext)) = scan_row_group_h3_extent(&*rg, h3_idx, rg_i) {
                rg_extents.push(ext);
            }
        }
    }

    // If no extents found or empty file, fallback to empty summary
    if rg_extents.is_empty() {
        let metadata = build_pmtiles_metadata(
            "h3_pmtiles_export",
            "H3 vector hexagon tile pyramid exported by raster_h3",
            0,
            0,
            "h3_hexagons",
            "H3 hexagonal vector polygons with attributes",
            json!({}),
            None,
        );
        let writer = PmtilesWriter::new(0, 0, [-180.0, -85.0, 180.0, 85.0], metadata.to_string())?;
        writer.finish(pmtiles_path)?;
        return Ok(PmtilesExportSummary {
            total_features: 0,
            valid_features: 0,
            invalid_features_dropped: 0,
            total_tiles: 0,
            min_zoom: 0,
            max_zoom: 0,
        });
    }

    let global_min_lon = rg_extents
        .iter()
        .map(|e| e.min_lon)
        .fold(180.0f64, f64::min);
    let global_min_lat = rg_extents.iter().map(|e| e.min_lat).fold(90.0f64, f64::min);
    let global_max_lon = rg_extents
        .iter()
        .map(|e| e.max_lon)
        .fold(-180.0f64, f64::max);
    let global_max_lat = rg_extents
        .iter()
        .map(|e| e.max_lat)
        .fold(-90.0f64, f64::max);
    let min_zoom = rg_extents.iter().map(|e| e.min_zoom).min().unwrap_or(0);
    let max_zoom = rg_extents.iter().map(|e| e.max_zoom).max().unwrap_or(0);
    let min_res = rg_extents.iter().map(|e| e.min_res).min().unwrap_or(0);
    let max_cell_radius = max_hex_radius_deg(min_res);
    let safety_margin = 2.5 * max_cell_radius;

    // Schedule row groups North-to-South (descending max_lat)
    let mut scheduled_rgs: Vec<RowGroupExtent> = rg_extents;
    scheduled_rgs.sort_by(|a, b| b.max_lat.total_cmp(&a.max_lat));

    // Compute future horizons
    let n_rgs = scheduled_rgs.len();
    let mut future_horizons = vec![-90.0f64; n_rgs];
    let mut max_future = -90.0f64;
    for k in (0..n_rgs).rev() {
        future_horizons[k] = max_future;
        max_future = max_future.max(scheduled_rgs[k].max_lat);
    }

    // Build dynamic fields metadata for vector layer
    let mut fields_map = serde_json::Map::new();
    fields_map.insert("h3_index".to_string(), json!("Number"));
    fields_map.insert("h3_hex".to_string(), json!("String"));
    fields_map.insert("resolution".to_string(), json!("Number"));
    for (idx, col) in schema.columns().iter().enumerate() {
        if idx == h3_idx {
            continue;
        }
        let type_str = match col.physical_type() {
            parquet::basic::Type::INT32
            | parquet::basic::Type::INT64
            | parquet::basic::Type::INT96 => "Number",
            parquet::basic::Type::FLOAT | parquet::basic::Type::DOUBLE => "Number",
            parquet::basic::Type::BYTE_ARRAY | parquet::basic::Type::FIXED_LEN_BYTE_ARRAY => {
                "String"
            }
            parquet::basic::Type::BOOLEAN => "Boolean",
        };
        fields_map.insert(col.name().to_string(), json!(type_str));
    }

    let metadata = build_pmtiles_metadata(
        "h3_pmtiles_export",
        "H3 vector hexagon tile pyramid exported by raster_h3",
        min_zoom,
        max_zoom,
        "h3_hexagons",
        "H3 hexagonal vector polygons with attributes",
        serde_json::Value::Object(fields_map),
        None,
    );

    let mut writer = PmtilesWriter::new(
        min_zoom,
        max_zoom,
        [
            global_min_lon,
            global_min_lat,
            global_max_lon,
            global_max_lat,
        ],
        metadata.to_string(),
    )?;

    let mut accumulator = TilePyramidAccumulator::new(safety_margin);

    let mut total_features = 0usize;
    let mut valid_features = 0usize;
    let mut invalid_dropped = 0usize;

    // Pre-extract column names once so we don't allocate String per field per row
    let col_names: Vec<Cow<'static, str>> = schema
        .columns()
        .iter()
        .map(|f| Cow::Owned(f.name().to_string()))
        .collect();

    for (k, rg_ext) in scheduled_rgs.iter().enumerate() {
        let rg = reader.get_row_group(rg_ext.rg_idx)?;
        let row_iter = rg.get_row_iter(None)?;

        for row_result in row_iter {
            total_features += 1;
            let row = match row_result {
                Ok(r) => r,
                Err(_) => {
                    invalid_dropped += 1;
                    continue;
                }
            };

            let mut h3_val_u64 = None;
            if let Some((_, field_val)) = row.get_column_iter().nth(h3_idx) {
                match field_val {
                    parquet::record::Field::ULong(u) => h3_val_u64 = Some(*u),
                    parquet::record::Field::Long(i) => h3_val_u64 = Some(*i as u64),
                    parquet::record::Field::Str(s) => {
                        h3_val_u64 = parse_hex_u64(s);
                    }
                    parquet::record::Field::Bytes(b) => {
                        if let Ok(s) = std::str::from_utf8(b.data()) {
                            h3_val_u64 = parse_hex_u64(s);
                        }
                    }
                    _ => {}
                }
            }

            let h3_u64 = match h3_val_u64 {
                Some(h) => h,
                None => {
                    invalid_dropped += 1;
                    continue;
                }
            };

            let cell = match CellIndex::try_from(h3_u64) {
                Ok(c) => c,
                Err(_) => {
                    invalid_dropped += 1;
                    continue;
                }
            };
            valid_features += 1;

            let center: LatLng = cell.into();
            let c_lat = center.lat();
            let c_lon = center.lng();

            let center_merc = MercatorPoint::from_lat_lng(c_lat, c_lon);
            let (v_merc, v_count) = cell_boundary_mercator(cell);
            let vertices_merc = &v_merc[..v_count];

            let res_u8: u8 = cell.resolution().into();
            let zoom = h3_res_to_zoom(res_u8);

            let mut properties = Vec::with_capacity(col_names.len().saturating_sub(1) + 3);
            for (col_i, (_, field_val)) in row.get_column_iter().enumerate() {
                if col_i == h3_idx {
                    continue;
                }
                let mvt_val = match field_val {
                    parquet::record::Field::Double(d) => MvtValue::Double(*d),
                    parquet::record::Field::Float(f) => MvtValue::Float(*f),
                    parquet::record::Field::Long(i) => MvtValue::Int(*i),
                    parquet::record::Field::ULong(u) => MvtValue::UInt(*u),
                    parquet::record::Field::Int(i) => MvtValue::Int(*i as i64),
                    parquet::record::Field::UInt(u) => MvtValue::UInt(*u as u64),
                    parquet::record::Field::Short(s) => MvtValue::Int(*s as i64),
                    parquet::record::Field::UShort(u) => MvtValue::UInt(*u as u64),
                    parquet::record::Field::Byte(b) => MvtValue::Int(*b as i64),
                    parquet::record::Field::UByte(u) => MvtValue::UInt(*u as u64),
                    parquet::record::Field::Str(s) => MvtValue::String(s.clone()),
                    parquet::record::Field::Bool(b) => MvtValue::Bool(*b),
                    _ => continue,
                };
                if let Some(col_name) = col_names.get(col_i) {
                    properties.push((col_name.clone(), mvt_val));
                }
            }

            if !properties.iter().any(|(k, _)| k == "h3_index") {
                properties.push((Cow::Borrowed("h3_index"), MvtValue::UInt(h3_u64)));
            }
            if !properties.iter().any(|(k, _)| k == "h3_hex") {
                properties.push((Cow::Borrowed("h3_hex"), MvtValue::from_hex_u64(h3_u64)));
            }
            if !properties.iter().any(|(k, _)| k == "resolution") {
                properties.push((Cow::Borrowed("resolution"), MvtValue::UInt(res_u8 as u64)));
            }

            accumulator.add_hexagon_mercator(h3_u64, center_merc, vertices_merc, zoom, properties);
        }

        // Streaming Horizon Eviction: Evict and compress all tiles completed prior to the remaining horizon
        let lat_horizon = future_horizons[k];
        accumulator.evict_and_write_tiles(lat_horizon, &mut writer)?;
    }

    let total_tiles = writer.tile_count() + accumulator.active_tile_count();
    accumulator.flush_all(&mut writer)?;
    writer.finish(pmtiles_path)?;

    Ok(PmtilesExportSummary {
        total_features,
        valid_features,
        invalid_features_dropped: invalid_dropped,
        total_tiles,
        min_zoom,
        max_zoom,
    })
}

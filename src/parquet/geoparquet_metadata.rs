//! OGC GeoParquet 1.1 JSON Metadata Builder
//!
//! Generates specification-compliant GeoParquet 1.1 JSON metadata embedded within
//! the Parquet `FileMetaData` key-value store, including official PROJJSON `OGC:CRS84`
//! datum ensemble definitions, planar edge definitions, and per-column bounding boxes.

use serde_json::json;

/// Build an OGC GeoParquet 1.1 compliant JSON metadata object for Parquet FileMetaData key-value store
pub fn build_geoparquet_metadata(primary_column: &str, bbox: [f64; 4]) -> String {
    let geo = json!({
        "version": "1.1.0",
        "primary_column": primary_column,
        "columns": {
            primary_column: {
                "encoding": "WKB",
                "geometry_types": ["Polygon"],
                "crs": {
                    "$schema": "https://proj.org/schemas/v0.7/projjson.schema.json",
                    "type": "GeographicCRS",
                    "name": "WGS 84 (CRS84)",
                    "datum_ensemble": {
                        "name": "World Geodetic System 1984 ensemble",
                        "members": [
                            { "name": "World Geodetic System 1984 (Transit)" },
                            { "name": "World Geodetic System 1984 (G730)" },
                            { "name": "World Geodetic System 1984 (G873)" },
                            { "name": "World Geodetic System 1984 (G1150)" },
                            { "name": "World Geodetic System 1984 (G1674)" },
                            { "name": "World Geodetic System 1984 (G1762)" },
                            { "name": "World Geodetic System 1984 (G2139)" }
                        ],
                        "ellipsoid": {
                            "name": "WGS 84",
                            "semi_major_axis": 6378137.0,
                            "inverse_flattening": 298.257223563
                        },
                        "accuracy": "2.0"
                    },
                    "coordinate_system": {
                        "subtype": "ellipsoidal",
                        "axis": [
                            {
                                "name": "Geodetic longitude",
                                "abbreviation": "Lon",
                                "direction": "east",
                                "unit": "degree"
                            },
                            {
                                "name": "Geodetic latitude",
                                "abbreviation": "Lat",
                                "direction": "north",
                                "unit": "degree"
                            }
                        ]
                    },
                    "id": {
                        "authority": "OGC",
                        "code": "CRS84"
                    }
                },
                "bbox": [bbox[0], bbox[1], bbox[2], bbox[3]],
                "edges": "planar"
            }
        }
    });
    geo.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_geoparquet_metadata_structure() {
        let meta_str = build_geoparquet_metadata("geometry", [-122.5, 37.5, -122.0, 38.0]);
        let parsed: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        assert_eq!(parsed["version"], "1.1.0");
        assert_eq!(parsed["primary_column"], "geometry");
        assert_eq!(parsed["columns"]["geometry"]["encoding"], "WKB");
        assert_eq!(
            parsed["columns"]["geometry"]["bbox"],
            serde_json::json!([-122.5, 37.5, -122.0, 38.0])
        );
    }
}

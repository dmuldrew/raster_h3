//! `string_to_h3`, `h3_to_lat` and `h3_to_lng` UDFs against `h3o` and the core scalars.

use std::str::FromStr;

use h3o::{CellIndex, LatLng, Resolution};
use raster_h3_core::columnar::{ArrowDataType, ColumnArray};
use raster_h3_core::scalar_ops;
use raster_h3_datafusion::{RasterH3SessionCatalog, ScalarUDFImpl, StringToH3Udf};

/// Cells spread over the globe, including a pentagon and cells near the poles and antimeridian.
fn sample_cells() -> Vec<u64> {
    let mut cells: Vec<u64> = [
        (37.77, -122.42, Resolution::Eight),
        (-33.86, 151.21, Resolution::Five),
        (89.9, 0.0, Resolution::Three),
        (-89.9, 45.0, Resolution::Ten),
        (0.0, 179.999, Resolution::Twelve),
        (0.0, -179.999, Resolution::Fifteen),
    ]
    .iter()
    .map(|&(lat, lng, res)| u64::from(LatLng::new(lat, lng).unwrap().to_cell(res)))
    .collect();
    cells.push(u64::from(
        CellIndex::base_cells()
            .find(|c| c.is_pentagon())
            .unwrap()
            .center_child(Resolution::Seven)
            .unwrap(),
    ));
    cells
}

#[test]
fn string_to_h3_parses_like_h3o_and_keeps_validity() {
    let ctx = RasterH3SessionCatalog::new();
    let cells = sample_cells();
    let mut strings: Vec<String> = cells
        .iter()
        .map(|&c| CellIndex::try_from(c).unwrap().to_string())
        .collect();
    // Upper-case hex parses too.
    strings.push(strings[0].to_uppercase());
    strings.push("not-hex".into());
    strings.push(String::new());
    let n = strings.len();
    let mut validity = vec![true; n];
    validity[n - 1] = false;

    let out = ctx
        .invoke_scalar(
            "STRING_TO_H3",
            &[ColumnArray::Utf8 {
                values: strings.clone(),
                validity: Some(validity.clone()),
            }],
        )
        .unwrap();
    let ColumnArray::UInt64 {
        values,
        validity: out_validity,
    } = out
    else {
        panic!("expected UInt64");
    };
    for (i, &c) in cells.iter().enumerate() {
        assert_eq!(values[i], c);
        assert_eq!(CellIndex::from_str(&strings[i]).map(u64::from), Ok(c));
    }
    assert_eq!(values[cells.len()], cells[0]);
    // Unparseable text maps to 0, like the core scalar; input validity is passed through.
    assert_eq!(values[n - 2], scalar_ops::string_to_h3("not-hex"));
    assert_eq!(values[n - 2], 0);
    assert_eq!(out_validity, Some(validity));

    let udf = StringToH3Udf;
    assert_eq!(
        udf.return_type(&[ArrowDataType::Utf8]).unwrap(),
        ArrowDataType::UInt64
    );
    assert!(udf.return_type(&[ArrowDataType::UInt64]).is_err());
    assert!(udf
        .invoke(&[ColumnArray::UInt64 {
            values: vec![1],
            validity: None
        }])
        .is_err());
    assert!(udf.invoke(&[]).is_err());
}

#[test]
fn h3_to_lat_and_lng_return_h3o_cell_centres() {
    let ctx = RasterH3SessionCatalog::new();
    let mut cells = sample_cells();
    cells.push(0);
    cells.push(u64::MAX);
    let n = cells.len();
    let mut validity = vec![true; n];
    validity[0] = false;
    let input = ColumnArray::UInt64 {
        values: cells.clone(),
        validity: Some(validity.clone()),
    };

    for name in ["h3_to_lat", "h3_to_lng"] {
        let udf = ctx.udf(name).unwrap();
        assert_eq!(udf.name(), name);
        assert_eq!(
            udf.return_type(&[ArrowDataType::UInt64]).unwrap(),
            ArrowDataType::Float64
        );
        assert!(udf.return_type(&[ArrowDataType::Utf8]).is_err());
        assert!(udf
            .invoke(&[ColumnArray::Utf8 {
                values: vec!["8828308281fffff".into()],
                validity: None
            }])
            .is_err());

        let ColumnArray::Float64 {
            values,
            validity: out_validity,
        } = ctx
            .invoke_scalar(name, std::slice::from_ref(&input))
            .unwrap()
        else {
            panic!("expected Float64");
        };
        assert_eq!(out_validity.as_ref(), Some(&validity));
        for (i, &cell) in cells.iter().enumerate() {
            let core = if name == "h3_to_lat" {
                scalar_ops::h3_to_lat(cell)
            } else {
                scalar_ops::h3_to_lng(cell)
            };
            match CellIndex::try_from(cell) {
                Ok(idx) => {
                    let ll = LatLng::from(idx);
                    let want = if name == "h3_to_lat" {
                        ll.lat()
                    } else {
                        ll.lng()
                    };
                    assert_eq!(values[i], want, "{name}({cell:#x})");
                    assert_eq!(values[i], core);
                }
                // Invalid cells give NaN rather than a bogus coordinate.
                Err(_) => assert!(values[i].is_nan() && core.is_nan(), "{name}({cell:#x})"),
            }
        }
        // Latitude and longitude stay in range.
        let bound = if name == "h3_to_lat" { 90.0 } else { 180.0 };
        assert!(values[..n - 2].iter().all(|v| v.abs() <= bound));
    }
}

#[test]
fn string_round_trip_through_lat_lng_matches_cell_centre() {
    let ctx = RasterH3SessionCatalog::new();
    let cells = sample_cells();
    let hex = ctx
        .invoke_scalar(
            "h3_to_string",
            &[ColumnArray::UInt64 {
                values: cells.clone(),
                validity: None,
            }],
        )
        .unwrap();
    let parsed = ctx.invoke_scalar("string_to_h3", &[hex]).unwrap();
    let lat = ctx
        .invoke_scalar("h3_to_lat", std::slice::from_ref(&parsed))
        .unwrap();
    let lng = ctx.invoke_scalar("h3_to_lng", &[parsed]).unwrap();
    let (ColumnArray::Float64 { values: lat, .. }, ColumnArray::Float64 { values: lng, .. }) =
        (lat, lng)
    else {
        panic!("expected Float64 columns");
    };
    for (i, &cell) in cells.iter().enumerate() {
        let res = CellIndex::try_from(cell).unwrap().resolution();
        let back = LatLng::new(lat[i], lng[i]).unwrap().to_cell(res);
        assert_eq!(u64::from(back), cell);
    }
}

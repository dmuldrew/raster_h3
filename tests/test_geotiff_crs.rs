//! CRS detection when opening real GeoTIFF files.

use std::fs::File;
use std::io::BufWriter;

use raster_h3::crs::transformer::CrsTransformer;
use raster_h3::raster::geotiff::GeoTiffStreamReader;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

const ALBERS_WKT: &str = r#"PROJCS["Albers_Conic_Equal_Area",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]"#;

/// GDAL writes user-defined ESRI projections as a citation of the form
/// `ESRI PE String = PROJCS[...]|` in GeoAsciiParamsTag.
#[test]
fn esri_pe_string_citation_is_detected_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("esri_pe.tif");
    let citation = format!("ESRI PE String = {ALBERS_WKT}|");
    // ModelTypeProjected, user-defined ProjectedCRS, citation in GeoAsciiParams.
    let geokeys: [u16; 16] = [
        1,
        1,
        0,
        3, //
        1024,
        0,
        1,
        1, //
        1026,
        34737,
        citation.len() as u16,
        0, //
        3072,
        0,
        1,
        32767,
    ];
    {
        let mut encoder = TiffEncoder::new(BufWriter::new(File::create(&path).unwrap())).unwrap();
        let mut image = encoder.new_image::<colortype::Gray32Float>(4, 4).unwrap();
        let enc = image.encoder();
        enc.write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, 1_500_000.0, 1_900_000.0, 0.0][..],
        )
        .unwrap();
        enc.write_tag(Tag::ModelPixelScaleTag, &[30.0, 30.0, 0.0][..])
            .unwrap();
        enc.write_tag(Tag::Unknown(34735), &geokeys[..]).unwrap();
        enc.write_tag(Tag::Unknown(34737), citation.as_str())
            .unwrap();
        image.write_data(&[1.0f32; 16]).unwrap();
    }

    let reader = GeoTiffStreamReader::open(&path).unwrap();
    assert_eq!(reader.metadata.epsg, None);
    let proj = reader
        .metadata
        .proj_string
        .clone()
        .expect("CRS lost from ESRI PE citation");
    assert!(
        proj.contains("+proj=aea") && proj.contains("+datum=NAD83"),
        "{proj}"
    );
    let tf = CrsTransformer::from_crs_or_epsg(None, Some(&proj)).unwrap();
    let ref5070 = CrsTransformer::from_epsg_code(5070).unwrap();
    let (a, b) = (
        tf.transform_point(1_500_000.0, 1_900_000.0).unwrap(),
        ref5070.transform_point(1_500_000.0, 1_900_000.0).unwrap(),
    );
    assert!((a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9);
}

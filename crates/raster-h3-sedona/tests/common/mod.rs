//! Shared test fixtures.
#![allow(dead_code)]

use std::fs::File;

use datafusion::arrow::array::RecordBatch;
use datafusion::prelude::SessionContext;
use tempfile::NamedTempFile;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;

/// A 0.001° EPSG:4326 raster over San Francisco whose values increase left to right.
pub fn gradient_tif(width: u32, height: u32) -> NamedTempFile {
    write_tif(width, height, true)
}

/// `geokeys: false` omits the GeoKey directory, so the CRS must be given explicitly.
pub fn write_tif(width: u32, height: u32, geokeys: bool) -> NamedTempFile {
    let tmp = NamedTempFile::new().unwrap();
    let file = File::create(tmp.path()).unwrap();
    let mut encoder = TiffEncoder::new(file).unwrap();
    let mut img = encoder.new_image::<Gray32Float>(width, height).unwrap();
    img.encoder()
        .write_tag(
            tiff::tags::Tag::Unknown(33550),
            &[0.001f64, 0.001f64, 0.0f64][..],
        )
        .unwrap();
    img.encoder()
        .write_tag(
            tiff::tags::Tag::Unknown(33922),
            &[0.0f64, 0.0f64, 0.0f64, -122.45f64, 37.80f64, 0.0f64][..],
        )
        .unwrap();
    if geokeys {
        // GTModelType = geographic, GeographicType = EPSG:4326.
        img.encoder()
            .write_tag(
                tiff::tags::Tag::Unknown(34735),
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
    }
    let data: Vec<f32> = (0..height)
        .flat_map(|_| (0..width).map(|x| x as f32))
        .collect();
    img.write_data(&data).unwrap();
    tmp
}

pub async fn collect(ctx: &SessionContext, sql: &str) -> Vec<RecordBatch> {
    ctx.sql(sql).await.unwrap().collect().await.unwrap()
}

pub fn rows(batches: &[RecordBatch]) -> usize {
    batches.iter().map(|b| b.num_rows()).sum()
}

pub fn source(tif: &NamedTempFile) -> String {
    tif.path().to_string_lossy().into_owned()
}

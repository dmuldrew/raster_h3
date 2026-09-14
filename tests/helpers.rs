#![allow(dead_code)]

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::{CompressionMethod, Tag};

/// Standard GeoKeys for EPSG:4326
const WGS84_GEOKEYS: [u16; 12] = [
    1, 1, 0, 2,
    1024, 0, 1, 2,
    2048, 0, 1, 4326,
];

/// Builder for generating GeoTIFF test files with custom dimensions, coordinates, and pixel values.
#[derive(Debug, Clone)]
pub struct TestGeoTiffBuilder {
    pub width: u32,
    pub height: u32,
    pub origin_lon: f64,
    pub origin_lat: f64,
    pub pixel_size_x: f64,
    pub pixel_size_y: f64,
    pub epsg: u16,
}

impl Default for TestGeoTiffBuilder {
    fn default() -> Self {
        Self {
            width: 64,
            height: 64,
            origin_lon: -122.45,
            origin_lat: 37.80,
            pixel_size_x: 0.0005,
            pixel_size_y: 0.0005,
            epsg: 4326,
        }
    }
}

impl TestGeoTiffBuilder {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            ..Default::default()
        }
    }

    pub fn origin(mut self, lon: f64, lat: f64) -> Self {
        self.origin_lon = lon;
        self.origin_lat = lat;
        self
    }

    pub fn pixel_size(mut self, size: f64) -> Self {
        self.pixel_size_x = size;
        self.pixel_size_y = size;
        self
    }

    pub fn pixel_size_xy(mut self, size_x: f64, size_y: f64) -> Self {
        self.pixel_size_x = size_x;
        self.pixel_size_y = size_y;
        self
    }

    pub fn epsg(mut self, code: u16) -> Self {
        self.epsg = code;
        self
    }

    /// Write Gray8 pixels with a constant fill value to the target path.
    pub fn write_gray8_constant<P: AsRef<Path>>(&self, path: P, fill_val: u8) -> std::io::Result<()> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let mut image = encoder
            .new_image::<colortype::Gray8>(self.width, self.height)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, self.origin_lon, self.origin_lat, 0.0][..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[self.pixel_size_x, self.pixel_size_y, 0.0][..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let geokeys: [u16; 12] = if self.epsg == 4326 {
            WGS84_GEOKEYS
        } else {
            [
                1, 1, 0, 2,
                1024, 0, 1, 1, // ModelTypeProjected
                3072, 0, 1, self.epsg,
            ]
        };
        image
            .encoder()
            .write_tag(Tag::Unknown(34735), &geokeys[..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let data = vec![fill_val; (self.width * self.height) as usize];
        image
            .write_data(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    /// Write Gray32Float pixels with a constant fill value to the target path.
    pub fn write_f32_constant<P: AsRef<Path>>(&self, path: P, fill_val: f32) -> std::io::Result<()> {
        self.write_f32_fn(path, |_c, _r| fill_val)
    }

    /// Write Gray32Float pixels evaluated from a closure `(col, row) -> f32` to the target path.
    pub fn write_f32_fn<P: AsRef<Path>, F: Fn(u32, u32) -> f32>(&self, path: P, val_fn: F) -> std::io::Result<()> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let mut image = encoder
            .new_image::<colortype::Gray32Float>(self.width, self.height)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        image
            .encoder()
            .write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, self.origin_lon, self.origin_lat, 0.0][..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[self.pixel_size_x, self.pixel_size_y, 0.0][..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let geokeys: [u16; 12] = if self.epsg == 4326 {
            WGS84_GEOKEYS
        } else {
            [
                1, 1, 0, 2,
                1024, 0, 1, 1, // ModelTypeProjected
                3072, 0, 1, self.epsg,
            ]
        };
        image
            .encoder()
            .write_tag(Tag::Unknown(34735), &geokeys[..])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let mut data = Vec::with_capacity((self.width * self.height) as usize);
        for row in 0..self.height {
            for col in 0..self.width {
                data.push(val_fn(col, row));
            }
        }
        image
            .write_data(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    /// Write standard synthetic continuous wave pattern: `sin(r * 0.1) * 20.0 + cos(c * 0.1) * 15.0 + 100.0`
    pub fn write_wave_f32<P: AsRef<Path>>(&self, path: P) -> std::io::Result<()> {
        self.write_f32_fn(path, |col, row| {
            let r_f = row as f32;
            let c_f = col as f32;
            (r_f * 0.1).sin() * 20.0 + (c_f * 0.1).cos() * 15.0 + 100.0
        })
    }

    /// Create a temporary GeoTIFF with the standard wave pattern.
    pub fn create_wave_tempfile(&self) -> NamedTempFile {
        let temp_file = NamedTempFile::new().expect("failed to create temp file");
        self.write_wave_f32(temp_file.path()).expect("failed to write wave geotiff");
        temp_file
    }

    /// Create a temporary GeoTIFF with custom f32 function and return `(NamedTempFile, PathBuf)`.
    pub fn create_f32_tempfile<F: Fn(u32, u32) -> f32>(&self, val_fn: F) -> (NamedTempFile, PathBuf) {
        let temp_file = NamedTempFile::new().expect("failed to create temp file");
        let path = temp_file.path().to_path_buf();
        self.write_f32_fn(&path, val_fn).expect("failed to write f32 geotiff");
        (temp_file, path)
    }
}

/// Create a temporary GeoTIFF with synthetic continuous wave pattern (SF Bay, 0.0005 deg/pixel)
pub fn create_wave_test_geotiff(width: usize, height: usize) -> NamedTempFile {
    TestGeoTiffBuilder::new(width as u32, height as u32)
        .origin(-122.45, 37.80)
        .pixel_size(0.0005)
        .create_wave_tempfile()
}

/// Create a temporary GeoTIFF with synthetic continuous wave pattern with custom pixel size (e.g. 0.001 deg)
pub fn create_wave_test_geotiff_with_scale(width: usize, height: usize, pixel_size: f64) -> NamedTempFile {
    TestGeoTiffBuilder::new(width as u32, height as u32)
        .origin(-122.45, 37.80)
        .pixel_size(pixel_size)
        .create_wave_tempfile()
}

/// Create a constant-value Gray8 GeoTIFF at a specified path.
pub fn create_constant_gray8_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: u8,
) {
    TestGeoTiffBuilder::new(width, height)
        .origin(origin_lon, origin_lat)
        .pixel_size(pixel_size)
        .write_gray8_constant(path, fill_val)
        .expect("failed to create constant gray8 geotiff");
}

/// Create a constant-value Gray32Float GeoTIFF at a specified path.
pub fn create_constant_f32_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: f32,
) {
    TestGeoTiffBuilder::new(width, height)
        .origin(origin_lon, origin_lat)
        .pixel_size(pixel_size)
        .write_f32_constant(path, fill_val)
        .expect("failed to create constant f32 geotiff");
}

/// Create a custom function-evaluated Gray32Float GeoTIFF returning `(NamedTempFile, PathBuf)`.
pub fn create_fn_f32_geotiff<F: Fn(u32, u32) -> f32>(
    width: u32,
    height: u32,
    val_fn: F,
) -> (NamedTempFile, PathBuf) {
    TestGeoTiffBuilder::new(width, height)
        .origin(-122.4, 37.8)
        .pixel_size(0.001)
        .create_f32_tempfile(val_fn)
}

/// Creates a temporary GeoTIFF file for tests.
/// * `path` – Destination path for the file.
/// * `width`, `height` – Image dimensions.
/// * `compression` – Desired TIFF compression.
/// The image is a single‑band 8‑bit grayscale with deterministic pixel values.
pub fn create_temp_geotiff<P: AsRef<Path>>(
    path: P,
    width: u32,
    height: u32,
    _compression: CompressionMethod,
) -> std::io::Result<()> {
    let file = File::create(&path)?;
    let mut encoder = TiffEncoder::new(file).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let mut image = encoder
        .new_image::<colortype::Gray8>(width, height)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    // Set TIFF tags (SF Bay area: -122.45, 37.80 with 0.0005 deg/pixel)
    image
        .encoder()
        .write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -122.45, 37.80, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[0.0005_f64, 0.0005, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &WGS84_GEOKEYS[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let mut data = Vec::with_capacity((width * height) as usize);
    for i in 0..(width * height) {
        data.push((i % 256) as u8);
    }
    image.write_data(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}

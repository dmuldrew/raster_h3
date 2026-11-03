//! Shared test fixture builders and synthetic GeoTIFF generators for test suites.
//!
//! Provides [`TestGeoTiffBuilder`], the [`TestTiffSample`] trait, wave and gradient
//! pattern generators, and convenience functions for constructing temporary GeoTIFFs.

#![allow(dead_code)]

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::{CompressionMethod, Tag};

/// Standard GeoKeys for EPSG:4326
const WGS84_GEOKEYS: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];

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
    pub georeferenced: bool,
    pub with_geokeys: bool,
    pub gdal_nodata: Option<String>,
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
            georeferenced: true,
            with_geokeys: true,
            gdal_nodata: None,
        }
    }
}

/// Trait implemented by supported TIFF sample types for testing
pub trait TestTiffSample: Copy + 'static {
    type ColorType: colortype::ColorType;
    #[allow(clippy::too_many_arguments)]
    fn write_image<W: std::io::Write + std::io::Seek>(
        encoder: &mut TiffEncoder<W>,
        width: u32,
        height: u32,
        tiepoint: Option<&[f64]>,
        pixel_scale: Option<&[f64]>,
        geokeys: Option<&[u16]>,
        gdal_nodata: Option<&str>,
        data: &[Self],
    ) -> std::io::Result<()>;
}

macro_rules! impl_test_tiff_sample {
    ($t:ty, $colortype:ident) => {
        impl TestTiffSample for $t {
            type ColorType = colortype::$colortype;
            fn write_image<W: std::io::Write + std::io::Seek>(
                encoder: &mut TiffEncoder<W>,
                width: u32,
                height: u32,
                tiepoint: Option<&[f64]>,
                pixel_scale: Option<&[f64]>,
                geokeys: Option<&[u16]>,
                gdal_nodata: Option<&str>,
                data: &[Self],
            ) -> std::io::Result<()> {
                let mut image = encoder
                    .new_image::<colortype::$colortype>(width, height)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                if let Some(tp) = tiepoint {
                    image
                        .encoder()
                        .write_tag(Tag::ModelTiepointTag, tp)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                if let Some(ps) = pixel_scale {
                    image
                        .encoder()
                        .write_tag(Tag::ModelPixelScaleTag, ps)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                if let Some(gk) = geokeys {
                    image
                        .encoder()
                        .write_tag(Tag::Unknown(34735), gk)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                if let Some(nd) = gdal_nodata {
                    image
                        .encoder()
                        .write_tag(Tag::GdalNodata, nd)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                image
                    .write_data(data)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                Ok(())
            }
        }
    };
}

impl_test_tiff_sample!(u8, Gray8);
impl_test_tiff_sample!(u16, Gray16);
impl_test_tiff_sample!(u32, Gray32);
impl_test_tiff_sample!(u64, Gray64);
impl_test_tiff_sample!(f32, Gray32Float);
impl_test_tiff_sample!(f64, Gray64Float);

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

    pub fn georeferenced(mut self, enabled: bool) -> Self {
        self.georeferenced = enabled;
        self
    }

    pub fn with_geokeys(mut self, enabled: bool) -> Self {
        self.with_geokeys = enabled;
        self
    }

    pub fn gdal_nodata(mut self, nodata_str: impl Into<String>) -> Self {
        self.gdal_nodata = Some(nodata_str.into());
        self
    }

    /// Write pixels of type `T` evaluated from a closure `(col, row) -> T` to the target path.
    pub fn write_fn<T: TestTiffSample, P: AsRef<Path>, F: Fn(u32, u32) -> T>(
        &self,
        path: P,
        val_fn: F,
    ) -> std::io::Result<()> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);
        let mut encoder = TiffEncoder::new(writer).map_err(std::io::Error::other)?;

        let tiepoint = [0.0_f64, 0.0, 0.0, self.origin_lon, self.origin_lat, 0.0];
        let pixel_scale = [self.pixel_size_x, self.pixel_size_y, 0.0];
        let geokeys: [u16; 12] = if self.epsg == 4326 {
            WGS84_GEOKEYS
        } else {
            [
                1, 1, 0, 2, 1024, 0, 1, 1, // ModelTypeProjected
                3072, 0, 1, self.epsg,
            ]
        };

        let mut data = Vec::with_capacity((self.width * self.height) as usize);
        for row in 0..self.height {
            for col in 0..self.width {
                data.push(val_fn(col, row));
            }
        }

        let tp = if self.georeferenced {
            Some(&tiepoint[..])
        } else {
            None
        };
        let ps = if self.georeferenced {
            Some(&pixel_scale[..])
        } else {
            None
        };
        let gk = if self.georeferenced && self.with_geokeys {
            Some(&geokeys[..])
        } else {
            None
        };

        T::write_image(
            &mut encoder,
            self.width,
            self.height,
            tp,
            ps,
            gk,
            self.gdal_nodata.as_deref(),
            &data,
        )
    }

    /// Write pixels of type `T` with a constant fill value to the target path.
    pub fn write_constant<T: TestTiffSample, P: AsRef<Path>>(
        &self,
        path: P,
        fill_val: T,
    ) -> std::io::Result<()> {
        self.write_fn(path, |_c, _r| fill_val)
    }

    /// Write Gray8 pixels evaluated from a closure `(col, row) -> u8` to the target path.
    pub fn write_gray8_fn<P: AsRef<Path>, F: Fn(u32, u32) -> u8>(
        &self,
        path: P,
        val_fn: F,
    ) -> std::io::Result<()> {
        self.write_fn(path, val_fn)
    }

    /// Write Gray8 pixels with a constant fill value to the target path.
    pub fn write_gray8_constant<P: AsRef<Path>>(
        &self,
        path: P,
        fill_val: u8,
    ) -> std::io::Result<()> {
        self.write_constant(path, fill_val)
    }

    /// Write Gray32Float pixels with a constant fill value to the target path.
    pub fn write_f32_constant<P: AsRef<Path>>(
        &self,
        path: P,
        fill_val: f32,
    ) -> std::io::Result<()> {
        self.write_constant(path, fill_val)
    }

    /// Write Gray32Float pixels evaluated from a closure `(col, row) -> f32` to the target path.
    pub fn write_f32_fn<P: AsRef<Path>, F: Fn(u32, u32) -> f32>(
        &self,
        path: P,
        val_fn: F,
    ) -> std::io::Result<()> {
        self.write_fn(path, val_fn)
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
        self.write_wave_f32(temp_file.path())
            .expect("failed to write wave geotiff");
        temp_file
    }

    /// Create a temporary GeoTIFF with custom f32 function and return `(NamedTempFile, PathBuf)`.
    pub fn create_f32_tempfile<F: Fn(u32, u32) -> f32>(
        &self,
        val_fn: F,
    ) -> (NamedTempFile, PathBuf) {
        let temp_file = NamedTempFile::new().expect("failed to create temp file");
        let path = temp_file.path().to_path_buf();
        self.write_f32_fn(&path, val_fn)
            .expect("failed to write f32 geotiff");
        (temp_file, path)
    }

    /// Create a temporary GeoTIFF of type `T` evaluated from `val_fn` and return `(NamedTempFile, PathBuf)`.
    pub fn create_tempfile<T: TestTiffSample, F: Fn(u32, u32) -> T>(
        &self,
        val_fn: F,
    ) -> (NamedTempFile, PathBuf) {
        let temp_file = NamedTempFile::new().expect("failed to create temp file");
        let path = temp_file.path().to_path_buf();
        self.write_fn(&path, val_fn)
            .expect("failed to write test geotiff");
        (temp_file, path)
    }

    /// Create a temporary GeoTIFF of type `T` with a constant fill value and return `(NamedTempFile, PathBuf)`.
    pub fn create_constant_tempfile<T: TestTiffSample>(&self, val: T) -> (NamedTempFile, PathBuf) {
        self.create_tempfile(|_c, _r| val)
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
pub fn create_wave_test_geotiff_with_scale(
    width: usize,
    height: usize,
    pixel_size: f64,
) -> NamedTempFile {
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

/// Create a constant-value Gray16 GeoTIFF at a specified path.
pub fn create_constant_u16_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: u16,
) {
    TestGeoTiffBuilder::new(width, height)
        .origin(origin_lon, origin_lat)
        .pixel_size(pixel_size)
        .write_constant(path, fill_val)
        .expect("failed to create constant u16 geotiff");
}

/// Create a constant-value Gray64Float GeoTIFF at a specified path.
pub fn create_constant_f64_geotiff(
    path: &Path,
    width: u32,
    height: u32,
    origin_lon: f64,
    origin_lat: f64,
    pixel_size: f64,
    fill_val: f64,
) {
    TestGeoTiffBuilder::new(width, height)
        .origin(origin_lon, origin_lat)
        .pixel_size(pixel_size)
        .write_constant(path, fill_val)
        .expect("failed to create constant f64 geotiff");
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
///
/// The image is a single‑band 8‑bit grayscale with deterministic pixel values.
pub fn create_temp_geotiff<P: AsRef<Path>>(
    path: P,
    width: u32,
    height: u32,
    _compression: CompressionMethod,
) -> std::io::Result<()> {
    TestGeoTiffBuilder::new(width, height)
        .write_gray8_fn(path, |col, row| ((row * width + col) % 256) as u8)
}

/// Workspace-root `data/` path for optional external datasets (tests run with the crate dir as cwd).
pub fn data_path(rel: &str) -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !dir.join("Cargo.lock").exists() {
        assert!(dir.pop(), "workspace root (Cargo.lock) not found");
    }
    dir.join("data").join(rel)
}

/// Deflate-compressed GeoTIFF whose metadata decodes but whose third strip payload is corrupt.
pub fn corrupt_strip_geotiff() -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    {
        let mut encoder = TiffEncoder::new(File::create(file.path()).unwrap())
            .unwrap()
            .with_compression(tiff::encoder::Compression::Deflate(Default::default()));
        let mut image = encoder.new_image::<colortype::Gray32Float>(32, 32).unwrap();
        image.rows_per_strip(8).unwrap();
        image
            .encoder()
            .write_tag(Tag::ModelPixelScaleTag, &[0.001f64, 0.001, 0.0][..])
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::ModelTiepointTag,
                &[0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
        image.write_data(&vec![42.0; 32 * 32]).unwrap();
    }
    let (offset, length) = {
        let reader =
            raster_h3_core::raster::geotiff::GeoTiffStreamReader::open(file.path()).unwrap();
        let info = reader.chunk_info.as_ref().unwrap();
        (info.chunk_offsets[2], info.chunk_bytes[2])
    };
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(file.path())
        .unwrap();
    std::io::Seek::seek(&mut writer, std::io::SeekFrom::Start(offset)).unwrap();
    std::io::Write::write_all(&mut writer, &vec![0xff; length as usize]).unwrap();
    // Metadata remains readable; only a later strip has a broken compressed payload.
    let reader = raster_h3_core::raster::geotiff::GeoTiffStreamReader::open(file.path()).unwrap();
    let mut decoder = reader.open_decoder().unwrap();
    assert!(decoder.read_chunk_with_payload(0, None, None).is_ok());
    assert!(decoder.read_chunk_with_payload(2, None, None).is_err());
    file
}

// -----------------------------------------------------------------------------------------
// Tiled and multi-strip GeoTIFFs (the `tiff` encoder writes one strip per image)
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Crs {
    /// EPSG:4326 with the origin (upper-left corner) in degrees.
    Geographic { lon: f64, lat: f64, pixel_deg: f64 },
    /// EPSG:32610 (UTM 10N) with the origin in metres.
    Utm10n {
        easting: f64,
        northing: f64,
        pixel_m: f64,
    },
}

/// Pixel encoding of a fixture.
pub trait Sample: Copy {
    const BITS: u16;
    /// TIFF SampleFormat: 1 unsigned integer, 3 IEEE float.
    const FORMAT: u16;
    fn bytes(self) -> Vec<u8>;
}
impl Sample for f32 {
    const BITS: u16 = 32;
    const FORMAT: u16 = 3;
    fn bytes(self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }
}
impl Sample for u8 {
    const BITS: u16 = 8;
    const FORMAT: u16 = 1;
    fn bytes(self) -> Vec<u8> {
        vec![self]
    }
}

/// Chunk layout of a fixture.
#[derive(Clone, Copy)]
pub enum Layout {
    /// Tiles of `(width, height)` pixels, padded at the right and bottom edges.
    Tiles(u32, u32),
    /// Strips of `rows` full-width rows; the last strip holds the remaining rows.
    Strips(u32),
}

/// Write an uncompressed, tiled, single-band GeoTIFF with `value(col, row)` pixels.
pub fn write_tiled<T: Sample>(
    path: &Path,
    size: (u32, u32),
    (tile_w, tile_h): (u32, u32),
    crs: Crs,
    nodata: &str,
    value: impl Fn(u32, u32) -> T,
) {
    write_geotiff(
        path,
        size,
        Layout::Tiles(tile_w, tile_h),
        crs,
        nodata,
        value,
    )
}

/// Write an uncompressed single-band GeoTIFF with `value(col, row)` pixels.
pub fn write_geotiff<T: Sample>(
    path: &Path,
    (width, height): (u32, u32),
    layout: Layout,
    crs: Crs,
    nodata: &str,
    value: impl Fn(u32, u32) -> T,
) {
    let (chunk_w, chunk_h, tiled) = match layout {
        Layout::Tiles(w, h) => (w, h, true),
        Layout::Strips(rows) => (width, rows, false),
    };
    let across = width.div_ceil(chunk_w);
    let down = height.div_ceil(chunk_h);
    let mut buf = b"II\x2a\x00\0\0\0\0".to_vec();

    let mut offsets = Vec::new();
    let mut counts = Vec::new();
    for t in 0..across * down {
        let (x0, y0) = ((t % across) * chunk_w, (t / across) * chunk_h);
        let rows = if tiled {
            chunk_h
        } else {
            chunk_h.min(height - y0)
        };
        offsets.push(buf.len() as u32);
        let start = buf.len();
        for r in 0..rows {
            for c in 0..chunk_w {
                let (x, y) = (x0 + c, y0 + r);
                let v = if x < width && y < height {
                    value(x, y)
                } else {
                    value(0, 0)
                };
                buf.extend(v.bytes());
            }
        }
        counts.push((buf.len() - start) as u32);
    }

    let (scale, tiepoint, geokeys): ([f64; 3], [f64; 6], [u16; 12]) = match crs {
        Crs::Geographic {
            lon,
            lat,
            pixel_deg,
        } => (
            [pixel_deg, pixel_deg, 0.0],
            [0.0, 0.0, 0.0, lon, lat, 0.0],
            [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326],
        ),
        Crs::Utm10n {
            easting,
            northing,
            pixel_m,
        } => (
            [pixel_m, pixel_m, 0.0],
            [0.0, 0.0, 0.0, easting, northing, 0.0],
            [1, 1, 0, 2, 1024, 0, 1, 1, 3072, 0, 1, 32610],
        ),
    };
    let mut blob = |bytes: Vec<u8>| -> u32 {
        if buf.len() % 2 == 1 {
            buf.push(0);
        }
        let at = buf.len() as u32;
        buf.extend(bytes);
        at
    };
    let le32 = |v: &[u32]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<_>>();
    let offsets_at = blob(le32(&offsets));
    let counts_at = blob(le32(&counts));
    let scale_at = blob(scale.iter().flat_map(|x| x.to_le_bytes()).collect());
    let tie_at = blob(tiepoint.iter().flat_map(|x| x.to_le_bytes()).collect());
    let keys_at = blob(geokeys.iter().flat_map(|x| x.to_le_bytes()).collect());
    let mut nodata_ascii = nodata.as_bytes().to_vec();
    nodata_ascii.push(0);
    let nodata_len = nodata_ascii.len() as u32;
    let nodata_at = blob(nodata_ascii);

    // (tag, type, count, value-or-offset); type 3 SHORT, 4 LONG, 2 ASCII, 12 DOUBLE.
    let n = offsets.len() as u32;
    let many = |at: u32, inline: u32| if n == 1 { inline } else { at };
    let (offsets_tag, counts_tag) = if tiled { (324, 325) } else { (273, 279) };
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 4, 1, width),
        (257, 4, 1, height),
        (258, 3, 1, u32::from(T::BITS)),
        (259, 3, 1, 1),
        (262, 3, 1, 1),
        (277, 3, 1, 1),
        (284, 3, 1, 1),
        (offsets_tag, 4, n, many(offsets_at, offsets[0])),
        (counts_tag, 4, n, many(counts_at, counts[0])),
        (339, 3, 1, u32::from(T::FORMAT)),
        (33550, 12, 3, scale_at),
        (33922, 12, 6, tie_at),
        (34735, 3, 12, keys_at),
        (42113, 2, nodata_len, nodata_at),
    ];
    if tiled {
        entries.extend([(322, 4, 1, chunk_w), (323, 4, 1, chunk_h)]);
    } else {
        entries.push((278, 4, 1, chunk_h));
    }
    entries.sort_by_key(|e| e.0);
    if buf.len() % 2 == 1 {
        buf.push(0);
    }
    let ifd = buf.len() as u32;
    buf[4..8].copy_from_slice(&ifd.to_le_bytes());
    buf.extend((entries.len() as u16).to_le_bytes());
    for (tag, ty, count, value) in entries {
        buf.extend(tag.to_le_bytes());
        buf.extend(ty.to_le_bytes());
        buf.extend(count.to_le_bytes());
        if ty == 3 && count == 1 {
            buf.extend((value as u16).to_le_bytes());
            buf.extend([0, 0]);
        } else {
            buf.extend(value.to_le_bytes());
        }
    }
    buf.extend([0, 0, 0, 0]);
    std::fs::write(path, buf).unwrap();
}

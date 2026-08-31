use std::fs::File; // used for file creation
// unused import removed
use std::path::Path;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::{Tag, CompressionMethod};

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
    let mut data = Vec::with_capacity((width * height) as usize);
    // No additional assertions needed
    for i in 0..(width * height) {
        data.push((i % 256) as u8);
    }
    image.write_data(&data).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    Ok(())
}

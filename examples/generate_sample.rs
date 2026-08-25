use std::fs::File;
use std::io::BufWriter;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "sample_sf.tif".to_string());

    println!("Generating sample GeoTIFF: {}", output_path);

    let width = 200;
    let height = 200;

    // Generate synthetic elevation surface with peaks
    let mut data = Vec::with_capacity(width * height);
    for row in 0..height {
        for col in 0..width {
            let dx = (col as f32) - 100.0;
            let dy = (row as f32) - 100.0;
            let dist = (dx * dx + dy * dy).sqrt();
            let elevation = (100.0 - dist).max(10.0) * 5.0 + 150.0;
            data.push(elevation);
        }
    }

    let file = File::create(&output_path)?;
    let writer = BufWriter::new(file);
    let mut encoder = TiffEncoder::new(writer)?;

    let mut image = encoder.new_image::<Gray32Float>(width as u32, height as u32)?;

    // Tiepoint: pixel (0,0) -> (-122.50, 37.85) San Francisco Bay Area (WGS84)
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.50, 37.85, 0.0][..])?;

    // Pixel Scale: 0.001 deg/pixel (~100m)
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.001f64, 0.001, 0.0][..])?;

    // GeoKeyDirectoryTag for WGS84 (EPSG:4326)
    // KeyDirectoryVersion=1, KeyRevision=1, MinorRevision=0, NumberOfKeys=2
    // Key 1: GTModelTypeGeoKey (1024) = 2 (Geographic 2D)
    // Key 2: GeographicTypeGeoKey (2048) = 4326 (WGS84)
    let geokeys: [u16; 12] = [
        1, 1, 0, 2,
        1024, 0, 1, 2,
        2048, 0, 1, 4326,
    ];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])?;

    // Write image data
    image.write_data(&data)?;

    println!("Successfully generated {}", output_path);
    Ok(())
}

use std::env;
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| "data/LF2024_FBFM40_HI.tif".to_string());
    let reader = GeoTiffStreamReader::open(&path).unwrap();
    println!("File: {}", path);
    println!("Width: {}, Height: {}", reader.metadata.width, reader.metadata.height);
    println!("Total chunks: {}, Chunk width: {}, Chunk height: {}", 
        reader.chunk_layout.total_chunks,
        reader.chunk_layout.chunk_width,
        reader.chunk_layout.chunk_height);
    println!("GeoTransform: {:?}", reader.metadata.geotransform);
    println!("EPSG: {:?}, Proj: {:?}", reader.metadata.epsg, reader.metadata.proj_string);
    let ll = h3o::LatLng::new(21.3069, -157.8583).unwrap();
    println!("LatLng degrees: lat={}, lng={}", ll.lat(), ll.lng());
    println!("LatLng radians: lat_rad={}, lng_rad={}", ll.lat_radians(), ll.lng_radians());
    let cell = ll.to_cell(h3o::Resolution::Eight);
    let center: h3o::LatLng = cell.into();
    println!("Cell center: lat={}, lng={}", center.lat(), center.lng());
}

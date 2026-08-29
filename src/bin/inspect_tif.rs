use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn main() {
    let reader = GeoTiffStreamReader::open("data/LF2024_FBFM40_HI.tif").unwrap();
    println!("Width: {}, Height: {}", reader.metadata.width, reader.metadata.height);
    println!("Total chunks: {}, Chunk width: {}, Chunk height: {}", 
        reader.chunk_layout.total_chunks,
        reader.chunk_layout.chunk_width,
        reader.chunk_layout.chunk_height);
    println!("GeoTransform: {:?}", reader.metadata.geotransform);
    println!("EPSG: {:?}, Proj: {:?}", reader.metadata.epsg, reader.metadata.proj_string);
    println!("NoData: {:?}", reader.metadata.nodata);
}

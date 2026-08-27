use h3o::{CellIndex, LatLng};
use raster_h3::pmtiles::tiler::lon_lat_to_tile_xy;

fn main() {
    let cell_u64 = 0x83464efffffffffu64;
    let cell = CellIndex::try_from(cell_u64).unwrap();
    let center: LatLng = cell.into();
    println!("Cell 0x{:x}: Lat={}, Lon={}", cell_u64, center.lat(), center.lng());

    let (tx, ty) = lon_lat_to_tile_xy(center.lng(), center.lat(), 5);
    println!("Computed tile at Z5: ({}, {})", tx, ty);

    let (tx7, ty7) = lon_lat_to_tile_xy(center.lng(), center.lat(), 7);
    println!("Computed tile at Z7: ({}, {})", tx7, ty7);
}

use raster_h3::raster::geotiff::GeoTiffStreamReader;
use raster_h3::aggregator::h3_map::aggregate_raster_stream;
use raster_h3::aggregator::horizon_streamer::AggregationConfig;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let reader = GeoTiffStreamReader::open("data/CFL_HI.tif")?;
    let resolutions = [3, 4, 5, 6, 7, 8, 9];

    for &res in &resolutions {
        let mut cfg = AggregationConfig::default();
        cfg.resolution = res;
        let map = aggregate_raster_stream(&reader, &cfg)?;
        println!("Resolution {}: {} total hexagons, {} non-zero mean",
            res,
            map.len(),
            map.values().filter(|a| a.mean() > 0.0).count()
        );
    }
    Ok(())
}

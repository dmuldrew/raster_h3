use super::super::config::MultiResolutionConfig;
use crate::aggregator::multi_horizon::continuous_streamer::MultiScanHorizonStreamer;
use crate::raster::geotiff::GeoTiffStreamReader;
use crate::raster::prefetch::MosaicPrefetchItem;
use crate::raster::RasterChunk;
use tiff::decoder::DecodingResult;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

/// Minimal georeferenced Gray8 GeoTIFF — enough for `GeoTiffStreamReader::open`.
fn write_gray_geotiff(path: &std::path::Path) {
    const GEOKEYS: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = TiffEncoder::new(std::io::BufWriter::new(file)).unwrap();
    let mut image = encoder.new_image::<colortype::Gray8>(8, 8).unwrap();
    image
        .encoder()
        .write_tag(
            Tag::ModelTiepointTag,
            &[0.0, 0.0, 0.0, -122.0, 38.0, 0.0][..],
        )
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::ModelPixelScaleTag, &[0.001, 0.001, 0.0][..])
        .unwrap();
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &GEOKEYS[..])
        .unwrap();
    image.write_data(&[1u8; 64]).unwrap();
}

/// A panic inside a wave used to leave `worker_maps` empty; every following
/// wave then zipped jobs against zero maps and silently dropped all
/// samples. `process_batch` now restores its state before resuming the
/// unwind, so a caller that catches the panic finds the maps intact.
#[test]
fn worker_panic_preserves_worker_maps() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_gray_geotiff(tmp.path());
    let reader = GeoTiffStreamReader::open(tmp.path()).unwrap();
    let config = MultiResolutionConfig::single(5);
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    let maps_before = streamer.worker_maps.len();
    assert!(maps_before > 0);

    // Without horizon maps, merging the window's map set indexes a
    // missing resolution and panics after the sets are taken from the pool.
    streamer.resolution_shards.clear();
    let mut items: Vec<MosaicPrefetchItem> = vec![Ok((
        0,
        0,
        RasterChunk {
            col_offset: 0,
            row_offset: 0,
            width: 4,
            height: 4,
        },
        DecodingResult::U8(vec![1; 16]),
        false,
    ))];
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = streamer.process_batch(&mut items);
    }));
    assert!(panic.is_err());
    assert_eq!(streamer.worker_maps.len(), maps_before);
}

/// A panic while advancing loses the batch in flight, so the stream
/// latches a failure: later reads return an error instead of continuing
/// past the missing samples.
#[test]
fn panic_while_advancing_latches_failure() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_gray_geotiff(tmp.path());
    let reader = GeoTiffStreamReader::open(tmp.path()).unwrap();
    let config = MultiResolutionConfig::single(5);
    let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
    // Without horizon maps, the first merge indexes a missing resolution
    // and panics on the calling thread.
    streamer.resolution_shards.clear();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = streamer.advance_until_completed(1);
    }));
    assert!(panic.is_err(), "the panic must still reach the caller");
    let err = streamer.advance_until_completed(1).unwrap_err();
    assert!(err.to_string().contains("panicked"), "{err}");
    assert!(streamer.fetch_next_batch(16).is_err());
}

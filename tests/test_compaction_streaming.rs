//! Hierarchical compaction streams with early eviction: output matches EOF
//! grouping of the uncompacted cells, and no record is emitted north of a
//! previously published horizon.

use std::collections::HashMap;
use tiff::encoder::{colortype, TiffEncoder};
use tiff::tags::Tag;

use h3o::CellIndex;
use raster_h3::aggregator::horizon_streamer::compute_cell_south_lat;
use raster_h3::aggregator::multi_horizon::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
};
use raster_h3::pmtiles::tiler::H3PmtilesTiler;
use raster_h3::raster::geotiff::GeoTiffStreamReader;

const NODATA: f32 = -9999.0;

/// North-up WGS84 strips with a nodata band so some sibling groups are incomplete.
fn fixture() -> tempfile::NamedTempFile {
    let (width, height) = (300u32, 1200u32);
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut encoder = TiffEncoder::new(std::fs::File::create(file.path()).unwrap()).unwrap();
    let mut image = encoder
        .new_image::<colortype::Gray32Float>(width, height)
        .unwrap();
    image.rows_per_strip(16).unwrap();
    let enc = image.encoder();
    enc.write_tag(Tag::ModelPixelScaleTag, &[0.0005, 0.0005, 0.0][..])
        .unwrap();
    enc.write_tag(
        Tag::ModelTiepointTag,
        &[0.0, 0.0, 0.0, -120.0, 45.0, 0.0][..],
    )
    .unwrap();
    let geokeys: [u16; 12] = [1, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326];
    enc.write_tag(Tag::GeoKeyDirectoryTag, &geokeys[..])
        .unwrap();
    enc.write_tag(Tag::Unknown(42113), "-9999").unwrap();
    let data: Vec<f32> = (0..height)
        .flat_map(|r| {
            (0..width).map(move |c| {
                if (c + r / 3) % 97 < 9 {
                    NODATA
                } else {
                    1.0 + (c % 5) as f32
                }
            })
        })
        .collect();
    image.write_data(&data).unwrap();
    file
}

fn config(resolutions: &[u8], compact: bool) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::new(resolutions.to_vec());
    config.compact_h3_children = compact;
    config
}

/// EOF grouping of uncompacted output: complete sibling groups become parents.
fn expected_compaction(cells: &HashMap<(u8, u64), f64>) -> HashMap<(u8, u64), f64> {
    let mut groups: HashMap<u64, Vec<(u8, u64, f64)>> = HashMap::new();
    for (&(res, cell), &count) in cells {
        let index = CellIndex::try_from(cell).unwrap();
        let parent = index.parent(index.resolution().pred().unwrap()).unwrap();
        groups
            .entry(parent.into())
            .or_default()
            .push((res, cell, count));
    }
    let mut out = HashMap::new();
    for (parent, children) in groups {
        let index = CellIndex::try_from(parent).unwrap();
        let siblings = if index.is_pentagon() { 6 } else { 7 };
        if children.len() == siblings {
            let total = children.iter().map(|c| c.2).sum();
            out.insert((u8::from(index.resolution()), parent), total);
        } else {
            for (res, cell, count) in children {
                out.insert((res, cell), count);
            }
        }
    }
    out
}

fn stream(
    path: &std::path::Path,
    config: &MultiResolutionConfig,
) -> (HashMap<(u8, u64), f64>, bool) {
    let reader = GeoTiffStreamReader::open(path).unwrap();
    let mut streamer = MultiScanHorizonStreamer::new(reader, config).unwrap();
    let total_chunks = streamer.mosaic().chunk_refs.len();
    let mut out = HashMap::new();
    let mut strictest_horizon = f64::INFINITY;
    let mut emitted_before_eof = false;
    loop {
        // Single-row batches let the controller publish a horizon between
        // nearly every record, so held sibling groups are actually tested.
        let batch = streamer.fetch_next_batch(1).unwrap();
        if batch.is_empty() {
            break;
        }
        if streamer.processed_chunk_count() < total_chunks {
            emitted_before_eof = true;
        }
        for rec in batch {
            let south = compute_cell_south_lat(rec.h3_index);
            assert!(
                south <= strictest_horizon,
                "record {:x} (south {south}) emitted north of published horizon {strictest_horizon}",
                rec.h3_index
            );
            let previous = out.insert((rec.resolution, rec.h3_index), rec.accumulator.count);
            assert!(previous.is_none(), "duplicate record {:x}", rec.h3_index);
        }
        strictest_horizon = strictest_horizon.min(streamer.current_lat_horizon());
    }
    (out, emitted_before_eof)
}

#[test]
fn compaction_streams_early_and_matches_eof_grouping() {
    let file = fixture();
    for resolutions in [vec![8u8], vec![6, 8]] {
        let (plain, _) = stream(file.path(), &config(&resolutions, false));
        let (compacted, early) = stream(file.path(), &config(&resolutions, true));
        assert!(
            early,
            "{resolutions:?}: compaction must not defer all output to EOF"
        );

        let expected = expected_compaction(&plain);
        let parents = expected
            .keys()
            .filter(|(res, _)| !resolutions.contains(res))
            .count();
        assert!(
            parents > 0,
            "{resolutions:?}: fixture must form complete groups"
        );
        assert!(
            expected.len() > parents,
            "{resolutions:?}: fixture must leave incomplete groups"
        );
        assert_eq!(compacted.len(), expected.len(), "{resolutions:?}");
        for (key, count) in &expected {
            let actual = compacted.get(key).unwrap_or_else(|| {
                panic!(
                    "{resolutions:?}: missing record res {} cell {:x}",
                    key.0, key.1
                )
            });
            assert!((actual - count).abs() < 1e-9, "{resolutions:?}: {key:?}");
        }
    }
}

#[test]
fn pmtiles_rejects_compacting_streamers_before_writing() {
    let file = fixture();
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.pmtiles");
    let compacting = config(&[8], true);
    let open = || GeoTiffStreamReader::open(file.path()).unwrap();

    let continuous = MultiScanHorizonStreamer::new(open(), &compacting).unwrap();
    let err = H3PmtilesTiler::generate_from_continuous_streamer(continuous, &out).unwrap_err();
    assert!(err.to_string().contains("compaction"), "{err}");
    let categorical = MultiCategoricalHorizonStreamer::new(open(), &compacting).unwrap();
    assert!(H3PmtilesTiler::generate_from_categorical_streamer(categorical, &out).is_err());
    assert!(!out.exists(), "a rejected stream must not create output");

    // Without compaction the same input tiles normally.
    let plain = MultiScanHorizonStreamer::new(open(), &config(&[8], false)).unwrap();
    assert!(H3PmtilesTiler::generate_from_continuous_streamer(plain, &out).unwrap() > 0);
}

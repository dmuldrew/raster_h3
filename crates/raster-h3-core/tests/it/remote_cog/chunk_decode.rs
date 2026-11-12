//! The chunk byte pathway shared by local and remote sources, including the generic decoder.

use std::fs::File;
use std::io::Read;
use std::sync::atomic::Ordering;

use raster_h3_core::raster::geotiff::GeoTiffStreamReader;
use tiff::decoder::DecodingResult;

use super::MockHttpServer;
use crate::helpers;

#[test]
fn test_unified_chunk_byte_pathway() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }

    use raster_h3_core::raster::geotiff::ChunkPayload;

    let local_path = helpers::data_path("CFL_HI.tif");
    if !local_path.exists() {
        return;
    }

    let mut file = File::open(&local_path).unwrap();
    let mut file_bytes = Vec::new();
    file.read_to_end(&mut file_bytes).unwrap();

    let server = MockHttpServer::start(file_bytes);
    let remote_url = format!("{}/CFL_HI.tif", server.url_base);

    let local_reader = GeoTiffStreamReader::open(&local_path).unwrap();
    let remote_reader = GeoTiffStreamReader::open(&remote_url).unwrap();

    // 1. Verify get_chunk_payload across local (zero-copy borrowed) and remote (range-fetched owned)
    for chunk_idx in [0, 5, 10, 15] {
        let local_payload = local_reader.get_chunk_payload(chunk_idx).unwrap();
        let remote_payload = remote_reader.get_chunk_payload(chunk_idx).unwrap();

        // Local must be zero-copy borrowed slice from mmap
        assert!(matches!(local_payload, ChunkPayload::Borrowed(_)));
        // Remote must be owned bytes fetched from remote range
        assert!(matches!(remote_payload, ChunkPayload::Owned(_)));

        // Both must be 100% byte-for-byte identical
        assert_eq!(local_payload.as_ref(), remote_payload.as_ref());
    }

    // 2. Verify standalone read_chunk and read_chunk_into on remote decoders use SIMD/LZW directly
    let mut local_decoder = local_reader.open_decoder().unwrap();
    let mut remote_decoder = remote_reader.open_decoder().unwrap();

    fn assert_decoding_result_eq(a: &DecodingResult, b: &DecodingResult) {
        match (a, b) {
            (DecodingResult::U8(v1), DecodingResult::U8(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U16(v1), DecodingResult::U16(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U32(v1), DecodingResult::U32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::U64(v1), DecodingResult::U64(v2)) => assert_eq!(v1, v2),
            (DecodingResult::F32(v1), DecodingResult::F32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::F64(v1), DecodingResult::F64(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I8(v1), DecodingResult::I8(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I16(v1), DecodingResult::I16(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I32(v1), DecodingResult::I32(v2)) => assert_eq!(v1, v2),
            (DecodingResult::I64(v1), DecodingResult::I64(v2)) => assert_eq!(v1, v2),
            _ => panic!("Mismatched DecodingResult types"),
        }
    }

    for chunk_idx in [0, 1, 2, 7, 12] {
        // Test read_chunk
        let (loc_bounds, loc_data) = local_decoder.read_chunk(chunk_idx).unwrap();
        let (rem_bounds, rem_data) = remote_decoder.read_chunk(chunk_idx).unwrap();
        assert_eq!(loc_bounds, rem_bounds);
        assert_decoding_result_eq(&loc_data, &rem_data);

        // Test read_chunk_into buffer recycling
        let (loc_b2, loc_d2) = local_decoder.read_chunk_into(chunk_idx, loc_data).unwrap();
        let (rem_b2, rem_d2) = remote_decoder.read_chunk_into(chunk_idx, rem_data).unwrap();
        assert_eq!(loc_b2, rem_b2);
        assert_decoding_result_eq(&loc_d2, &rem_d2);

        // Test read_chunk_with_payload with externally provided bytes
        let raw_payload = local_decoder.read_chunk_payload(chunk_idx).unwrap();
        let (pay_b, pay_d) = remote_decoder
            .read_chunk_with_payload(chunk_idx, Some(raw_payload.as_ref()), None)
            .unwrap();
        assert_eq!(loc_b2, pay_b);
        assert_decoding_result_eq(&loc_d2, &pay_d);
    }
}

/// A georeferenced 8-bit GeoTIFF in 16-row strips, compressed with PackBits: a codec the
/// accelerated decoder does not cover, so every strip goes to the generic TIFF decoder.
fn packbits_geotiff(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
    use tiff::encoder::{colortype, Compression, TiffEncoder};
    // Pseudo-random samples, so PackBits cannot shrink the strips much.
    let pixels: Vec<u8> = (0..width * height)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let mut out = std::io::Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut out)
            .unwrap()
            .with_compression(Compression::Packbits);
        let mut image = encoder
            .new_image::<colortype::Gray8>(width, height)
            .unwrap();
        image.rows_per_strip(16).unwrap();
        image
            .encoder()
            .write_tag(
                tiff::tags::Tag::ModelPixelScaleTag,
                &[0.001f64, 0.001, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                tiff::tags::Tag::ModelTiepointTag,
                &[0.0f64, 0.0, 0.0, -122.45, 37.85, 0.0][..],
            )
            .unwrap();
        image
            .encoder()
            .write_tag(
                tiff::tags::Tag::GeoKeyDirectoryTag,
                &[1u16, 1, 0, 2, 1024, 0, 1, 2, 2048, 0, 1, 4326][..],
            )
            .unwrap();
        image.write_data(&pixels).unwrap();
    }
    (out.into_inner(), pixels)
}

// Chunks the accelerated decoder hands to the generic TIFF decoder were downloaded twice:
// once by the prefetch queue and again by the generic decoder reading the source.
#[test]
fn remote_chunks_for_the_generic_decoder_are_downloaded_once() {
    if !MockHttpServer::is_networking_supported() {
        eprintln!("Skipping test: localhost networking not permitted in test environment");
        return;
    }
    use raster_h3_core::aggregator::multi_horizon::{
        MultiResolutionConfig, MultiScanHorizonStreamer,
    };

    let (bytes, _) = packbits_geotiff(512, 1024);
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), &bytes).unwrap();
    let local = GeoTiffStreamReader::open(file.path()).unwrap();
    let info = local.chunk_info.as_ref().unwrap().clone();
    assert_eq!(info.compression, tiff::tags::CompressionMethod::PackBits);
    let chunk_bytes: u64 = info.chunk_bytes.iter().sum();
    assert!(
        chunk_bytes > 400_000,
        "fixture too compressible: {chunk_bytes}"
    );

    let collect = |reader: GeoTiffStreamReader| {
        let config = MultiResolutionConfig::new(vec![8]);
        let mut streamer = MultiScanHorizonStreamer::new(reader, &config).unwrap();
        let mut records = Vec::new();
        loop {
            let batch = streamer.fetch_next_batch(1024).unwrap();
            if batch.is_empty() {
                break;
            }
            records.extend(batch);
        }
        records.sort_by_key(|r| (r.resolution, r.h3_index));
        records
    };

    let server = MockHttpServer::start(bytes);
    let remote = GeoTiffStreamReader::open(format!("{}/packbits.tif", server.url_base)).unwrap();
    let after_open = server.bytes_served.load(Ordering::SeqCst);
    let got = collect(remote);
    let streamed = (server.bytes_served.load(Ordering::SeqCst) - after_open) as u64;

    eprintln!("streamed {streamed} bytes for {chunk_bytes} bytes of strips");
    assert_eq!(
        got,
        collect(local),
        "remote output must match the local file"
    );
    // Every strip is fetched once by the prefetch queue; allow for strips the open
    // already cached and for range coalescing, but not for a second download.
    assert!(
        streamed <= chunk_bytes + chunk_bytes / 4,
        "streamed {streamed} bytes for {chunk_bytes} bytes of strips"
    );
}

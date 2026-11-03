//! Partitioned scans (`MultiResolutionConfig::partition`) must reproduce a single scan.
//!
//! Each case scans a raster or mosaic once as a whole and once as every partition of a
//! plan, merging the partitions' partial cells, and requires the same cells with the same
//! statistics (up to floating-point summation order). Cases cover tiled and striped
//! layouts, geographic and projected CRSs, mosaics under every overlap rule, quantiles,
//! categorical data, filters and spilling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use raster_h3_core::aggregator::accumulator::H3Accumulator;
use raster_h3_core::aggregator::categorical::CategoricalAccumulator;
use raster_h3_core::aggregator::multi_horizon::{
    decode_partial_state, encode_partial_state, plan_partitions, MultiCategoricalHorizonStreamer,
    MultiResolutionConfig, MultiScanHorizonStreamer, QuantileTarget, ScanPartition,
};
use raster_h3_core::raster::mosaic::{MosaicReader, OverlapRule};

/// Generous bound for decoded partial states in these tests.
const STATE_LIMIT: usize = 64 << 20;

// -----------------------------------------------------------------------------------------
// Tiled GeoTIFF fixtures
// -----------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Crs {
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
trait Sample: Copy {
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
enum Layout {
    /// Tiles of `(width, height)` pixels, padded at the right and bottom edges.
    Tiles(u32, u32),
    /// Strips of `rows` full-width rows; the last strip holds the remaining rows.
    Strips(u32),
}

/// Write an uncompressed, tiled, single-band GeoTIFF with `value(col, row)` pixels.
fn write_tiled<T: Sample>(
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
fn write_geotiff<T: Sample>(
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

/// Smooth surface with texture, and a sprinkling of NoData (-9999).
fn surface(x: u32, y: u32) -> f32 {
    if (x * 7 + y * 3).is_multiple_of(101) {
        return -9999.0;
    }
    let (fx, fy) = (x as f32, y as f32);
    100.0 + 50.0 * (fx / 17.0).sin() * (fy / 23.0).cos() + ((x * 31 + y * 17) % 13) as f32
}

/// Patchy land-cover classes 1..=5, with NoData (0).
fn classes(x: u32, y: u32) -> u8 {
    if (x * 5 + y * 11).is_multiple_of(89) {
        return 0;
    }
    (((x / 9) + (y / 13) + (x * y / 400)) % 5 + 1) as u8
}

const SF: Crs = Crs::Geographic {
    lon: -122.45,
    lat: 37.80,
    pixel_deg: 0.0005,
};

fn open(paths: &[PathBuf], rule: OverlapRule) -> MosaicReader {
    MosaicReader::open(paths, None, None, rule).unwrap()
}

// -----------------------------------------------------------------------------------------
// Single vs partitioned scans
// -----------------------------------------------------------------------------------------

type Cells<A> = BTreeMap<(u8, u64), A>;

/// Aggregation results of one run plus the partition statistics.
struct Run<A> {
    cells: Cells<A>,
    partitions: usize,
    interior: u64,
    boundary: u64,
}

fn insert_unique<A>(cells: &mut Cells<A>, key: (u8, u64), acc: A, what: &str) {
    assert!(
        cells.insert(key, acc).is_none(),
        "cell {key:?} emitted twice ({what})"
    );
}

fn single_continuous(
    paths: &[PathBuf],
    rule: OverlapRule,
    config: &MultiResolutionConfig,
) -> Cells<H3Accumulator> {
    let mut s = MultiScanHorizonStreamer::new_mosaic(Arc::new(open(paths, rule)), config).unwrap();
    let mut cells = Cells::new();
    while s
        .drain_completed_into(1024, |_, r| {
            insert_unique(
                &mut cells,
                (r.resolution, r.h3_index),
                r.accumulator,
                "single scan",
            )
        })
        .unwrap()
        > 0
    {}
    cells
}

/// Scan every partition of a plan, pass each partial state through its byte encoding
/// (as a distributed engine would), merge the partials and combine with interior cells.
fn partitioned_continuous(
    paths: &[PathBuf],
    rule: OverlapRule,
    config: &MultiResolutionConfig,
    target_chunks: usize,
) -> Run<H3Accumulator> {
    let plan = plan_partitions(&open(paths, rule), target_chunks).unwrap();
    let mut merger = MultiScanHorizonStreamer::partial_merger(config);
    let mut cells = Cells::new();
    let (mut interior, mut boundary) = (0, 0);
    for partition in &plan {
        // Ship the partition as JSON and reopen the mosaic, like a remote worker.
        let partition = ScanPartition::from_json(&partition.to_json()).unwrap();
        let mut cfg = config.clone();
        cfg.partition = Some(partition);
        let mut s =
            MultiScanHorizonStreamer::new_mosaic(Arc::new(open(paths, rule)), &cfg).unwrap();
        let mut partials = Vec::new();
        while s
            .drain_partitioned_into(
                512,
                |_, r| {
                    insert_unique(
                        &mut cells,
                        (r.resolution, r.h3_index),
                        r.accumulator,
                        "interior",
                    )
                },
                |p| {
                    partials.push((
                        p.resolution,
                        p.h3_index,
                        encode_partial_state(&p.accumulator),
                    ))
                },
            )
            .unwrap()
            > 0
        {}
        let (i, b) = s.partition_cell_counts().unwrap();
        assert_eq!(b as usize, partials.len());
        (interior, boundary) = (interior + i, boundary + b);
        for (res, h3, state) in partials {
            merger.push_encoded(res, h3, &state, STATE_LIMIT).unwrap();
        }
    }
    for r in merger.finish() {
        insert_unique(
            &mut cells,
            (r.resolution, r.h3_index),
            r.accumulator,
            "interior and merged",
        );
    }
    Run {
        cells,
        partitions: plan.len(),
        interior,
        boundary,
    }
}

fn single_categorical(
    paths: &[PathBuf],
    config: &MultiResolutionConfig,
) -> Cells<CategoricalAccumulator> {
    let mosaic = Arc::new(open(paths, OverlapRule::Cutline));
    let mut s = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, config).unwrap();
    let mut cells = Cells::new();
    while s
        .drain_completed_into(1024, |_, r| {
            insert_unique(
                &mut cells,
                (r.resolution, r.h3_index),
                r.accumulator,
                "single scan",
            )
        })
        .unwrap()
        > 0
    {}
    cells
}

fn partitioned_categorical(
    paths: &[PathBuf],
    config: &MultiResolutionConfig,
    target_chunks: usize,
) -> Run<CategoricalAccumulator> {
    let plan = plan_partitions(&open(paths, OverlapRule::Cutline), target_chunks).unwrap();
    let mut merger = MultiCategoricalHorizonStreamer::partial_merger(config);
    let mut cells = Cells::new();
    let (mut interior, mut boundary) = (0, 0);
    for partition in &plan {
        let mut cfg = config.clone();
        cfg.partition = Some(partition.clone());
        let mosaic = Arc::new(open(paths, OverlapRule::Cutline));
        let mut s = MultiCategoricalHorizonStreamer::new_mosaic(mosaic, &cfg).unwrap();
        let mut partials = Vec::new();
        while s
            .drain_partitioned_into(
                512,
                |_, r| {
                    insert_unique(
                        &mut cells,
                        (r.resolution, r.h3_index),
                        r.accumulator,
                        "interior",
                    )
                },
                |p| {
                    partials.push((
                        p.resolution,
                        p.h3_index,
                        encode_partial_state(&p.accumulator),
                    ))
                },
            )
            .unwrap()
            > 0
        {}
        let (i, b) = s.partition_cell_counts().unwrap();
        (interior, boundary) = (interior + i, boundary + b);
        for (res, h3, state) in partials {
            merger.push_encoded(res, h3, &state, STATE_LIMIT).unwrap();
        }
    }
    for r in merger.finish() {
        insert_unique(
            &mut cells,
            (r.resolution, r.h3_index),
            r.accumulator,
            "interior and merged",
        );
    }
    Run {
        cells,
        partitions: plan.len(),
        interior,
        boundary,
    }
}

/// Equal up to floating-point summation order.
fn close(a: f64, b: f64, what: &str, key: (u8, u64)) {
    let tol = 1e-9 * a.abs().max(b.abs()).max(1.0);
    assert!(
        (a - b).abs() <= tol || (a.is_nan() && b.is_nan()),
        "{what} differs for cell {key:?}: single {a}, partitioned {b}"
    );
}

fn assert_same_continuous(
    single: &Cells<H3Accumulator>,
    run: &Run<H3Accumulator>,
    quantiles: &[f64],
) {
    assert!(!single.is_empty());
    let missing: Vec<_> = single
        .keys()
        .filter(|k| !run.cells.contains_key(k))
        .take(5)
        .collect();
    let extra: Vec<_> = run
        .cells
        .keys()
        .filter(|k| !single.contains_key(k))
        .take(5)
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "cell sets differ: missing {missing:?}, extra {extra:?} ({} vs {})",
        single.len(),
        run.cells.len()
    );
    for (key, a) in single {
        let b = &run.cells[key];
        close(a.count, b.count, "count", *key);
        close(a.sum, b.sum, "sum", *key);
        close(a.min, b.min, "min", *key);
        close(a.max, b.max, "max", *key);
        close(a.mean(), b.mean(), "mean", *key);
        close(a.variance(), b.variance(), "variance", *key);
        assert_eq!(
            a.quantiles.is_some(),
            b.quantiles.is_some(),
            "sketch presence for {key:?}"
        );
        for &q in quantiles {
            close(a.quantile(q), b.quantile(q), &format!("p{q}"), *key);
        }
    }
}

fn assert_same_categorical(
    single: &Cells<CategoricalAccumulator>,
    run: &Run<CategoricalAccumulator>,
) {
    assert!(!single.is_empty());
    assert_eq!(
        single.keys().collect::<Vec<_>>(),
        run.cells.keys().collect::<Vec<_>>(),
        "cell sets differ"
    );
    for (key, a) in single {
        let b = &run.cells[key];
        close(a.total_count, b.total_count, "total_count", *key);
        let mut classes_a = Vec::new();
        a.for_each_class(|k, v| classes_a.push((k, v)));
        classes_a.sort_by_key(|&(k, _)| k);
        for (k, v) in classes_a {
            close(v, b.get_class_count(k), &format!("class {k}"), *key);
        }
        assert_eq!(
            a.unique_classes(),
            b.unique_classes(),
            "unique classes for {key:?}"
        );
        assert_eq!(a.majority().0, b.majority().0, "majority class for {key:?}");
        close(a.shannon_entropy(), b.shannon_entropy(), "entropy", *key);
    }
}

fn quantile_config(resolutions: Vec<u8>) -> MultiResolutionConfig {
    let mut config = MultiResolutionConfig::new(resolutions);
    config.quantiles = vec![
        QuantileTarget::Percentile(0.5, "p50".into()),
        QuantileTarget::Percentile(0.9, "p90".into()),
    ];
    config
}

#[test]
fn tiled_geographic_continuous_matches_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sf.tif");
    // 10 x 7 tiles with partial edge tiles.
    write_tiled(&path, (300, 220), (32, 32), SF, "-9999", surface);
    let paths = vec![path];
    let config = quantile_config(vec![7, 8, 9]);
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);

    let mut prev_partitions = 0;
    for target in [10_000, 20, 7, 3, 1] {
        let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, target);
        assert_same_continuous(&single, &run, &[0.5, 0.9]);
        assert!(
            run.partitions > prev_partitions,
            "target {target}: {} partitions",
            run.partitions
        );
        prev_partitions = run.partitions;
        if run.partitions == 1 {
            assert_eq!(run.boundary, 0, "a single partition has no boundary cells");
        } else {
            assert!(run.boundary > 0, "target {target}: no boundary cells");
        }
    }
}

#[test]
fn boundary_cells_stay_a_small_fraction_for_large_partitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.tif");
    // 32 x 24 tiles of 16 px; res 9 cells span a few pixels.
    write_tiled(&path, (512, 384), (16, 16), SF, "-9999", surface);
    let paths = vec![path];
    let config = MultiResolutionConfig::new(vec![9]);
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);
    // A handful of partitions of ~192 chunks (224 x 224 px) each.
    let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, 192);
    assert_same_continuous(&single, &run, &[]);
    assert!(run.partitions > 1);
    let fraction = run.boundary as f64 / (run.interior + run.boundary) as f64;
    eprintln!(
        "{} partitions, boundary fraction {fraction:.3}",
        run.partitions
    );
    assert!(fraction < 0.25, "boundary fraction {fraction:.3}");
}

#[test]
fn tiled_projected_continuous_matches_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("utm.tif");
    let crs = Crs::Utm10n {
        easting: 550_000.0,
        northing: 4_180_000.0,
        pixel_m: 30.0,
    };
    write_tiled(&path, (320, 256), (64, 32), crs, "-9999", surface);
    let paths = vec![path];
    let config = MultiResolutionConfig::new(vec![8, 9]);
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);
    for target in [2, 9] {
        let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, target);
        assert!(run.partitions > 1);
        assert_same_continuous(&single, &run, &[]);
    }
}

#[test]
fn striped_raster_partitions_by_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("striped.tif");
    write_geotiff(&path, (240, 181), Layout::Strips(8), SF, "-9999", surface);
    let paths = vec![path];
    let config = MultiResolutionConfig::new(vec![8, 9]);
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);
    assert_eq!(open(&paths, OverlapRule::Cutline).chunk_refs.len(), 23);
    let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, 5);
    assert!(run.partitions > 2);
    assert_same_continuous(&single, &run, &[]);
}

#[test]
fn mosaics_match_single_scan_under_every_overlap_rule() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("b.tif");
    write_tiled(&a, (200, 160), (32, 32), SF, "-9999", surface);
    // Overlaps the first tile by 100 x 100 pixels, with a different tile size.
    let b_crs = Crs::Geographic {
        lon: -122.40,
        lat: 37.77,
        pixel_deg: 0.0005,
    };
    write_tiled(&b, (200, 160), (48, 40), b_crs, "-9999", |x, y| {
        surface(x + 100, y + 60) + 3.0
    });
    let paths = vec![a, b];
    let config = quantile_config(vec![8, 9]);
    for rule in [
        OverlapRule::Cutline,
        OverlapRule::First,
        OverlapRule::Average,
    ] {
        let single = single_continuous(&paths, rule, &config);
        for target in [4, 13, 1_000] {
            let run = partitioned_continuous(&paths, rule, &config, target);
            assert_same_continuous(&single, &run, &[0.5, 0.9]);
        }
    }
}

#[test]
fn filters_apply_to_merged_cells() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sf.tif");
    write_tiled(&path, (300, 220), (32, 32), SF, "-9999", surface);
    let paths = vec![path];
    let mut config = MultiResolutionConfig::new(vec![8, 9]);
    // Boundary cells split between partitions hold fewer samples each than in total, so
    // filtering partials would drop cells a single scan keeps.
    config.min_count = Some(12.0);
    config.min_mean = Some(95.0);
    config.max_mean = Some(140.0);
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);
    let unfiltered = single_continuous(
        &paths,
        OverlapRule::Cutline,
        &MultiResolutionConfig::new(vec![8, 9]),
    );
    assert!(
        single.len() < unfiltered.len(),
        "filters must remove some cells"
    );
    for target in [3, 8] {
        let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, target);
        assert_same_continuous(&single, &run, &[]);
    }
}

#[test]
fn categorical_matches_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("classes.tif");
    write_tiled(&path, (300, 220), (32, 32), SF, "0", classes);
    let paths = vec![path];
    let config = MultiResolutionConfig::new(vec![8, 9]);
    let single = single_categorical(&paths, &config);
    for target in [2, 6, 25] {
        let run = partitioned_categorical(&paths, &config, target);
        assert!(run.partitions > 1);
        assert_same_categorical(&single, &run);
    }

    let mut filtered = config.clone();
    filtered.min_count = Some(10.0);
    filtered.min_majority_fraction = Some(0.6);
    let single = single_categorical(&paths, &filtered);
    let run = partitioned_categorical(&paths, &filtered, 6);
    assert_same_categorical(&single, &run);
}

#[test]
fn spilling_partitions_match_single_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sf.tif");
    write_tiled(&path, (300, 220), (32, 32), SF, "-9999", surface);
    let spill = tempfile::tempdir().unwrap();
    let paths = vec![path];
    let mut config = MultiResolutionConfig::new(vec![10]);
    config.aggregation_budget_bytes = 64 * 1024;
    config.spill_directory = Some(spill.path().to_path_buf());
    let single = single_continuous(&paths, OverlapRule::Cutline, &config);
    let run = partitioned_continuous(&paths, OverlapRule::Cutline, &config, 12);
    assert!(run.partitions > 1);
    assert_same_continuous(&single, &run, &[]);

    // The budget is small enough that a partition does spill.
    let mut cfg = config.clone();
    cfg.partition =
        Some(plan_partitions(&open(&paths, OverlapRule::Cutline), 12).unwrap()[0].clone());
    let mut s =
        MultiScanHorizonStreamer::new_mosaic(Arc::new(open(&paths, OverlapRule::Cutline)), &cfg)
            .unwrap();
    while s.drain_partitioned_into(512, |_, _| {}, |_| {}).unwrap() > 0 {}
    assert!(s.spill_run_count() > 0, "partition did not spill");
}

// -----------------------------------------------------------------------------------------
// Plans, encoding and misuse
// -----------------------------------------------------------------------------------------

#[test]
fn plans_cover_every_chunk_once() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("b.tif");
    let c = dir.path().join("c.tif");
    write_tiled(&a, (300, 220), (32, 32), SF, "-9999", surface);
    write_tiled(&b, (64, 64), (32, 32), SF, "-9999", surface); // 4 chunks
    write_tiled(&c, (40, 40), (32, 32), SF, "-9999", surface); // 4 chunks
    let mosaic = open(&[a, b, c], OverlapRule::Cutline);
    for target in [1, 2, 5, 16, 70, 1_000] {
        let plan = plan_partitions(&mosaic, target).unwrap();
        let mut owners = vec![0u32; mosaic.chunk_refs.len()];
        for p in &plan {
            assert_eq!(p.count as usize, plan.len());
            for (i, chunk) in mosaic.chunk_refs.iter().enumerate() {
                if p.contains_chunk(&mosaic, chunk) {
                    owners[i] += 1;
                }
            }
            // Blocks are rounded to whole chunk rows and columns.
            assert!(
                p.chunk_count(&mosaic) <= 2 * target,
                "partition far over target"
            );
        }
        assert!(
            owners.iter().all(|&n| n == 1),
            "target {target}: chunk owners {owners:?}"
        );
    }
    // The two small tiles (8 chunks) share one partition when they fit.
    let plan = plan_partitions(&mosaic, 70).unwrap();
    assert!(plan
        .iter()
        .any(|p| p.blocks.iter().any(|b| b.tile_idx == 1)
            && p.blocks.iter().any(|b| b.tile_idx == 2)));
    assert!(plan_partitions(&mosaic, 0).is_err());
}

#[test]
fn partial_states_round_trip_and_reject_corruption() {
    let mut acc = H3Accumulator::with_quantiles();
    for v in [1.0, 2.5, 7.0, -3.0, 0.0] {
        acc.update(v);
    }
    let bytes = encode_partial_state(&acc);
    let back: H3Accumulator = decode_partial_state(&bytes, STATE_LIMIT).unwrap();
    assert_eq!(
        (back.count, back.sum, back.min, back.max),
        (acc.count, acc.sum, acc.min, acc.max)
    );
    assert_eq!(back.quantile(0.5), acc.quantile(0.5));

    assert!(decode_partial_state::<H3Accumulator>(&bytes[..bytes.len() - 1], STATE_LIMIT).is_err());
    assert!(decode_partial_state::<H3Accumulator>(b"nope", STATE_LIMIT).is_err());
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(decode_partial_state::<H3Accumulator>(&extra, STATE_LIMIT).is_err());
    // A continuous state is not a categorical one.
    assert!(decode_partial_state::<CategoricalAccumulator>(&bytes, STATE_LIMIT).is_err());

    let mut cat = CategoricalAccumulator::new();
    cat.update(3);
    cat.update(3);
    cat.update(9);
    let back: CategoricalAccumulator =
        decode_partial_state(&encode_partial_state(&cat), STATE_LIMIT).unwrap();
    assert_eq!(back.get_class_count(3), 2.0);
    assert_eq!(back.total_count, 3.0);
}

#[test]
fn partitions_refuse_other_mosaics_and_misuse() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.tif");
    let b = dir.path().join("b.tif");
    write_tiled(&a, (300, 220), (32, 32), SF, "-9999", surface);
    write_tiled(&b, (300, 221), (32, 32), SF, "-9999", surface);
    let plan = plan_partitions(&open(std::slice::from_ref(&a), OverlapRule::Cutline), 8).unwrap();
    let mut config = MultiResolutionConfig::new(vec![8]);
    config.partition = Some(plan[0].clone());

    // Another raster, or the same one with another bbox, is a different mosaic.
    let other = Arc::new(open(&[b], OverlapRule::Cutline));
    assert!(MultiScanHorizonStreamer::new_mosaic(other, &config).is_err());
    let narrowed = MosaicReader::open(
        std::slice::from_ref(&a),
        Some([-122.45, 37.72, -122.40, 37.80]),
        None,
        OverlapRule::Cutline,
    )
    .unwrap();
    assert!(MultiScanHorizonStreamer::new_mosaic(Arc::new(narrowed), &config).is_err());

    // Compaction would need sibling groups that span partitions.
    let mut compact = config.clone();
    compact.compact_h3_children = true;
    let mosaic = Arc::new(open(std::slice::from_ref(&a), OverlapRule::Cutline));
    assert!(MultiScanHorizonStreamer::new_mosaic(Arc::clone(&mosaic), &compact).is_err());

    // Record-only reads would drop partial cells.
    let mut s = MultiScanHorizonStreamer::new_mosaic(mosaic, &config).unwrap();
    assert!(s.drain_completed_into(16, |_, _| {}).is_err());

    // Tampered blocks are rejected.
    let mut bad = plan[0].clone();
    bad.blocks[0].col_end = 1_000;
    let mut cfg = MultiResolutionConfig::new(vec![8]);
    cfg.partition = Some(bad);
    let mosaic = Arc::new(open(&[a], OverlapRule::Cutline));
    assert!(MultiScanHorizonStreamer::new_mosaic(mosaic, &cfg).is_err());
    assert!(ScanPartition::from_json("{").is_err());
}

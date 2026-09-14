use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::time::Instant;
use tiff::encoder::colortype::Gray32Float;
use tiff::encoder::TiffEncoder;
use tiff::tags::Tag;

use raster_h3::aggregator::{
    MultiCategoricalHorizonStreamer, MultiResolutionConfig, MultiScanHorizonStreamer,
    SamplingPattern,
};
use raster_h3::raster::geotiff::GeoTiffStreamReader;

fn generate_benchmark_raster(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    let file = File::create(path)?;
    let writer = BufWriter::with_capacity(4 * 1024 * 1024, file);
    let mut encoder = TiffEncoder::new(writer)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let mut image = encoder
        .new_image::<Gray32Float>(width, height)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // ModelTiepoint: San Francisco (-122.50, 37.85)
    image
        .encoder()
        .write_tag(Tag::Unknown(33922), &[-0.0f64, 0.0, 0.0, -122.50, 37.85, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // Pixel Scale: ~10m resolution (0.0001 deg)
    image
        .encoder()
        .write_tag(Tag::Unknown(33550), &[0.0001f64, 0.0001, 0.0][..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    // EPSG:4326 GeoKeys
    let geokeys: [u16; 12] = [
        1, 1, 0, 2,
        1024, 0, 1, 2,
        2048, 0, 1, 4326,
    ];
    image
        .encoder()
        .write_tag(Tag::Unknown(34735), &geokeys[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let total_pixels = (width as usize) * (height as usize);
    let mut data = Vec::with_capacity(total_pixels);
    for row in 0..height {
        let r_val = (row as f32) * 0.05;
        for col in 0..width {
            let c_val = (col as f32) * 0.02;
            data.push(100.0 + r_val + c_val);
        }
    }

    image
        .write_data(&data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    Ok(())
}

fn benchmark_dataset(path: &Path, label: &str) {
    let reader = match GeoTiffStreamReader::open(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not open {}: {}", path.display(), e);
            return;
        }
    };
    let width = reader.metadata.width;
    let height = reader.metadata.height;
    let total_pixels = (width as u64) * (height as u64);
    let mpx = total_pixels as f64 / 1_000_000.0;

    println!("\n================================================================================");
    println!("  BENCHMARK: {}", label);
    println!("  Raster: {}x{} ({:.2} Megapixels)", width, height, mpx);
    println!("  Resolutions: H3 Res 8 + Res 9");
    println!("================================================================================");

    println!("\n--- [Continuous: Center Point] ---");
    let t_res8 = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig::single(8);
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 8:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };

    let t_res9 = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig::single(9);
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 9:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let t_seq = t_res8 + t_res9;
    println!("  Sequential Total:       {:6.2} ms ({:6.2} Mpx/s eqv)", t_seq * 1000.0, (2.0 * mpx) / t_seq);

    let t_fused = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig { resolutions: vec![8, 9], ..Default::default() };
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Fused Res [8, 9]:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let speedup = t_seq / t_fused;
    println!("  >> Speedup vs Sequential: {:.2}x ({:.1}% time saved)", speedup, (1.0 - t_fused / t_seq) * 100.0);

    println!("\n--- [Continuous: 5-Point Super-Sampling] ---");
    let t_res8_5p = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let mut cfg = MultiResolutionConfig::single(8);
        cfg.sampling = SamplingPattern::five_point();
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 8 (5pt): {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let t_res9_5p = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let mut cfg = MultiResolutionConfig::single(9);
        cfg.sampling = SamplingPattern::five_point();
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 9 (5pt): {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let t_seq_5p = t_res8_5p + t_res9_5p;
    println!("  Sequential Total:       {:6.2} ms", t_seq_5p * 1000.0);

    let t_fused_5p = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig { resolutions: vec![8, 9], sampling: SamplingPattern::five_point(), ..Default::default() };
        let mut streamer = MultiScanHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Fused Res [8, 9] (5pt): {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let speedup_5p = t_seq_5p / t_fused_5p;
    println!("  >> Speedup vs Sequential: {:.2}x ({:.1}% time saved)", speedup_5p, (1.0 - t_fused_5p / t_seq_5p) * 100.0);

    println!("\n--- [Categorical: Center Point] ---");
    let t_cat_res8 = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig::single(8);
        let mut streamer = MultiCategoricalHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 8:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let t_cat_res9 = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig::single(9);
        let mut streamer = MultiCategoricalHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Standalone Res 9:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let t_cat_seq = t_cat_res8 + t_cat_res9;
    println!("  Sequential Total:       {:6.2} ms", t_cat_seq * 1000.0);

    let t_cat_fused = {
        let r = GeoTiffStreamReader::open(path).unwrap();
        let cfg = MultiResolutionConfig { resolutions: vec![8, 9], ..Default::default() };
        let mut streamer = MultiCategoricalHorizonStreamer::new(r, &cfg).unwrap();
        let start = Instant::now();
        let mut count = 0;
        loop {
            let b = streamer.fetch_next_batch(32);
            if b.is_empty() { break; }
            count += b.len();
        }
        let dur = start.elapsed().as_secs_f64();
        println!("  Fused Res [8, 9]:       {:6.2} ms ({:6.2} Mpx/s, {} cells)", dur * 1000.0, mpx / dur, count);
        dur
    };
    let cat_speedup = t_cat_seq / t_cat_fused;
    println!("  >> Speedup vs Sequential: {:.2}x ({:.1}% time saved)", cat_speedup, (1.0 - t_cat_fused / t_cat_seq) * 100.0);
}

fn main() {
    let temp_file = tempfile::NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();
    print!("Generating synthetic GeoTIFF (2048x2048)... ");
    let t0 = Instant::now();
    generate_benchmark_raster(&path, 2048, 2048).unwrap();
    println!("done in {:.2?}", t0.elapsed());

    benchmark_dataset(&path, "Synthetic 2048x2048 (In-Memory Uncompressed)");

    let cfl_path = Path::new("data/CFL_HI.tif");
    if cfl_path.exists() {
        benchmark_dataset(cfl_path, "Real Dataset: Hawaii Canopy Fuel Load (CFL_HI.tif)");
    }
}

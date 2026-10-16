//! Unified `raster-h3-cli` executable supporting `ipc` (Arrow/Columnar IPC to stdout or file),
//! `jsonl` (`JSONEachRow` to stdout for ClickHouse executable table functions),
//! `parquet`, and `pmtiles` exports.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::Path;

use raster_h3_cli::{
    export_raster_to_parquet, export_raster_to_pmtiles, parse_stream_args, stream_raster_to_writer,
    StreamOutputFormat,
};

fn print_help() {
    eprintln!(
        "raster-h3-cli: High-performance GeoTIFF/COG to H3 CLI\n\n\
        Usage:\n  \
          raster-h3-cli ipc <input.tif> [options]        Stream binary Columnar IPC to stdout or -o <file>\n  \
          raster-h3-cli jsonl <input.tif> [options]      Stream JSONEachRow (NDJSON) to stdout or -o <file>\n  \
          raster-h3-cli parquet <input.tif> -o <out>     Export directly to Parquet / GeoParquet\n  \
          raster-h3-cli pmtiles <input.tif> -o <out>     Export directly to PMTiles v3 archive\n\n\
        Options:\n  \
          -r, --resolutions <LIST>   H3 resolutions (e.g. 8 or 7,8,9; default: 8)\n  \
          -c, --categorical          Enable categorical aggregation\n  \
          --format <wide|long>       Categorical output format (default: wide)\n  \
          --crs <CRS>                Override source CRS (e.g. EPSG:4326)\n  \
          --nodata <FLOAT>           Override nodata value\n  \
          --bbox <minx,miny,maxx,maxy> WGS84 bounding box filter\n  \
          --columns <col1,col2,...>  Column projection pushdown\n  \
          --limit <N>                Maximum number of rows to emit\n  \
          -o, --output <PATH|->      Output file path (default: '-' for stdout on ipc/jsonl)"
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        if args.len() < 2 {
            std::process::exit(1);
        }
        return;
    }

    let subcmd = args[1].as_str();
    let rest = &args[2..];

    let result = match subcmd {
        "ipc" => parse_stream_args(rest, StreamOutputFormat::Ipc).and_then(|(opts, out)| {
            if let Some(path) = out {
                let mut writer = BufWriter::new(File::create(path)?);
                stream_raster_to_writer(&opts, &mut writer)
            } else {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                stream_raster_to_writer(&opts, &mut writer)
            }
        }),
        "jsonl" => parse_stream_args(rest, StreamOutputFormat::Jsonl).and_then(|(opts, out)| {
            if let Some(path) = out {
                let mut writer = BufWriter::new(File::create(path)?);
                stream_raster_to_writer(&opts, &mut writer)
            } else {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                stream_raster_to_writer(&opts, &mut writer)
            }
        }),
        "parquet" => parse_stream_args(rest, StreamOutputFormat::Ipc).and_then(|(opts, out)| {
            let out_path = out.ok_or_else(|| {
                raster_h3_core::error::RasterH3Error::InvalidParameter(
                    "Missing -o / --output path for parquet export".into(),
                )
            })?;
            export_raster_to_parquet(
                &opts.input_path,
                Path::new(&out_path),
                opts.resolutions,
                opts.categorical,
                opts.source_crs,
                opts.nodata,
                opts.bbox,
                true,
            )
        }),
        "pmtiles" => parse_stream_args(rest, StreamOutputFormat::Ipc).and_then(|(opts, out)| {
            let out_path = out.ok_or_else(|| {
                raster_h3_core::error::RasterH3Error::InvalidParameter(
                    "Missing -o / --output path for pmtiles export".into(),
                )
            })?;
            export_raster_to_pmtiles(
                &opts.input_path,
                Path::new(&out_path),
                opts.resolutions,
                opts.categorical,
                opts.source_crs,
                opts.nodata,
                opts.bbox,
            )
        }),
        other => {
            eprintln!("Unknown subcommand: '{}'", other);
            print_help();
            std::process::exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

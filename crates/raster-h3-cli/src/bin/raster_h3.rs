//! Unified `raster-h3-cli` executable supporting `ipc` (Arrow/Columnar IPC to stdout or file),
//! `jsonl` (`JSONEachRow` to stdout for ClickHouse executable table functions),
//! `parquet`, and `pmtiles` exports, `parquet-to-pmtiles` transcoding, plus `plan` / `merge`
//! for partitioned scans.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use raster_h3_cli::{
    export_raster_to_parquet_with_grid, export_raster_to_pmtiles_with_grid, merge_partition_files,
    parse_merge_args, parse_parquet_to_pmtiles_args, parse_plan_args, parse_stream_args,
    plan_raster_partitions, stream_raster_to_writer, transcode_parquet_to_pmtiles, write_plan,
    StreamOutputFormat,
};
use raster_h3_core::error::Result;

/// Run `f` on a buffered writer for `path`, or stdout when `None`.
fn with_output<T>(path: Option<String>, f: impl FnOnce(&mut dyn Write) -> Result<T>) -> Result<T> {
    match path {
        Some(path) => f(&mut BufWriter::new(File::create(path)?)),
        None => f(&mut BufWriter::new(io::stdout().lock())),
    }
}

fn print_help() {
    eprintln!(
        "raster-h3-cli: High-performance GeoTIFF/COG to H3 CLI\n\n\
        Usage:\n  \
          raster-h3-cli ipc <input.tif> [options]        Stream binary Columnar IPC to stdout or -o <file>\n  \
          raster-h3-cli jsonl <input.tif> [options]      Stream JSONEachRow (NDJSON) to stdout or -o <file>\n  \
          raster-h3-cli parquet <input.tif> -o <out>     Export directly to Parquet / GeoParquet\n  \
          raster-h3-cli pmtiles <input.tif> -o <out>     Export directly to PMTiles v3 archive\n  \
          raster-h3-cli parquet-to-pmtiles <in.parquet> -o <out>  Transcode H3-indexed Parquet to PMTiles v3\n  \
          raster-h3-cli plan <input.tif> [options]       Print partition specs (JSON Lines) for parallel scans\n  \
          raster-h3-cli merge <part.rh3>... [-o <out>]   Combine partitioned `ipc` outputs into final cells\n\n\
        Options:\n  \
          --grid <h3|a5>             Discrete global grid system (default: h3)\n  \
          -r, --resolutions <LIST>   Grid resolutions (H3: 0..=15, A5: 0..=30; default: 8)\n  \
          -c, --categorical          Enable categorical aggregation\n  \
          --format <wide|long>       Categorical output format (default: wide)\n  \
          --crs <CRS>                Override source CRS (e.g. EPSG:4326)\n  \
          --nodata <FLOAT>           Override nodata value\n  \
          --raw-values               Skip GDAL band scale/offset calibration\n  \
          --bbox <minx,miny,maxx,maxy> WGS84 bounding box filter\n  \
          --columns <col1,col2,...>  Column projection pushdown\n  \
          --limit <N>                Maximum number of rows to emit\n  \
          -o, --output <PATH|->      Output file path (default: '-' for stdout on ipc/jsonl/plan/merge)\n  \
          --h3-column <NAME>         parquet-to-pmtiles: H3 column, integer or hex (default: h3_index,\n  \
          \x20                          h3_hex, h3, cell or hex, whichever comes first)\n\n\
        Partitioned scans (shell fan-out; plan and scan with the same --crs and --bbox):\n  \
          plan --target-chunks <N>       TIFF tiles/strips per partition (default: 256)\n  \
          ipc --partition <SPEC|@FILE>   Scan one partition (adds a partial_state column)\n  \
          ipc --partition-index <K>      Line K of the plan file given as --partition @FILE\n  \
          merge -c                       Categorical partitions (else inferred from columns)\n  \
          merge --jsonl                  Write JSONEachRow instead of IPC\n\n  \
          raster-h3-cli plan dem.tif -o plan.jsonl\n  \
          seq 0 $(( $(wc -l < plan.jsonl) - 1 )) | xargs -P 8 -I{{}} \\\n      \
            raster-h3-cli ipc dem.tif -r 8 --partition @plan.jsonl --partition-index {{}} -o part-{{}}.rh3\n  \
          raster-h3-cli merge part-*.rh3 -o dem.rh3"
    );
}

fn reject_partition(opts: &raster_h3_cli::CliStreamOptions, subcmd: &str) -> Result<()> {
    if opts.partition.is_some() {
        return Err(raster_h3_core::error::RasterH3Error::InvalidParameter(format!(
            "{subcmd} does not take --partition: scan partitions with `ipc` and combine them with `merge`"
        )));
    }
    Ok(())
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
            with_output(out, |mut w| stream_raster_to_writer(&opts, &mut w))
        }),
        "jsonl" => parse_stream_args(rest, StreamOutputFormat::Jsonl).and_then(|(opts, out)| {
            with_output(out, |mut w| stream_raster_to_writer(&opts, &mut w))
        }),
        "plan" => parse_plan_args(rest).and_then(|args| {
            let plan = plan_raster_partitions(
                &args.input_path,
                args.target_chunks,
                args.source_crs.as_deref(),
                args.bbox,
            )?;
            with_output(args.output, |mut w| write_plan(&plan, &mut w))?;
            Ok(plan.len())
        }),
        "merge" => parse_merge_args(rest)
            .and_then(|(opts, out)| with_output(out, |mut w| merge_partition_files(&opts, &mut w))),
        "parquet" => parse_stream_args(rest, StreamOutputFormat::Ipc).and_then(|(opts, out)| {
            reject_partition(&opts, "parquet")?;
            let out_path = out.ok_or_else(|| {
                raster_h3_core::error::RasterH3Error::InvalidParameter(
                    "Missing -o / --output path for parquet export".into(),
                )
            })?;
            export_raster_to_parquet_with_grid(
                opts.grid,
                &opts.input_path,
                Path::new(&out_path),
                opts.resolutions,
                opts.categorical,
                opts.source_crs,
                opts.nodata,
                opts.bbox,
                true,
                opts.apply_scale_offset,
            )
        }),
        "pmtiles" => parse_stream_args(rest, StreamOutputFormat::Ipc).and_then(|(opts, out)| {
            reject_partition(&opts, "pmtiles")?;
            let out_path = out.ok_or_else(|| {
                raster_h3_core::error::RasterH3Error::InvalidParameter(
                    "Missing -o / --output path for pmtiles export".into(),
                )
            })?;
            export_raster_to_pmtiles_with_grid(
                opts.grid,
                &opts.input_path,
                Path::new(&out_path),
                opts.resolutions,
                opts.categorical,
                opts.source_crs,
                opts.nodata,
                opts.bbox,
                opts.apply_scale_offset,
            )
        }),
        "parquet-to-pmtiles" => parse_parquet_to_pmtiles_args(rest).and_then(|args| {
            let summary = transcode_parquet_to_pmtiles(
                Path::new(&args.input_path),
                Path::new(&args.output_path),
                args.h3_column.as_deref(),
            )?;
            if summary.invalid_features_dropped > 0 {
                eprintln!(
                    "raster-h3-cli: skipped {} of {} rows whose H3 value was null or not a valid cell",
                    summary.invalid_features_dropped, summary.total_features
                );
            }
            Ok(summary.valid_features)
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

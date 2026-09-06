use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use flate2::write::GzEncoder;
use flate2::Compression;

fn detect_platform() -> String {
    let os = match env::consts::OS {
        "linux" => "linux",
        "macos" => "osx",
        "windows" => "windows",
        other => other,
    };

    let arch = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    format!("{}_{}", os, arch)
}

fn print_help() {
    println!(
        "Usage: package_extension [OPTIONS]\n\n\
        Options:\n  \
        -i, --input <PATH>           Path to compiled dynamic library (.so, .dylib, .dll)\n  \
        -o, --output <PATH>          Path to output .duckdb_extension file\n  \
        -p, --platform <NAME>        DuckDB target platform (default: auto-detected host platform)\n  \
        -d, --duckdb-version <VER>   DuckDB version (default: v1.2.0)\n  \
        -v, --ext-version <VER>      Extension version (default: v{})\n  \
        -a, --abi-type <TYPE>        ABI type (0=CPP, 1=C_STRUCT, 2=C_STRUCT_V0; default: 1)\n  \
        -z, --gzip                   Compress output extension with gzip (.gz)\n  \
        -h, --help                   Print help information\n\n\
        Example:\n  \
        package_extension -i target/release/libraster_h3.so -o target/release/raster_h3.duckdb_extension",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut platform: Option<String> = None;
    let mut duckdb_version = "v1.2.0".to_string();
    let mut ext_version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut abi_type: u8 = 1; // 1 = C_STRUCT
    let mut gzip = false;

    let mut i = 1;
    let mut positional = Vec::new();

    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "-i" | "--input" => {
                i += 1;
                if i < args.len() {
                    input = Some(args[i].clone());
                }
            }
            "-o" | "--output" => {
                i += 1;
                if i < args.len() {
                    output = Some(args[i].clone());
                }
            }
            "-p" | "--platform" => {
                i += 1;
                if i < args.len() {
                    platform = Some(args[i].clone());
                }
            }
            "-d" | "--duckdb-version" => {
                i += 1;
                if i < args.len() {
                    duckdb_version = args[i].clone();
                }
            }
            "-v" | "--ext-version" => {
                i += 1;
                if i < args.len() {
                    ext_version = args[i].clone();
                }
            }
            "-a" | "--abi-type" => {
                i += 1;
                if i < args.len() {
                    abi_type = args[i].parse().unwrap_or(1);
                }
            }
            "-z" | "--gzip" => {
                gzip = true;
            }
            arg if !arg.starts_with('-') => {
                positional.push(arg.to_string());
            }
            unknown => {
                eprintln!("Unknown argument: {}", unknown);
                print_help();
                std::process::exit(1);
            }
        }
        i += 1;
    }

    if input.is_none() && !positional.is_empty() {
        input = Some(positional.remove(0));
    }
    if output.is_none() && !positional.is_empty() {
        output = Some(positional.remove(0));
    }
    if platform.is_none() && !positional.is_empty() {
        platform = Some(positional.remove(0));
    }
    if !positional.is_empty() {
        duckdb_version = positional.remove(0);
    }

    let input_path = match input {
        Some(p) => p,
        None => {
            eprintln!("Error: Missing required input file path (-i, --input)");
            print_help();
            std::process::exit(1);
        }
    };

    let output_path = match output {
        Some(p) => p,
        None => {
            let in_p = Path::new(&input_path);
            let stem = in_p.file_stem().and_then(|s| s.to_str()).unwrap_or("extension");
            let clean_stem = stem.strip_prefix("lib").unwrap_or(stem);
            let mut out = in_p.with_file_name(format!("{}.duckdb_extension", clean_stem));
            if gzip {
                out.set_extension("duckdb_extension.gz");
            }
            out.to_string_lossy().to_string()
        }
    };

    let target_platform = platform.unwrap_or_else(detect_platform);

    println!("Packaging DuckDB Extension:");
    println!("  Input:          {}", input_path);
    println!("  Output:         {}", output_path);
    println!("  Platform:       {}", target_platform);
    println!("  DuckDB Version: {}", duckdb_version);
    println!("  Ext Version:    {}", ext_version);
    println!("  ABI Type:       {} (C_STRUCT)", abi_type);
    println!("  Gzip:           {}", gzip);

    let mut so_bytes = Vec::new();
    let mut file = File::open(&input_path).unwrap_or_else(|e| {
        eprintln!("Error: Failed to open input file '{}': {}", input_path, e);
        std::process::exit(1);
    });
    file.read_to_end(&mut so_bytes).unwrap_or_else(|e| {
        eprintln!("Error: Failed to read input file: {}", e);
        std::process::exit(1);
    });

    let raw_size = so_bytes.len();
    if raw_size == 0 {
        eprintln!("Error: Input file is empty!");
        std::process::exit(1);
    }

    // DuckDB 512-byte extension footer
    let mut footer = [0u8; 512];

    // Offset 480..512: Magic version "4\0"
    let magic = b"4\0";
    footer[480..480 + magic.len()].copy_from_slice(magic);

    // Offset 448..480: Platform (e.g. linux_amd64, osx_arm64, windows_amd64)
    let plat_bytes = target_platform.as_bytes();
    let plat_len = plat_bytes.len().min(31);
    footer[448..448 + plat_len].copy_from_slice(&plat_bytes[..plat_len]);

    // Offset 416..448: DuckDB Version (e.g. v1.2.0)
    let ver_bytes = duckdb_version.as_bytes();
    let ver_len = ver_bytes.len().min(31);
    footer[416..416 + ver_len].copy_from_slice(&ver_bytes[..ver_len]);

    // Offset 384..416: Extension Version (e.g. v0.1.0)
    let ext_bytes = ext_version.as_bytes();
    let ext_len = ext_bytes.len().min(31);
    footer[384..384 + ext_len].copy_from_slice(&ext_bytes[..ext_len]);

    // Offset 352..384: ABI Type (0 = CPP, 1 = C_STRUCT, 2 = C_STRUCT_V0)
    footer[352] = abi_type;

    if gzip {
        let out_file = File::create(&output_path).unwrap_or_else(|e| {
            eprintln!("Error: Failed to create output file '{}': {}", output_path, e);
            std::process::exit(1);
        });
        let mut encoder = GzEncoder::new(out_file, Compression::default());
        encoder.write_all(&so_bytes).unwrap();
        encoder.write_all(&footer).unwrap();
        let written = encoder.finish().unwrap();
        let compressed_size = written.metadata().map(|m| m.len()).unwrap_or(0);
        println!(
            "Successfully packaged gzipped DuckDB extension: {} ({} -> {} bytes, {:.1}% ratio)",
            output_path,
            raw_size + 512,
            compressed_size,
            (compressed_size as f64 / (raw_size + 512) as f64) * 100.0
        );
    } else {
        let mut out_file = File::create(&output_path).unwrap_or_else(|e| {
            eprintln!("Error: Failed to create output file '{}': {}", output_path, e);
            std::process::exit(1);
        });
        out_file.write_all(&so_bytes).unwrap();
        out_file.write_all(&footer).unwrap();
        println!(
            "Successfully packaged DuckDB extension: {} (total {} bytes with 512-byte footer)",
            output_path,
            raw_size + 512
        );
    }
}

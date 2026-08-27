use std::env;
use std::fs::File;
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: append_duckdb_metadata <input_so> <output_extension> [platform] [duckdb_version]");
        return;
    }
    let input = &args[1];
    let output = &args[2];
    let platform = args.get(3).map(|s| s.as_str()).unwrap_or("linux_amd64");
    let version = args.get(4).map(|s| s.as_str()).unwrap_or("v1.5.5");

    let mut so_bytes = Vec::new();
    File::open(input).expect("Failed to open input .so").read_to_end(&mut so_bytes).unwrap();

    let mut footer = [0u8; 512];
    // Signature [0..256] = 0

    // Metadata fields (32 bytes each in reverse order):
    // Offset 480..512: Magic version "4"
    let magic = b"4\0";
    footer[480..480 + magic.len()].copy_from_slice(magic);

    // Offset 448..480: Platform (e.g. linux_amd64 or linux_arm64)
    let plat_bytes = platform.as_bytes();
    footer[448..448 + plat_bytes.len().min(31)].copy_from_slice(&plat_bytes[..plat_bytes.len().min(31)]);

    // Offset 416..448: DuckDB Version (e.g. v1.5.5)
    let ver_bytes = version.as_bytes();
    footer[416..416 + ver_bytes.len().min(31)].copy_from_slice(&ver_bytes[..ver_bytes.len().min(31)]);

    // Offset 384..416: Extension Version
    let ext_ver = b"v0.1.0\0";
    footer[384..384 + ext_ver.len()].copy_from_slice(ext_ver);

    // Offset 352..384: ABI Type (0 = CPP, 1 = C_STRUCT, 2 = C_STRUCT_V0)
    footer[352] = 1;

    let mut out_file = File::create(output).expect("Failed to create output extension file");
    out_file.write_all(&so_bytes).unwrap();
    out_file.write_all(&footer).unwrap();
    println!("Successfully formatted DuckDB extension with 512-byte metadata footer: {}", output);
}

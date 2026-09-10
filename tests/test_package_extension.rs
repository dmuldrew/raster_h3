use std::fs::File;
use std::io::{Read, Write};
use tempfile::tempdir;
use flate2::read::GzDecoder;

#[test]
fn test_duckdb_extension_footer_layout() {
    let dir = tempdir().unwrap();
    let dummy_so_path = dir.path().join("dummy.so");
    let dummy_ext_path = dir.path().join("dummy.duckdb_extension");
    let dummy_gz_path = dir.path().join("dummy.duckdb_extension.gz");

    // Write a dummy library file
    let dummy_payload = vec![0xABu8; 1024];
    {
        let mut f = File::create(&dummy_so_path).unwrap();
        f.write_all(&dummy_payload).unwrap();
    }

    // Run package_extension binary
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("-i")
        .arg(&dummy_so_path)
        .arg("-o")
        .arg(&dummy_ext_path)
        .arg("-p")
        .arg("linux_amd64")
        .arg("-d")
        .arg("v1.2.0")
        .arg("-v")
        .arg("v0.1.0")
        .arg("-a")
        .arg("1")
        .status()
        .expect("Failed to execute package_extension");

    assert!(status.success());
    assert!(dummy_ext_path.exists());

    let mut ext_bytes = Vec::new();
    File::open(&dummy_ext_path).unwrap().read_to_end(&mut ext_bytes).unwrap();
    assert_eq!(ext_bytes.len(), 1024 + 534);

    // Verify original payload is intact
    assert_eq!(&ext_bytes[..1024], &dummy_payload[..]);

    // Inspect 534-byte footer
    let footer = &ext_bytes[1024..];
    assert_eq!(footer.len(), 534);

    // Check WebAssembly custom section header (duckdb_signature)
    assert_eq!(&footer[0..4], &[0, 147, 4, 16]);
    assert_eq!(&footer[4..20], b"duckdb_signature");
    assert_eq!(&footer[20..22], &[128, 4]);

    // Check ABI type at offset 118 (FIELD5)
    let abi_str = std::str::from_utf8(&footer[118..126]).unwrap();
    assert_eq!(abi_str, "C_STRUCT");

    // Check extension version at offset 150 (FIELD4)
    let ext_ver_str = std::str::from_utf8(&footer[150..156]).unwrap();
    assert_eq!(ext_ver_str, "v0.1.0");

    // Check DuckDB version at offset 182 (FIELD3)
    let duck_ver_str = std::str::from_utf8(&footer[182..188]).unwrap();
    assert_eq!(duck_ver_str, "v1.2.0");

    // Check platform at offset 214 (FIELD2)
    let plat_str = std::str::from_utf8(&footer[214..225]).unwrap();
    assert_eq!(plat_str, "linux_amd64");

    // Check magic header signature at offset 246 (FIELD1)
    let magic_str = std::str::from_utf8(&footer[246..248]).unwrap();
    assert_eq!(magic_str, "4\0");

    // Check signature space is zeroed (256 bytes at offset 278..534)
    assert_eq!(footer[278..534].len(), 256);
    assert!(footer[278..534].iter().all(|&b| b == 0));

    // Now test gzip packaging
    let status_gz = std::process::Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("-i")
        .arg(&dummy_so_path)
        .arg("-o")
        .arg(&dummy_gz_path)
        .arg("-p")
        .arg("linux_amd64")
        .arg("-z")
        .status()
        .expect("Failed to execute package_extension with -z");

    assert!(status_gz.success());
    assert!(dummy_gz_path.exists());

    let gz_file = File::open(&dummy_gz_path).unwrap();
    let mut decoder = GzDecoder::new(gz_file);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).unwrap();

    assert_eq!(decompressed.len(), 1024 + 534);
    assert_eq!(decompressed, ext_bytes);
}

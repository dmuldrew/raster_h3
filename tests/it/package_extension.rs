//! Tests the DuckDB extension packaging binary, metadata footer structure,
//! cryptographic RSA-2048 signing (1 MiB chunked SHA-256), signature verification,
//! and DuckDB 2.0 `.well-known/duckdb-extension-repo.json` manifest generation.

use flate2::read::GzDecoder;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::process::Command;
use tempfile::tempdir;

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

    // Run package_extension binary (unsigned)
    let status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .env_remove("DUCKDB_EXTENSION_SIGNING_KEY")
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
    File::open(&dummy_ext_path)
        .unwrap()
        .read_to_end(&mut ext_bytes)
        .unwrap();
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
    let status_gz = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .env_remove("DUCKDB_EXTENSION_SIGNING_KEY")
        .arg("-i")
        .arg(&dummy_so_path)
        .arg("-o")
        .arg(&dummy_gz_path)
        .arg("-p")
        .arg("linux_amd64")
        .arg("-d")
        .arg("v1.2.0")
        .arg("-v")
        .arg("v0.1.0")
        .arg("-a")
        .arg("1")
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

#[test]
fn test_duckdb_extension_signing_verification_and_repo_manifest() {
    let dir = tempdir().unwrap();
    let priv_key = dir.path().join("signing_key.pem");
    let pub_key = dir.path().join("signing_key.pub");

    // Generate a 2048-bit RSA keypair via openssl
    let gen_status = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&priv_key)
        .arg("2048")
        .output()
        .expect("openssl genrsa must be available");
    assert!(gen_status.status.success());

    let pub_status = Command::new("openssl")
        .args(["pkey", "-in"])
        .arg(&priv_key)
        .arg("-pubout")
        .arg("-out")
        .arg(&pub_key)
        .output()
        .expect("openssl pkey -pubout");
    assert!(pub_status.status.success());

    // Create a payload > 1 MiB (1.3 MiB) so the 1 MiB chunked SHA-256 tree hash exercises >1 chunk
    let payload_len = (1024 * 1024) + 300_000;
    let dummy_payload: Vec<u8> = (0..payload_len)
        .map(|i| ((i * 31 + 17) & 0xFF) as u8)
        .collect();
    let dummy_so_path = dir.path().join("libraster_h3.so");
    fs::write(&dummy_so_path, &dummy_payload).unwrap();

    let signed_ext_path = dir.path().join("raster_h3.duckdb_extension");
    let signed_gz_path = dir.path().join("raster_h3.duckdb_extension.gz");
    let manifest_path = dir
        .path()
        .join(".well-known")
        .join("duckdb-extension-repo.json");

    // Package + sign + write repo manifest + verify in one invocation
    let status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("-i")
        .arg(&dummy_so_path)
        .arg("-o")
        .arg(&signed_ext_path)
        .arg("-p")
        .arg("linux_amd64")
        .arg("-d")
        .arg("v1.2.0")
        .arg("-k")
        .arg(&priv_key)
        .arg("-m")
        .arg(&manifest_path)
        .arg("--verify")
        .arg(&signed_ext_path)
        .status()
        .expect("Failed to execute package_extension with signing");
    assert!(status.success());

    // Verify the signature slot is populated (non-zero)
    let ext_bytes = fs::read(&signed_ext_path).unwrap();
    assert_eq!(ext_bytes.len(), payload_len + 534);
    let sig_slice = &ext_bytes[ext_bytes.len() - 256..];
    assert!(sig_slice.iter().any(|&b| b != 0));

    // Independently verify the 1 MiB chunked SHA-256 signature using openssl dgst + pkeyutl
    let unsigned_len = ext_bytes.len() - 256;
    let chunk0 = &ext_bytes[..1024 * 1024];
    let chunk1 = &ext_bytes[1024 * 1024..unsigned_len];
    let chunk0_path = dir.path().join("chunk0.bin");
    let chunk1_path = dir.path().join("chunk1.bin");
    fs::write(&chunk0_path, chunk0).unwrap();
    fs::write(&chunk1_path, chunk1).unwrap();

    let d0 = Command::new("openssl")
        .args(["dgst", "-binary", "-sha256"])
        .arg(&chunk0_path)
        .output()
        .unwrap()
        .stdout;
    let d1 = Command::new("openssl")
        .args(["dgst", "-binary", "-sha256"])
        .arg(&chunk1_path)
        .output()
        .unwrap()
        .stdout;
    assert_eq!(d0.len(), 32);
    assert_eq!(d1.len(), 32);

    let concat_path = dir.path().join("concat.bin");
    let mut concat = Vec::with_capacity(64);
    concat.extend_from_slice(&d0);
    concat.extend_from_slice(&d1);
    fs::write(&concat_path, &concat).unwrap();

    let composite_hash = Command::new("openssl")
        .args(["dgst", "-binary", "-sha256"])
        .arg(&concat_path)
        .output()
        .unwrap()
        .stdout;
    let hash_path = dir.path().join("composite.hash");
    let sig_path = dir.path().join("extracted.sig");
    fs::write(&hash_path, &composite_hash).unwrap();
    fs::write(&sig_path, sig_slice).unwrap();

    let verify_out = Command::new("openssl")
        .args(["pkeyutl", "-verify", "-pubin", "-inkey"])
        .arg(&pub_key)
        .arg("-sigfile")
        .arg(&sig_path)
        .arg("-in")
        .arg(&hash_path)
        .args(["-pkeyopt", "digest:sha256"])
        .output()
        .unwrap();
    assert!(
        verify_out.status.success(),
        "Independent OpenSSL verification failed: {}",
        String::from_utf8_lossy(&verify_out.stderr)
    );

    // Package signed gzipped extension and verify it using standalone `--verify` with `--public-key`
    let gz_status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("-i")
        .arg(&dummy_so_path)
        .arg("-o")
        .arg(&signed_gz_path)
        .arg("-p")
        .arg("linux_amd64")
        .arg("-d")
        .arg("v1.2.0")
        .arg("-k")
        .arg(&priv_key)
        .arg("-z")
        .status()
        .unwrap();
    assert!(gz_status.success());

    let standalone_verify = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("--verify")
        .arg(&signed_gz_path)
        .arg("-K")
        .arg(&pub_key)
        .status()
        .unwrap();
    assert!(standalone_verify.success());

    // Tamper with 1 byte of the signed extension and confirm `--verify` rejects it
    let tampered_path = dir.path().join("tampered.duckdb_extension");
    let mut tampered_bytes = ext_bytes.clone();
    tampered_bytes[42] ^= 0x01;
    fs::write(&tampered_path, &tampered_bytes).unwrap();

    let tampered_verify = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .arg("--verify")
        .arg(&tampered_path)
        .arg("-K")
        .arg(&pub_key)
        .output()
        .unwrap();
    assert!(
        !tampered_verify.status.success(),
        "Tampered extension must fail signature verification"
    );

    // Inspect `.well-known/duckdb-extension-repo.json`
    let manifest_bytes = fs::read(&manifest_path).unwrap();
    assert!(manifest_bytes.len() <= 64 * 1024);
    let parsed: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let keys = parsed["signature_keys"]
        .as_array()
        .expect("signature_keys must be a JSON array");
    assert_eq!(keys.len(), 1);
    let key_str = keys[0].as_str().unwrap();
    let expected_pub = fs::read_to_string(&pub_key)
        .unwrap()
        .replace("\r\n", "\n")
        .trim()
        .to_string();
    assert_eq!(key_str, expected_pub);
}

#[test]
fn test_duckdb_extension_signing_via_inline_env_var() {
    let dir = tempdir().unwrap();
    let priv_key = dir.path().join("env_key.pem");
    let gen_status = Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&priv_key)
        .arg("2048")
        .output()
        .unwrap();
    assert!(gen_status.status.success());

    let inline_pem = fs::read_to_string(&priv_key).unwrap();
    let so_path = dir.path().join("dummy.so");
    let ext_path = dir.path().join("dummy.duckdb_extension");
    fs::write(&so_path, vec![0x5Au8; 2048]).unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .env("DUCKDB_EXTENSION_SIGNING_KEY", &inline_pem)
        .arg("-i")
        .arg(&so_path)
        .arg("-o")
        .arg(&ext_path)
        .arg("--verify")
        .arg(&ext_path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// Packages `dummy.so` without `--platform` and returns the footer's platform tag.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn default_platform_tag(duckdb_version: &str) -> String {
    let dir = tempdir().unwrap();
    let so_path = dir.path().join("dummy.so");
    let ext_path = dir.path().join("dummy.duckdb_extension");
    File::create(&so_path)
        .unwrap()
        .write_all(&[0xABu8; 64])
        .unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_package_extension"))
        .env_remove("DUCKDB_EXTENSION_SIGNING_KEY")
        .arg("-i")
        .arg(&so_path)
        .arg("-o")
        .arg(&ext_path)
        .arg("-d")
        .arg(duckdb_version)
        .status()
        .expect("Failed to execute package_extension");
    assert!(status.success());

    let mut ext_bytes = Vec::new();
    File::open(&ext_path)
        .unwrap()
        .read_to_end(&mut ext_bytes)
        .unwrap();
    let field = &ext_bytes[64 + 214..64 + 246];
    let len = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8(field[..len].to_vec()).unwrap()
}

#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn test_default_platform_follows_duckdb_version_on_linux_x86_64() {
    assert_eq!(default_platform_tag("v1.2.0"), "linux_amd64_gcc4");
    assert_eq!(default_platform_tag("v1.3.0"), "linux_amd64");
    assert_eq!(default_platform_tag("v2.0.0"), "linux_amd64");
}

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

/// DuckDB hashes unsigned extension payloads in 1 MiB (1,048,576-byte) chunks.
const DUCKDB_HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// Total size of the DuckDB extension metadata footer in bytes.
const FOOTER_LEN: usize = 534;

/// Trailing bytes of the footer reserved for the RSA-2048 PKCS#1 v1.5 signature.
const SIGNATURE_LEN: usize = 256;

/// Max size DuckDB 2.0 allows for `.well-known/duckdb-extension-repo.json` (64 KiB).
const MAX_REPO_MANIFEST_BYTES: usize = 64 * 1024;

/// Standard FIPS 180-4 SHA-256 round constants.
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Computes a standard FIPS 180-4 SHA-256 digest in pure Rust.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let process_block = |state: &mut [u32; 8], block: &[u8; 64]| {
        let mut w = [0u32; 64];
        for (i, &chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    };

    let (blocks, rem) = data.as_chunks::<64>();
    for block in blocks {
        process_block(&mut state, block);
    }

    let bit_len = (data.len() as u64) * 8;
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;

    if rem.len() < 56 {
        tail[56..64].copy_from_slice(&bit_len.to_be_bytes());
        process_block(&mut state, tail[..64].try_into().unwrap());
    } else {
        tail[120..128].copy_from_slice(&bit_len.to_be_bytes());
        process_block(&mut state, tail[..64].try_into().unwrap());
        process_block(&mut state, tail[64..128].try_into().unwrap());
    }

    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Computes DuckDB's two-level chunked SHA-256 hash over the unsigned extension bytes
/// (everything preceding the final 256-byte signature slot):
/// 1. Split into 1 MiB chunks and compute the 32-byte SHA-256 digest of each chunk.
/// 2. Concatenate all chunk digests and compute SHA-256 over the concatenated buffer.
fn compute_duckdb_extension_hash(unsigned_bytes: &[u8]) -> [u8; 32] {
    let num_chunks = unsigned_bytes.len().div_ceil(DUCKDB_HASH_CHUNK_BYTES);
    let mut concat_digests = Vec::with_capacity(num_chunks.max(1) * 32);
    if unsigned_bytes.is_empty() {
        concat_digests.extend_from_slice(&sha256(&[]));
    } else {
        for chunk in unsigned_bytes.chunks(DUCKDB_HASH_CHUNK_BYTES) {
            concat_digests.extend_from_slice(&sha256(chunk));
        }
    }
    sha256(&concat_digests)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

/// Materializes a key specification (either a filesystem path or inline PEM text)
/// as a file inside `temp_dir` when needed, returning the path to pass to `openssl`.
fn resolve_key_file(
    key_spec: &str,
    temp_dir: &Path,
    filename: &str,
) -> Result<std::path::PathBuf, String> {
    let trimmed = key_spec.trim();
    if trimmed.starts_with("-----BEGIN ") {
        let key_path = temp_dir.join(filename);
        let mut f = File::create(&key_path)
            .map_err(|e| format!("Failed to create temporary key file: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(fs::Permissions::from_mode(0o600));
        }
        f.write_all(trimmed.as_bytes())
            .and_then(|_| f.write_all(b"\n"))
            .map_err(|e| format!("Failed to write temporary key file: {e}"))?;
        Ok(key_path)
    } else {
        let p = Path::new(trimmed);
        if !p.exists() {
            return Err(format!("Key file '{}' does not exist", trimmed));
        }
        Ok(p.to_path_buf())
    }
}

/// Signs the 32-byte composite extension hash using an RSA-2048 private key via `openssl pkeyutl`.
fn sign_extension_hash(hash: &[u8; 32], signing_key_spec: &str) -> Result<[u8; 256], String> {
    let temp = tempfile::tempdir().map_err(|e| format!("Failed to create tempdir: {e}"))?;
    let key_path = resolve_key_file(signing_key_spec, temp.path(), "signing_key.pem")?;
    let hash_path = temp.path().join("extension.hash");
    let sig_path = temp.path().join("extension.sig");

    fs::write(&hash_path, hash).map_err(|e| format!("Failed to write hash file: {e}"))?;

    let output = Command::new("openssl")
        .arg("pkeyutl")
        .arg("-sign")
        .arg("-in")
        .arg(&hash_path)
        .arg("-inkey")
        .arg(&key_path)
        .args(["-pkeyopt", "digest:sha256", "-out"])
        .arg(&sig_path)
        .output()
        .map_err(|e| format!("Failed to execute `openssl pkeyutl -sign`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "`openssl pkeyutl -sign` failed (status {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let sig_bytes =
        fs::read(&sig_path).map_err(|e| format!("Failed to read generated signature: {e}"))?;
    if sig_bytes.len() != SIGNATURE_LEN {
        return Err(format!(
            "Expected {SIGNATURE_LEN}-byte RSA-2048 signature, got {} bytes (ensure key is 2048-bit RSA)",
            sig_bytes.len()
        ));
    }

    let mut sig = [0u8; SIGNATURE_LEN];
    sig.copy_from_slice(&sig_bytes);
    Ok(sig)
}

/// Extracts the normalized PEM public key and its SHA-256 DER fingerprint (matching
/// DuckDB 2.0's `CREATE EXTENSION REPOSITORY` key fingerprint).
fn extract_public_key_and_fingerprint(
    key_spec: &str,
    is_public: bool,
) -> Result<(String, [u8; 32]), String> {
    let temp = tempfile::tempdir().map_err(|e| format!("Failed to create tempdir: {e}"))?;
    let key_path = resolve_key_file(key_spec, temp.path(), "key.pem")?;

    let mut pem_cmd = Command::new("openssl");
    pem_cmd.arg("pkey");
    if is_public {
        pem_cmd.arg("-pubin");
    }
    let pem_out = pem_cmd
        .arg("-in")
        .arg(&key_path)
        .arg("-pubout")
        .output()
        .map_err(|e| format!("Failed to execute `openssl pkey -pubout`: {e}"))?;

    if !pem_out.status.success() {
        return Err(format!(
            "`openssl pkey -pubout` failed: {}",
            String::from_utf8_lossy(&pem_out.stderr).trim()
        ));
    }

    let pub_pem = String::from_utf8(pem_out.stdout)
        .map_err(|e| format!("Public key PEM is not valid UTF-8: {e}"))?
        .replace("\r\n", "\n")
        .trim()
        .to_string();

    let mut der_cmd = Command::new("openssl");
    der_cmd.arg("pkey");
    if is_public {
        der_cmd.arg("-pubin");
    }
    let der_out = der_cmd
        .arg("-in")
        .arg(&key_path)
        .args(["-pubout", "-outform", "DER"])
        .output()
        .map_err(|e| format!("Failed to execute `openssl pkey -outform DER`: {e}"))?;

    if !der_out.status.success() {
        return Err(format!(
            "`openssl pkey -outform DER` failed: {}",
            String::from_utf8_lossy(&der_out.stderr).trim()
        ));
    }

    let fingerprint = sha256(&der_out.stdout);
    Ok((pub_pem, fingerprint))
}

/// Writes DuckDB 2.0's `.well-known/duckdb-extension-repo.json` repository descriptor.
fn write_repo_manifest(
    manifest_path: &str,
    key_spec: &str,
    is_public: bool,
) -> Result<[u8; 32], String> {
    let (pub_pem, fingerprint) = extract_public_key_and_fingerprint(key_spec, is_public)?;
    let doc = serde_json::json!({
        "signature_keys": [pub_pem],
    });
    let mut json_bytes = serde_json::to_vec_pretty(&doc)
        .map_err(|e| format!("Failed to serialize repository manifest JSON: {e}"))?;
    json_bytes.push(b'\n');

    if json_bytes.len() > MAX_REPO_MANIFEST_BYTES {
        return Err(format!(
            "Repository manifest exceeds DuckDB 64 KiB limit ({} bytes)",
            json_bytes.len()
        ));
    }

    let path = Path::new(manifest_path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "Failed to create parent directory '{}': {e}",
                    parent.display()
                )
            })?;
        }
    }
    fs::write(path, &json_bytes)
        .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    Ok(fingerprint)
}

/// Verifies the 256-byte footer signature of a `.duckdb_extension` or `.duckdb_extension.gz`.
fn verify_extension_signature(
    ext_path: &str,
    key_spec: &str,
    is_public: bool,
) -> Result<[u8; 32], String> {
    let raw = fs::read(ext_path).map_err(|e| format!("Failed to read '{}': {e}", ext_path))?;
    let bytes = if ext_path.ends_with(".gz") || (raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b)
    {
        let mut decoder = GzDecoder::new(&raw[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .map_err(|e| format!("Failed to decompress gzipped extension '{}': {e}", ext_path))?;
        decompressed
    } else {
        raw
    };

    if bytes.len() <= FOOTER_LEN {
        return Err(format!(
            "Extension '{}' is too small ({} bytes; expected > {FOOTER_LEN} bytes)",
            ext_path,
            bytes.len()
        ));
    }

    let footer_start = bytes.len() - FOOTER_LEN;
    let footer = &bytes[footer_start..];
    if footer[0..4] != [0, 147, 4, 16] || &footer[4..20] != b"duckdb_signature" {
        return Err(format!(
            "Extension '{}' is missing the `duckdb_signature` metadata footer",
            ext_path
        ));
    }

    let sig_start = bytes.len() - SIGNATURE_LEN;
    let unsigned = &bytes[..sig_start];
    let signature = &bytes[sig_start..];

    if signature.iter().all(|&b| b == 0) {
        return Err(format!(
            "Extension '{}' has a zeroed signature slot (unsigned)",
            ext_path
        ));
    }

    let hash = compute_duckdb_extension_hash(unsigned);
    let (pub_pem, fingerprint) = extract_public_key_and_fingerprint(key_spec, is_public)?;

    let temp = tempfile::tempdir().map_err(|e| format!("Failed to create tempdir: {e}"))?;
    let pub_path = temp.path().join("pub.pem");
    let hash_path = temp.path().join("ext.hash");
    let sig_path = temp.path().join("ext.sig");

    fs::write(&pub_path, format!("{pub_pem}\n"))
        .map_err(|e| format!("Failed to write public key: {e}"))?;
    fs::write(&hash_path, hash).map_err(|e| format!("Failed to write hash: {e}"))?;
    fs::write(&sig_path, signature).map_err(|e| format!("Failed to write signature: {e}"))?;

    let output = Command::new("openssl")
        .arg("pkeyutl")
        .arg("-verify")
        .arg("-pubin")
        .arg("-inkey")
        .arg(&pub_path)
        .arg("-sigfile")
        .arg(&sig_path)
        .arg("-in")
        .arg(&hash_path)
        .args(["-pkeyopt", "digest:sha256"])
        .output()
        .map_err(|e| format!("Failed to execute `openssl pkeyutl -verify`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "Signature verification failed for '{}': {}",
            ext_path,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(fingerprint)
}

/// Parses `vMAJOR.MINOR[.PATCH]` into `(major, minor)`.
fn parse_duckdb_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn detect_platform(duckdb_version: &str) -> String {
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

    // DuckDB v1.0-v1.2 tag glibc x86_64 builds linux_amd64_gcc4; v1.3+ (including v2.0+) renamed it linux_amd64.
    let gcc4_era = parse_duckdb_version(duckdb_version).is_some_and(|v| v < (1, 3));
    if os == "linux" && arch == "amd64" && gcc4_era {
        return "linux_amd64_gcc4".to_string();
    }

    format!("{}_{}", os, arch)
}

fn print_help() {
    println!(
        "Usage: package_extension [OPTIONS]\n\n\
        Options:\n  \
        -i, --input <PATH>           Path to compiled dynamic library (.so, .dylib, .dll)\n  \
        -o, --output <PATH>          Path to output .duckdb_extension file\n  \
        -p, --platform <NAME>        DuckDB target platform (default: host platform for --duckdb-version)\n  \
        -d, --duckdb-version <VER>   DuckDB C API version written to footer (default: v1.2.0)\n  \
        -v, --ext-version <VER>      Extension version (default: v{})\n  \
        -a, --abi-type <TYPE>        ABI type (0=CPP, 1=C_STRUCT, 2=C_STRUCT_V0; default: 1)\n  \
        -z, --gzip                   Compress output extension with gzip (.gz)\n  \
        -k, --signing-key <PATH>     RSA-2048 private key PEM file for signing (or DUCKDB_EXTENSION_SIGNING_KEY env)\n  \
        -K, --public-key <PATH>      RSA-2048 public key PEM file for verification / repo manifest\n  \
        -m, --repo-manifest <PATH>   Write DuckDB 2.0 `.well-known/duckdb-extension-repo.json`\n  \
            --verify <PATH>          Verify signature of a packaged .duckdb_extension[.gz] file\n  \
        -h, --help                   Print help information\n\n\
        Example:\n  \
        package_extension -i target/release/libraster_h3.so -o target/release/raster_h3.duckdb_extension \\\n    \
            -k signing_key.pem -m dist/.well-known/duckdb-extension-repo.json",
        env!("CARGO_PKG_VERSION")
    );
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
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
    let mut signing_key: Option<String> = non_empty_env("DUCKDB_EXTENSION_SIGNING_KEY");
    let mut public_key: Option<String> = non_empty_env("DUCKDB_EXTENSION_PUBLIC_KEY");
    let mut repo_manifest: Option<String> = None;
    let mut verify_path: Option<String> = None;

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
            "-k" | "--signing-key" => {
                i += 1;
                if i < args.len() {
                    signing_key = Some(args[i].clone());
                }
            }
            "-K" | "--public-key" => {
                i += 1;
                if i < args.len() {
                    public_key = Some(args[i].clone());
                }
            }
            "-m" | "--repo-manifest" => {
                i += 1;
                if i < args.len() {
                    repo_manifest = Some(args[i].clone());
                }
            }
            "--verify" => {
                i += 1;
                if i < args.len() {
                    verify_path = Some(args[i].clone());
                }
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

    // Standalone or combined `--repo-manifest` mode
    if let Some(ref manifest_out) = repo_manifest {
        let (key_spec, is_pub) = if let Some(ref pub_k) = public_key {
            (pub_k.as_str(), true)
        } else if let Some(ref priv_k) = signing_key {
            (priv_k.as_str(), false)
        } else {
            eprintln!("Error: --repo-manifest requires --public-key (-K) or --signing-key (-k)");
            std::process::exit(1);
        };

        let fingerprint = write_repo_manifest(manifest_out, key_spec, is_pub).unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
        println!(
            "Wrote DuckDB 2.0 extension repository manifest: {} (key SHA-256 fingerprint: {})",
            manifest_out,
            hex_encode(&fingerprint)
        );
        if input.is_none() && verify_path.is_none() {
            return;
        }
    }

    // Standalone `--verify` mode (when no `--input` is being packaged first)
    if input.is_none() {
        if let Some(ref target_ext) = verify_path {
            let (key_spec, is_pub) = if let Some(ref pub_k) = public_key {
                (pub_k.as_str(), true)
            } else if let Some(ref priv_k) = signing_key {
                (priv_k.as_str(), false)
            } else {
                eprintln!("Error: --verify requires --public-key (-K) or --signing-key (-k)");
                std::process::exit(1);
            };
            let fingerprint = verify_extension_signature(target_ext, key_spec, is_pub)
                .unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
            println!(
                "Verified DuckDB extension signature: {} (key SHA-256 fingerprint: {})",
                target_ext,
                hex_encode(&fingerprint)
            );
            return;
        }
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
            let stem = in_p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("extension");
            let clean_stem = stem.strip_prefix("lib").unwrap_or(stem);
            let mut out = in_p.with_file_name(format!("{}.duckdb_extension", clean_stem));
            if gzip {
                out.set_extension("duckdb_extension.gz");
            }
            out.to_string_lossy().to_string()
        }
    };

    let target_platform = platform.unwrap_or_else(|| detect_platform(&duckdb_version));

    println!("Packaging DuckDB Extension:");
    println!("  Input:          {}", input_path);
    println!("  Output:         {}", output_path);
    println!("  Platform:       {}", target_platform);
    println!("  DuckDB Version: {}", duckdb_version);
    println!("  Ext Version:    {}", ext_version);
    println!("  ABI Type:       {} (C_STRUCT)", abi_type);
    println!("  Signed:         {}", signing_key.is_some());
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

    // Official DuckDB 534-byte extension footer
    let mut footer = Vec::with_capacity(FOOTER_LEN);

    // 22-byte WebAssembly custom section header: duckdb_signature
    footer.push(0);
    footer.push(147);
    footer.push(4);
    footer.push(16);
    footer.extend_from_slice(b"duckdb_signature");
    footer.push(128);
    footer.push(4);

    let pad_field = |val: &str| -> [u8; 32] {
        let mut buf = [0u8; 32];
        let bytes = val.as_bytes();
        let len = bytes.len().min(32);
        buf[..len].copy_from_slice(&bytes[..len]);
        buf
    };

    let abi_str = match abi_type {
        0 => "CPP",
        1 => "C_STRUCT",
        2 => "C_STRUCT_V0",
        _ => "C_STRUCT",
    };

    // 8 fields of 32 bytes each (256 bytes total; 22 + 256 = 278 bytes before signature)
    footer.extend_from_slice(&pad_field("")); // FIELD8 (unused)
    footer.extend_from_slice(&pad_field("")); // FIELD7 (unused)
    footer.extend_from_slice(&pad_field("")); // FIELD6 (unused)
    footer.extend_from_slice(&pad_field(abi_str)); // FIELD5 (abi_type)
    footer.extend_from_slice(&pad_field(&ext_version)); // FIELD4 (extension_version)
    footer.extend_from_slice(&pad_field(&duckdb_version)); // FIELD3 (duckdb_version)
    footer.extend_from_slice(&pad_field(&target_platform)); // FIELD2 (duckdb_platform)
    footer.extend_from_slice(&pad_field("4")); // FIELD1 (header signature: 4)

    // Append the 278-byte metadata prefix to `so_bytes` before computing the signature hash
    so_bytes.extend_from_slice(&footer);

    // 256 bytes signature space (signed with RSA-2048 if `--signing-key` is provided)
    let signature_bytes = if let Some(ref key_spec) = signing_key {
        let composite_hash = compute_duckdb_extension_hash(&so_bytes);
        let sig = sign_extension_hash(&composite_hash, key_spec).unwrap_or_else(|e| {
            eprintln!("Error signing extension: {e}");
            std::process::exit(1);
        });
        println!("  Composite Hash: {}", hex_encode(&composite_hash));
        sig
    } else {
        [0u8; SIGNATURE_LEN]
    };
    so_bytes.extend_from_slice(&signature_bytes);

    let total_size = so_bytes.len(); // raw_size + 534 bytes

    if gzip {
        let out_file = File::create(&output_path).unwrap_or_else(|e| {
            eprintln!(
                "Error: Failed to create output file '{}': {}",
                output_path, e
            );
            std::process::exit(1);
        });
        let mut encoder = GzEncoder::new(out_file, Compression::default());
        encoder.write_all(&so_bytes).unwrap();
        let written = encoder.finish().unwrap();
        let compressed_size = written.metadata().map(|m| m.len()).unwrap_or(0);
        println!(
            "Successfully packaged gzipped DuckDB extension: {} ({} -> {} bytes, {:.1}% ratio)",
            output_path,
            total_size,
            compressed_size,
            (compressed_size as f64 / total_size as f64) * 100.0
        );
    } else {
        let mut out_file = File::create(&output_path).unwrap_or_else(|e| {
            eprintln!(
                "Error: Failed to create output file '{}': {}",
                output_path, e
            );
            std::process::exit(1);
        });
        out_file.write_all(&so_bytes).unwrap();
        println!(
            "Successfully packaged DuckDB extension: {} (total {} bytes with 534-byte footer)",
            output_path, total_size
        );
    }

    if let Some(ref target_ext) = verify_path {
        let (key_spec, is_pub) = if let Some(ref pub_k) = public_key {
            (pub_k.as_str(), true)
        } else if let Some(ref priv_k) = signing_key {
            (priv_k.as_str(), false)
        } else {
            eprintln!("Error: --verify requires --public-key (-K) or --signing-key (-k)");
            std::process::exit(1);
        };
        let fingerprint =
            verify_extension_signature(target_ext, key_spec, is_pub).unwrap_or_else(|e| {
                eprintln!("Error: {e}");
                std::process::exit(1);
            });
        println!(
            "Verified DuckDB extension signature: {} (key SHA-256 fingerprint: {})",
            target_ext,
            hex_encode(&fingerprint)
        );
    }
}

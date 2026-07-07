//! Author-only tool for the ralphus secure distribution.
//!
//! # Subcommands
//!
//! ```text
//! ralphus-keygen generate [--priv-out PATH] [--pub-out PATH]
//!     Generates a fresh Ed25519 keypair.
//!     Writes the private key (hex seed) to PATH (default: ralphus-private.key).
//!     Writes the 32-byte public key to PATH (default: auth/public.key).
//!     After running this, rebuild with --features secure-dist.
//!
//! ralphus-keygen sign --key PATH --name NAME [--expiry YYYY-MM-DD] [--out PATH]
//!     Signs a license for the named holder using the given private key.
//!     Writes ralphus.lic (or --out path) — ship this alongside the exe.
//! ```

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use serde::Serialize;
use std::path::{Path, PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("generate") => cmd_generate(&args[1..]),
        Some("sign") => cmd_sign(&args[1..]),
        _ => {
            eprintln!(
                "Usage:\n\
                 \n  ralphus-keygen generate [--priv-out PATH] [--pub-out PATH]\
                 \n  ralphus-keygen sign --key PATH --name NAME [--expiry YYYY-MM-DD] [--out PATH]"
            );
            std::process::exit(1);
        }
    }
}

fn cmd_generate(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut priv_out = PathBuf::from("ralphus-private.key");
    let mut pub_out = PathBuf::from("auth/public.key");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--priv-out" => {
                priv_out = PathBuf::from(get_value(args, i, "--priv-out")?);
                i += 2;
            }
            "--pub-out" => {
                pub_out = PathBuf::from(get_value(args, i, "--pub-out")?);
                i += 2;
            }
            flag => return Err(format!("Unknown flag: {flag}").into()),
        }
    }

    if priv_out.exists() {
        return Err(format!(
            "{} already exists — delete it manually if you really want a new keypair.",
            priv_out.display()
        )
        .into());
    }

    let signing_key = SigningKey::generate(&mut OsRng);
    let seed_hex = hex_encode(signing_key.as_bytes());

    // Private key: human-readable text file with a clear header.
    let priv_content = format!("RALPHUS PRIVATE KEY\n{seed_hex}\n");
    std::fs::write(&priv_out, priv_content)?;

    // Public key: raw 32 bytes consumed by `include_bytes!` in auth/src/lib.rs.
    let pub_bytes = signing_key.verifying_key().to_bytes();
    write_creating_dirs(&pub_out, &pub_bytes)?;

    println!("Private key -> {}", priv_out.display());
    println!("Public key  -> {}", pub_out.display());
    println!();
    println!(
        "IMPORTANT: Keep {} secret. Never commit it.",
        priv_out.display()
    );
    println!();
    println!("Next steps:");
    println!(
        "  1. Rebuild with the new key:\n\
         \n       cargo build --release \\\n\
               --features ralphus-daemon/secure-dist,ralphus-librarian/secure-dist"
    );
    println!(
        "  2. Sign a license for each authorized user:\n\
         \n       cargo run -p ralphus-keygen -- sign --key {} --name \"Name\" --expiry YYYY-MM-DD",
        priv_out.display()
    );

    Ok(())
}

fn cmd_sign(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut key_path: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut expiry: Option<String> = None;
    let mut out_path = PathBuf::from("ralphus.lic");

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--key" => {
                key_path = Some(PathBuf::from(get_value(args, i, "--key")?));
                i += 2;
            }
            "--name" => {
                name = Some(get_value(args, i, "--name")?.to_string());
                i += 2;
            }
            "--expiry" => {
                expiry = Some(get_value(args, i, "--expiry")?.to_string());
                i += 2;
            }
            "--out" => {
                out_path = PathBuf::from(get_value(args, i, "--out")?);
                i += 2;
            }
            flag => return Err(format!("Unknown flag: {flag}").into()),
        }
    }

    let key_path = key_path.ok_or("--key <path-to-private-key> is required")?;
    let name = name.ok_or("--name <holder-name> is required")?;

    if let Some(ref exp) = expiry {
        validate_date(exp)?;
    }

    let signing_key = load_signing_key(&key_path)?;

    let message = match &expiry {
        Some(exp) => format!("RALPHUS|{name}|{exp}"),
        None => format!("RALPHUS|{name}|never"),
    };

    use ed25519_dalek::Signer as _;
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = STANDARD.encode(signature.to_bytes());

    #[derive(Serialize)]
    struct LicenseFile<'a> {
        holder: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        expiry: Option<&'a str>,
        signature: String,
    }

    let lic = LicenseFile {
        holder: &name,
        expiry: expiry.as_deref(),
        signature: sig_b64,
    };

    let json = serde_json::to_string_pretty(&lic)?;
    std::fs::write(&out_path, json)?;

    println!("License written -> {}", out_path.display());
    println!("  Holder : {name}");
    match &expiry {
        Some(exp) => println!("  Expires: {exp}"),
        None => println!("  Expires: never"),
    }

    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Cannot read private key at {}: {e}", path.display()))?;

    let seed_hex = content
        .lines()
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or("Private key file does not contain a valid 64-char hex seed.")?;

    let seed_bytes = hex_decode(seed_hex)?;
    let seed: [u8; 32] = seed_bytes.try_into().map_err(|_| "Seed is not 32 bytes.")?;

    Ok(SigningKey::from_bytes(&seed))
}

fn validate_date(s: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Accept YYYY-MM-DD only; we do not validate the calendar value strictly,
    // just the format, since the license check is a string comparison.
    if s.len() != 10
        || !s[..4].chars().all(|c| c.is_ascii_digit())
        || s.as_bytes()[4] != b'-'
        || !s[5..7].chars().all(|c| c.is_ascii_digit())
        || s.as_bytes()[7] != b'-'
        || !s[8..10].chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("Expiry must be YYYY-MM-DD, got: {s}").into());
    }
    Ok(())
}

fn get_value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a str, String> {
    args.get(i + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn write_creating_dirs(path: &Path, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, data)?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("write to String is infallible");
    }
    s
}

fn hex_decode(s: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if s.len() % 2 != 0 {
        return Err("hex string has odd length".into());
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|e| Box::from(e.to_string())))
        .collect()
}

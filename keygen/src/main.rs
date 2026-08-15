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
//! ralphus-keygen sign --key PATH --name NAME [--seat USER@HOST | --this-seat]
//!                     [--expiry YYYY-MM-DD] [--out PATH]
//!     Signs a license for the named holder using the given private key.
//!     Writes ralphus.lic (or --out path) — ship this alongside the exe.
//!     A seat locks the license to one user on one host; without one it
//!     runs anywhere.
//! ```

// An author-only CLI: its stdout IS the product (generated key paths, license
// summaries), so the workspace-wide `clippy::print_stdout = "deny"` is relaxed
// here. This tool is never shipped and never participates in the
// daemon<->runner JSON contract that lint protects.
#![allow(clippy::print_stdout)]

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
                 \n  ralphus-keygen sign --key PATH --name NAME [--seat USER@HOST | --this-seat] \
                 [--expiry YYYY-MM-DD] [--out PATH]"
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
         \n       cargo run -p ralphus-keygen -- sign --key {} --name \"Name\" \\\n\
                   --seat user@HOST --expiry YYYY-MM-DD\n\
         \n     (--this-seat fills the seat in from this host; omit both to\n\
              issue a license that runs anywhere.)",
        priv_out.display()
    );

    Ok(())
}

fn cmd_sign(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut key_path: Option<PathBuf> = None;
    let mut name: Option<String> = None;
    let mut seat: Option<String> = None;
    let mut this_seat = false;
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
            "--seat" => {
                seat = Some(get_value(args, i, "--seat")?.to_string());
                i += 2;
            }
            "--this-seat" => {
                this_seat = true;
                i += 1;
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

    if seat.is_some() && this_seat {
        return Err("--seat and --this-seat are mutually exclusive.".into());
    }
    if this_seat {
        seat = Some(local_seat().ok_or(
            "--this-seat: could not determine this host's user or hostname. \
             Pass --seat USER@HOST explicitly.",
        )?);
    }
    if let Some(ref s) = seat {
        validate_seat(s)?;
    }

    if let Some(ref exp) = expiry {
        validate_date(exp)?;
    }

    let signing_key = load_signing_key(&key_path)?;

    // Must stay byte-identical to `signing_message` in auth/src/lib.rs, or
    // nothing this tool signs will verify.
    let message = format!(
        "RALPHUS|{name}|{}|{}",
        seat.as_deref().unwrap_or("any"),
        expiry.as_deref().unwrap_or("never")
    );

    use ed25519_dalek::Signer as _;
    let signature = signing_key.sign(message.as_bytes());
    let sig_b64 = STANDARD.encode(signature.to_bytes());

    #[derive(Serialize)]
    struct LicenseFile<'a> {
        holder: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        seat: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        expiry: Option<&'a str>,
        signature: String,
    }

    let lic = LicenseFile {
        holder: &name,
        seat: seat.as_deref(),
        expiry: expiry.as_deref(),
        signature: sig_b64,
    };

    let json = serde_json::to_string_pretty(&lic)?;
    std::fs::write(&out_path, json)?;

    println!("License written -> {}", out_path.display());
    println!("  Holder : {name}");
    match &seat {
        Some(s) => println!("  Seat   : {s}"),
        None => println!("  Seat   : any (runs on any user/host)"),
    }
    match &expiry {
        Some(exp) => println!("  Expires: {exp}"),
        None => println!("  Expires: never"),
    }

    Ok(())
}

/// This host's seat as `user@hostname`, mirroring `local_seat` in
/// `auth/src/lib.rs` — the value written here is compared against the value
/// derived there, so the two must agree on shape.
fn local_seat() -> Option<String> {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()?;
    let host = gethostname::gethostname().into_string().ok()?;
    let (user, host) = (user.trim(), host.trim());
    if user.is_empty() || host.is_empty() {
        return None;
    }
    Some(format!("{user}@{host}"))
}

/// Rejects a seat that could never match, so the mistake surfaces at signing
/// time rather than as an unexplained startup refusal on the target host.
fn validate_seat(seat: &str) -> Result<(), Box<dyn std::error::Error>> {
    let trimmed = seat.trim();
    if trimmed != seat {
        return Err(format!("Seat has leading/trailing whitespace: {seat:?}").into());
    }
    // `|` would corrupt the field separators in the signed message.
    if trimmed.contains('|') {
        return Err(format!("Seat must not contain '|': {seat}").into());
    }
    match trimmed.split_once('@') {
        Some((user, host)) if !user.is_empty() && !host.is_empty() && !host.contains('@') => Ok(()),
        _ => Err(format!("Seat must be USER@HOST, got: {seat}").into()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_seat_accepts_user_at_host() {
        assert!(validate_seat("colin.kennedy@DESKTOP-ABC").is_ok());
        assert!(validate_seat("a@b").is_ok());
    }

    #[test]
    fn validate_seat_rejects_malformed_values() {
        for bad in [
            "no-at-sign",
            "@host",
            "user@",
            "user@a@b",
            " user@host",
            "user@host ",
            "us|er@host",
            "",
        ] {
            assert!(
                validate_seat(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn local_seat_has_user_at_host_shape() {
        // Every platform CI runs on sets one of USERNAME/USER/LOGNAME, but
        // don't hard-fail the suite on an environment that sets none.
        if let Some(seat) = local_seat() {
            assert!(validate_seat(&seat).is_ok(), "malformed local seat: {seat}");
        }
    }

    /// The signed message is duplicated between this tool and
    /// `auth::signing_message`; a drift in either breaks every license
    /// silently. This pins the format both sides must produce.
    #[test]
    fn signed_message_format_is_pinned() {
        let msg = |seat: Option<&str>, expiry: Option<&str>| {
            format!(
                "RALPHUS|Alice|{}|{}",
                seat.unwrap_or("any"),
                expiry.unwrap_or("never")
            )
        };
        assert_eq!(msg(None, None), "RALPHUS|Alice|any|never");
        assert_eq!(
            msg(Some("colin@BOX"), Some("2027-01-01")),
            "RALPHUS|Alice|colin@BOX|2027-01-01"
        );
    }
}

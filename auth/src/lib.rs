//! License verification for the `secure-dist` build of ralphus.
//!
//! Without the `secure-dist` feature, `check_license` is always `Ok(())`.
//! With it, the exe refuses to start unless a valid `ralphus.lic` signed by
//! the author's private key is present next to the executable (or at the path
//! in `RALPHUS_LICENSE`).

/// Verifies that a valid Ralphus license is present before the daemon or
/// librarian starts serving.  No-op in non-`secure-dist` builds.
pub fn check_license() -> Result<(), String> {
    #[cfg(feature = "secure-dist")]
    return check();
    #[cfg(not(feature = "secure-dist"))]
    Ok(())
}

// ---------------------------------------------------------------------------
// Everything below is compiled only when the secure-dist feature is on.
// ---------------------------------------------------------------------------

#[cfg(feature = "secure-dist")]
fn check() -> Result<(), String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct LicenseFile {
        holder: String,
        expiry: Option<String>,
        signature: String,
    }

    // 32-byte Ed25519 public key written by `ralphus-keygen generate`.
    // Replace this file by running `cargo run -p ralphus-keygen -- generate`
    // and rebuilding with `--features secure-dist`.
    const PUBLIC_KEY: &[u8; 32] = include_bytes!("../public.key");

    let path = license_path().ok_or_else(|| {
        "No license file found. \
         Place ralphus.lic next to this executable or set RALPHUS_LICENSE."
            .to_string()
    })?;

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Cannot read license at {}: {e}", path.display()))?;

    let lic: LicenseFile =
        serde_json::from_str(&raw).map_err(|e| format!("Malformed license file: {e}"))?;

    if let Some(ref expiry) = lic.expiry {
        let today = today_string();
        if expiry.as_str() < today.as_str() {
            return Err(format!("License expired on {expiry}."));
        }
    }

    let vk = VerifyingKey::from_bytes(PUBLIC_KEY).map_err(|_| {
        "Invalid embedded public key — rebuild with a valid public.key.".to_string()
    })?;

    let sig_bytes = STANDARD
        .decode(&lic.signature)
        .map_err(|_| "Malformed license signature (bad base64).".to_string())?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|_| "Invalid license signature format.".to_string())?;

    let message = signing_message(&lic.holder, lic.expiry.as_deref());
    vk.verify(message.as_bytes(), &sig).map_err(|_| {
        "License signature verification failed. This copy is not authorized.".to_string()
    })
}

#[cfg(feature = "secure-dist")]
fn signing_message(holder: &str, expiry: Option<&str>) -> String {
    match expiry {
        Some(exp) => format!("RALPHUS|{holder}|{exp}"),
        None => format!("RALPHUS|{holder}|never"),
    }
}

#[cfg(feature = "secure-dist")]
fn license_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("RALPHUS_LICENSE") {
        let path = std::path::PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("ralphus.lic");
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Returns today's date as `"YYYY-MM-DD"` using only `std::time`.
/// Uses Howard Hinnant's civil-from-days algorithm (public domain).
#[cfg(feature = "secure-dist")]
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    // Shift epoch from 1970-01-01 to 0000-03-01 (makes leap-day fall at
    // year end, simplifying integer arithmetic).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // day-of-era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // year-of-era [0, 399]
    let base_y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year [0, 365]
    let mp = (5 * doy + 2) / 153; // month-of-year in March-based numbering [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // month [1, 12]
    let y = if m <= 2 { base_y + 1 } else { base_y }; // year
    format!("{y:04}-{m:02}-{d:02}")
}

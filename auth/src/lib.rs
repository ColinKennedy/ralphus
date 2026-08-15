//! License verification for the `secure-dist` build of ralphus.
//!
//! Without the `secure-dist` feature, `check_license` is always `Ok(())`.
//! With it, the exe refuses to start unless a valid `ralphus.lic` signed by
//! the author's private key is present next to the executable (or at the path
//! in `RALPHUS_LICENSE`).
//!
//! A license may additionally be bound to one **seat** — a `user@hostname`
//! pair naming exactly who, on which host, is allowed to run it. A license
//! with no seat runs anywhere.

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
        /// `user@hostname` this license is locked to. Absent = runs anywhere.
        #[serde(default)]
        seat: Option<String>,
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

    let vk = VerifyingKey::from_bytes(PUBLIC_KEY).map_err(|_| {
        "Invalid embedded public key — rebuild with a valid public.key.".to_string()
    })?;

    let sig_bytes = STANDARD
        .decode(&lic.signature)
        .map_err(|_| "Malformed license signature (bad base64).".to_string())?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|_| "Invalid license signature format.".to_string())?;

    // Verify BEFORE reading any other field: `expiry` and `seat` are only
    // trustworthy once the signature covering them has checked out. A forged
    // or hand-edited file must report "not authorized", never "wrong seat".
    let message = signing_message(&lic.holder, lic.seat.as_deref(), lic.expiry.as_deref());
    vk.verify(message.as_bytes(), &sig).map_err(|_| {
        "License signature verification failed. This copy is not authorized.".to_string()
    })?;

    if let Some(ref expiry) = lic.expiry {
        let today = today_string();
        if expiry.as_str() < today.as_str() {
            return Err(format!("License expired on {expiry}."));
        }
    }

    if let Some(ref seat) = lic.seat {
        let local = local_seat().ok_or_else(|| {
            format!(
                "License is bound to seat {seat}, but this host's user or \
                 hostname could not be determined."
            )
        })?;
        if normalize_seat(seat) != normalize_seat(&local) {
            return Err(format!(
                "License is bound to seat {seat}, but this is {local}."
            ));
        }
    }

    Ok(())
}

#[cfg(feature = "secure-dist")]
fn signing_message(holder: &str, seat: Option<&str>, expiry: Option<&str>) -> String {
    format!(
        "RALPHUS|{holder}|{}|{}",
        seat.unwrap_or("any"),
        expiry.unwrap_or("never")
    )
}

/// This host's seat as `user@hostname`, or `None` if either half is
/// unavailable or blank.
///
/// The hostname comes from the OS via `gethostname`; the username comes from
/// the environment (`USERNAME` on Windows, `USER`/`LOGNAME` elsewhere), which
/// a determined user can override. See `docs/secure-dist.md` — seat binding
/// is distribution hygiene, not a tamper-proof boundary.
#[cfg(feature = "secure-dist")]
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

/// Casing- and whitespace-insensitive form used to compare two seats.
/// Windows reports `COMPUTERNAME` uppercase while a hand-written license
/// usually is not, so a literal comparison would reject valid licenses.
#[cfg(feature = "secure-dist")]
fn normalize_seat(seat: &str) -> String {
    seat.trim().to_ascii_lowercase()
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

#[cfg(all(test, feature = "secure-dist"))]
mod tests {
    use super::*;

    /// Pins the exact bytes signed. `ralphus-keygen sign` builds this string
    /// independently — if either side drifts, no license ever verifies again.
    #[test]
    fn signing_message_format_is_pinned() {
        assert_eq!(
            signing_message("Alice", None, None),
            "RALPHUS|Alice|any|never"
        );
        assert_eq!(
            signing_message("Alice", Some("colin@BOX"), Some("2027-01-01")),
            "RALPHUS|Alice|colin@BOX|2027-01-01"
        );
        // A seat-bound, never-expiring license and an unbound, expiring one
        // must not collapse to the same message.
        assert_ne!(
            signing_message("Alice", Some("colin@BOX"), None),
            signing_message("Alice", None, Some("colin@BOX"))
        );
    }

    #[test]
    fn seat_comparison_ignores_case_and_padding() {
        assert_eq!(
            normalize_seat("  Colin.Kennedy@DESKTOP-ABC "),
            normalize_seat("colin.kennedy@desktop-abc")
        );
        assert_ne!(normalize_seat("colin@box-a"), normalize_seat("colin@box-b"));
        assert_ne!(normalize_seat("alice@box"), normalize_seat("bob@box"));
    }

    #[test]
    fn local_seat_is_user_at_host_when_available() {
        if let Some(seat) = local_seat() {
            let (user, host) = seat.split_once('@').expect("seat must contain '@'");
            assert!(!user.is_empty(), "user half empty: {seat}");
            assert!(!host.is_empty(), "host half empty: {seat}");
        }
    }
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

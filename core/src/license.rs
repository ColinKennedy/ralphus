//! Decodes the repo's `LICENSE` text, which `build.rs` embeds into every
//! crate that depends on `ralphus-core` as XOR-obfuscated bytes rather than a
//! plain string, so a `strings`/grep pass over a shipped executable does not
//! surface the license text directly (see RAL-236). This raises the bar
//! against casual tampering only — it is not a cryptographic guarantee.

/// Must match `build.rs::KEY`.
const KEY: &[u8] = b"ralphus-license-obfuscation-key";

const OBFUSCATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/license.obf"));

/// Returns the exact current contents of the repo's `LICENSE` file, decoded
/// from the obfuscated copy embedded at build time.
#[must_use]
pub fn embedded_license() -> String {
    let bytes: Vec<u8> = OBFUSCATED
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KEY[i % KEY.len()])
        .collect();
    String::from_utf8(bytes).expect("embedded LICENSE is valid UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_license_matches_repo_license_file() {
        let repo_license = include_str!("../../LICENSE");
        assert_eq!(embedded_license(), repo_license);
    }

    #[test]
    fn obfuscated_bytes_do_not_contain_plaintext_license_text() {
        // A weak but representative "strings"-style check: a distinctive
        // phrase from LICENSE must not appear verbatim in the obfuscated
        // bytes.
        let needle = b"exclusive property of Colin Kennedy";
        assert!(
            !OBFUSCATED
                .windows(needle.len())
                .any(|window| window == needle),
            "obfuscated LICENSE bytes contain a plaintext-discoverable phrase"
        );
    }
}

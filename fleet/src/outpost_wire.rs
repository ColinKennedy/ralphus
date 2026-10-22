//! The channel between `ralphus-fleet` and a `ralphus-outpost`: mutual
//! authentication from a shared pairing secret, then every message sealed.
//!
//! The outpost connects out to the fleet over plain HTTP(S); nothing on the
//! wire carries the secret or anything derived from it alone:
//!
//! 1. `hello`: the outpost sends its machine name and a random nonce `No`.
//!    The fleet answers with a session id `S`, its own nonce `Nf` and
//!    `HMAC(secret, "fleet" | name | No | Nf | S)` -- which only a holder of
//!    the secret can compute, so the outpost knows it reached the real fleet
//!    before acting on anything.
//! 2. Both derive the session key `HKDF-SHA256(secret, salt = No | Nf,
//!    info = name | S)`. From then on every message body is sealed with
//!    ChaCha20-Poly1305 under that key, its direction and session id bound
//!    in as associated data. The fleet accepts the session (and hands it
//!    work) only once a sealed poll from the outpost opens correctly, which
//!    proves the outpost holds the secret too.
//! 3. Each poll carries a sequence number the fleet's sealed answer repeats,
//!    so an old answer cannot be replayed into a later poll; request ids are
//!    executed and answered at most once per session.
//!
//! A secret must be at least [`MIN_SECRET_LEN`] characters: the hello
//! exchange lets an eavesdropper test guesses offline, so it must be long
//! and random (`openssl rand -hex 32`), never a password.

use ring::{aead, hkdf, hmac};

use base64::Engine as _;

/// The protocol name every hello carries.
pub const PROTOCOL: &str = "ralphus-outpost-v1";
/// The shortest pairing secret either side accepts.
pub const MIN_SECRET_LEN: usize = 32;
/// Associated-data tag of messages the fleet sends.
pub const FLEET_TO_OUTPOST: &str = "fleet-to-outpost";
/// Associated-data tag of messages the outpost sends.
pub const OUTPOST_TO_FLEET: &str = "outpost-to-fleet";

/// Why a pairing secret is unusable, if it is.
///
/// # Errors
/// The secret is shorter than [`MIN_SECRET_LEN`].
pub fn check_secret(secret: &str) -> Result<(), String> {
    if secret.trim().len() < MIN_SECRET_LEN {
        return Err(format!(
            "the pairing secret must be at least {MIN_SECRET_LEN} random characters \
             (generate one with `openssl rand -hex 32`)"
        ));
    }
    Ok(())
}

/// `n` random bytes as lowercase hex.
///
/// # Errors
/// The operating system's random source failed.
pub fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0_u8; n];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `value` is `n` bytes of lowercase hex.
#[must_use]
pub fn is_hex(value: &str, n: usize) -> bool {
    value.len() == n * 2
        && value
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn transcript(
    role: &str,
    name: &str,
    outpost_nonce: &str,
    fleet_nonce: &str,
    session: &str,
) -> String {
    format!("{PROTOCOL}|{role}|{name}|{outpost_nonce}|{fleet_nonce}|{session}")
}

/// The fleet's proof that it holds `secret`, as hex.
#[must_use]
pub fn fleet_proof(
    secret: &str,
    name: &str,
    outpost_nonce: &str,
    fleet_nonce: &str,
    session: &str,
) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.trim().as_bytes());
    let tag = hmac::sign(
        &key,
        transcript("fleet", name, outpost_nonce, fleet_nonce, session).as_bytes(),
    );
    tag.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Check the fleet's proof in constant time.
#[must_use]
pub fn verify_fleet_proof(
    secret: &str,
    name: &str,
    outpost_nonce: &str,
    fleet_nonce: &str,
    session: &str,
    proof_hex: &str,
) -> bool {
    let Some(proof) = from_hex(proof_hex) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.trim().as_bytes());
    hmac::verify(
        &key,
        transcript("fleet", name, outpost_nonce, fleet_nonce, session).as_bytes(),
        &proof,
    )
    .is_ok()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// One session's sealing key.
pub struct SessionKey {
    key: aead::LessSafeKey,
    session: String,
}

impl std::fmt::Debug for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionKey")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl SessionKey {
    /// Derive the key both sides share for `session`.
    ///
    /// # Errors
    /// Key derivation failed (it cannot for these lengths).
    pub fn derive(
        secret: &str,
        name: &str,
        outpost_nonce: &str,
        fleet_nonce: &str,
        session: &str,
    ) -> Result<Self, String> {
        Self::derive_from(
            secret.trim().as_bytes(),
            &format!("{outpost_nonce}{fleet_nonce}"),
            &format!("{PROTOCOL}|session|{name}|{session}"),
            session,
        )
    }

    /// Derive a paired (`v2`) session's key from the X25519 shared secret
    /// (`crate::outpost_keys`).
    ///
    /// # Errors
    /// Key derivation failed (it cannot for these lengths).
    pub fn derive_v2(
        shared: &[u8],
        name: &str,
        outpost_nonce: &str,
        fleet_nonce: &str,
        session: &str,
    ) -> Result<Self, String> {
        Self::derive_from(
            shared,
            &format!("{outpost_nonce}{fleet_nonce}"),
            &format!(
                "{}|session|{name}|{session}",
                crate::outpost_keys::PROTOCOL_V2
            ),
            session,
        )
    }

    fn derive_from(ikm: &[u8], salt: &str, info: &str, session: &str) -> Result<Self, String> {
        let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt.as_bytes());
        let prk = salt.extract(ikm);
        let info = [info.as_bytes()];
        let okm = prk
            .expand(&info, &aead::CHACHA20_POLY1305)
            .map_err(|_| "could not derive the session key".to_string())?;
        Ok(Self {
            key: aead::LessSafeKey::new(aead::UnboundKey::from(okm)),
            session: session.to_string(),
        })
    }

    fn aad(&self, direction: &str) -> String {
        format!("{PROTOCOL}|{direction}|{}", self.session)
    }

    /// Seal `plaintext` for `direction`: base64 of nonce and ciphertext.
    ///
    /// # Errors
    /// No randomness, or sealing failed.
    pub fn seal(&self, direction: &str, plaintext: &[u8]) -> Result<String, String> {
        let mut nonce = [0_u8; aead::NONCE_LEN];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("no randomness: {e}"))?;
        let mut buffer = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                aead::Nonce::assume_unique_for_key(nonce),
                aead::Aad::from(self.aad(direction).as_bytes()),
                &mut buffer,
            )
            .map_err(|_| "could not seal a message".to_string())?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&buffer);
        Ok(base64::engine::general_purpose::STANDARD.encode(out))
    }

    /// Open a message sealed for `direction`.
    ///
    /// # Errors
    /// The message is malformed, was sealed under another key, session or
    /// direction, or was altered.
    pub fn open(&self, direction: &str, sealed: &str) -> Result<Vec<u8>, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(sealed.trim())
            .map_err(|_| "a sealed message is not base64".to_string())?;
        if bytes.len() < aead::NONCE_LEN + aead::CHACHA20_POLY1305.tag_len() {
            return Err("a sealed message is too short".to_string());
        }
        let (nonce, body) = bytes.split_at(aead::NONCE_LEN);
        let nonce = aead::Nonce::try_assume_unique_for_key(nonce)
            .map_err(|_| "a sealed message has a bad nonce".to_string())?;
        let mut body = body.to_vec();
        let plain = self
            .key
            .open_in_place(
                nonce,
                aead::Aad::from(self.aad(direction).as_bytes()),
                &mut body,
            )
            .map_err(|_| "a sealed message did not authenticate".to_string())?;
        Ok(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_proof_verifies_only_with_the_same_secret_and_transcript() {
        let proof = fleet_proof(SECRET, "win", "aa", "bb", "s1");
        assert!(verify_fleet_proof(SECRET, "win", "aa", "bb", "s1", &proof));
        assert!(!verify_fleet_proof(
            "x".repeat(32).as_str(),
            "win",
            "aa",
            "bb",
            "s1",
            &proof
        ));
        assert!(!verify_fleet_proof(
            SECRET, "other", "aa", "bb", "s1", &proof
        ));
        assert!(!verify_fleet_proof(SECRET, "win", "aa", "bb", "s2", &proof));
        assert!(!verify_fleet_proof(SECRET, "win", "aa", "bb", "s1", "zz"));
    }

    #[test]
    fn sealed_messages_open_only_with_the_same_key_session_and_direction() {
        let fleet = SessionKey::derive(SECRET, "win", "aa", "bb", "s1").unwrap();
        let outpost = SessionKey::derive(SECRET, "win", "aa", "bb", "s1").unwrap();
        let sealed = outpost.seal(OUTPOST_TO_FLEET, b"hello").unwrap();
        assert!(!sealed.contains("hello"));
        assert_eq!(fleet.open(OUTPOST_TO_FLEET, &sealed).unwrap(), b"hello");
        assert!(fleet.open(FLEET_TO_OUTPOST, &sealed).is_err(), "reflected");
        let other_session = SessionKey::derive(SECRET, "win", "aa", "bb", "s2").unwrap();
        assert!(other_session.open(OUTPOST_TO_FLEET, &sealed).is_err());
        let wrong_secret = SessionKey::derive(&"y".repeat(32), "win", "aa", "bb", "s1").unwrap();
        assert!(wrong_secret.open(OUTPOST_TO_FLEET, &sealed).is_err());
        let mut tampered = base64::engine::general_purpose::STANDARD
            .decode(&sealed)
            .unwrap();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        let tampered = base64::engine::general_purpose::STANDARD.encode(tampered);
        assert!(fleet.open(OUTPOST_TO_FLEET, &tampered).is_err());
    }

    #[test]
    fn short_secrets_are_refused() {
        assert!(check_secret("hunter2").is_err());
        assert!(check_secret(SECRET).is_ok());
        assert!(is_hex(&random_hex(16).unwrap(), 16));
    }
}

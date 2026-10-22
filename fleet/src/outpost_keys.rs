//! Keys for paired outposts: pairing codes, Ed25519 identities and the
//! `ralphus-outpost-v2` handshake (`docs/outpost.md#trust-model`).
//!
//! **Pairing.** `ralphus-fleet pair <machine>` makes a one-time pairing code
//! (16 Crockford base32 characters, 80 bits, valid for ten minutes). The
//! outpost makes its own Ed25519 key pair and sends the public half with
//! `HMAC(K, "request" | public key | nonce)`, where `K` is derived from the
//! code. The fleet finds the pending code that verifies it, records the
//! machine's public key, forgets the code, and answers with its own public
//! key, the machine's name and `remote_root`, authenticated with the same
//! `K` -- so each side learns the other's public key from someone who knew
//! the code, and nothing on the wire lets an eavesdropper pair (the code is
//! never sent, and 80 bits are too many to guess from a captured exchange).
//!
//! **Connecting.** Each side keeps only its own private key and the other's
//! public key. A hello trades nonces and ephemeral X25519 keys; the fleet
//! signs the transcript (the outpost will not take requests from anyone
//! else), the outpost signs it back in its first poll (the fleet will not
//! hand work to anyone else), and the session key is HKDF-SHA256 of the
//! X25519 shared secret -- so a recorded session cannot be opened later even
//! by someone who steals a key.

use base64::Engine as _;
use ring::rand::SystemRandom;
use ring::signature::{self, KeyPair as _};
use ring::{agreement, digest, hmac};

/// The paired handshake's protocol name.
pub const PROTOCOL_V2: &str = "ralphus-outpost-v2";
/// The pairing exchange's protocol name.
pub const PAIR_PROTOCOL: &str = "ralphus-outpost-pair-v1";
/// How long a pairing code is valid.
pub const PAIRING_TTL_MS: i64 = 10 * 60 * 1000;

const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 16;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A new pairing code: `XXXX-XXXX-XXXX-XXXX`.
///
/// # Errors
/// The OS refused randomness.
pub fn new_pairing_code() -> Result<String, String> {
    let mut bytes = [0_u8; 10];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("no randomness: {e}"))?;
    let mut value = bytes
        .iter()
        .fold(0_u128, |acc, b| (acc << 8) | u128::from(*b));
    let mut chars = Vec::with_capacity(CODE_LEN);
    for _ in 0..CODE_LEN {
        chars.push(CODE_ALPHABET[usize::try_from(value & 31).unwrap_or(0)]);
        value >>= 5;
    }
    let text = String::from_utf8(chars).map_err(|e| e.to_string())?;
    Ok(text
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-"))
}

/// `code` as typed (any case, dashes or spaces, `O`/`I`/`L` for `0`/`1`/`1`)
/// in canonical form, or `None` when it cannot be a pairing code.
#[must_use]
pub fn normalize_pairing_code(code: &str) -> Option<String> {
    let text: String = code
        .chars()
        .filter(|c| !matches!(c, '-' | ' '))
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        })
        .collect();
    (text.len() == CODE_LEN && text.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(text)
}

/// The key a pairing code stands for, as hex. The fleet stores this, not the
/// code; both sides authenticate the pairing exchange with it.
#[must_use]
pub fn pairing_key(normalized_code: &str) -> String {
    hex(digest::digest(
        &digest::SHA256,
        format!("{PAIR_PROTOCOL}|code|{normalized_code}").as_bytes(),
    )
    .as_ref())
}

fn mac(key_hex: &str, message: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &unhex(key_hex).unwrap_or_default());
    hex(hmac::sign(&key, message.as_bytes()).as_ref())
}

fn verify_mac(key_hex: &str, message: &str, mac_hex: &str) -> bool {
    let (Some(key), Some(tag)) = (unhex(key_hex), unhex(mac_hex)) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, &key);
    hmac::verify(&key, message.as_bytes(), &tag).is_ok()
}

fn pair_request_message(machine_public_key: &str, nonce: &str) -> String {
    format!("{PAIR_PROTOCOL}|request|{machine_public_key}|{nonce}")
}

fn pair_answer_message(
    nonce: &str,
    machine_public_key: &str,
    fleet_public_key: &str,
    name: &str,
    remote_root: &str,
) -> String {
    format!(
        "{PAIR_PROTOCOL}|answer|{nonce}|{machine_public_key}|{fleet_public_key}|{name}|{remote_root}"
    )
}

/// The outpost's MAC on its pairing request.
#[must_use]
pub fn pair_request_mac(key_hex: &str, machine_public_key: &str, nonce: &str) -> String {
    mac(key_hex, &pair_request_message(machine_public_key, nonce))
}

/// Whether a pairing request was made by someone holding the code behind
/// `key_hex`.
#[must_use]
pub fn verify_pair_request(
    key_hex: &str,
    machine_public_key: &str,
    nonce: &str,
    mac_hex: &str,
) -> bool {
    verify_mac(
        key_hex,
        &pair_request_message(machine_public_key, nonce),
        mac_hex,
    )
}

/// The fleet's MAC on its pairing answer.
#[must_use]
pub fn pair_answer_mac(
    key_hex: &str,
    nonce: &str,
    machine_public_key: &str,
    fleet_public_key: &str,
    name: &str,
    remote_root: &str,
) -> String {
    mac(
        key_hex,
        &pair_answer_message(
            nonce,
            machine_public_key,
            fleet_public_key,
            name,
            remote_root,
        ),
    )
}

/// Whether a pairing answer came from the fleet that issued the code.
#[must_use]
pub fn verify_pair_answer(
    key_hex: &str,
    nonce: &str,
    machine_public_key: &str,
    fleet_public_key: &str,
    name: &str,
    remote_root: &str,
    mac_hex: &str,
) -> bool {
    verify_mac(
        key_hex,
        &pair_answer_message(
            nonce,
            machine_public_key,
            fleet_public_key,
            name,
            remote_root,
        ),
        mac_hex,
    )
}

/// A long-lived Ed25519 key pair: a machine's credential, or the fleet's
/// identity. Its `Debug` shows only the public fingerprint.
pub struct Identity {
    pair: signature::Ed25519KeyPair,
    pkcs8: Vec<u8>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &fingerprint(&self.public_key()))
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A fresh key pair.
    ///
    /// # Errors
    /// The OS refused randomness.
    pub fn generate() -> Result<Self, String> {
        let document = signature::Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .map_err(|_| "could not generate a key pair".to_string())?;
        Self::from_pkcs8(document.as_ref())
    }

    /// The key pair in `pkcs8`.
    ///
    /// # Errors
    /// Not an Ed25519 PKCS#8 document.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let pair = signature::Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|_| "the stored key is not an Ed25519 key".to_string())?;
        Ok(Self {
            pair,
            pkcs8: pkcs8.to_vec(),
        })
    }

    /// The key pair from [`Self::private_b64`]'s output.
    ///
    /// # Errors
    /// Not base64 of an Ed25519 PKCS#8 document.
    pub fn from_private_b64(text: &str) -> Result<Self, String> {
        Self::from_pkcs8(&unb64(text).ok_or("the stored key is not base64")?)
    }

    /// The private key (PKCS#8, base64). A secret: store it only in a
    /// [`crate::secret_store::SecretStore`].
    #[must_use]
    pub fn private_b64(&self) -> String {
        b64(&self.pkcs8)
    }

    /// The public key, base64.
    #[must_use]
    pub fn public_key(&self) -> String {
        b64(self.pair.public_key().as_ref())
    }

    /// Sign `message`; the signature as base64.
    #[must_use]
    pub fn sign(&self, message: &str) -> String {
        b64(self.pair.sign(message.as_bytes()).as_ref())
    }
}

/// Whether `signature_b64` is `public_key_b64`'s signature of `message`.
#[must_use]
pub fn verify_signature(public_key_b64: &str, message: &str, signature_b64: &str) -> bool {
    let (Some(key), Some(sig)) = (unb64(public_key_b64), unb64(signature_b64)) else {
        return false;
    };
    signature::UnparsedPublicKey::new(&signature::ED25519, key)
        .verify(message.as_bytes(), &sig)
        .is_ok()
}

/// A short, stable name for a public key: `SHA256:<16 hex>`.
#[must_use]
pub fn fingerprint(public_key_b64: &str) -> String {
    let bytes = unb64(public_key_b64).unwrap_or_default();
    let hash = hex(digest::digest(&digest::SHA256, &bytes).as_ref());
    format!("SHA256:{}", &hash[..16])
}

/// Whether `text` is base64 of a 32-byte public key.
#[must_use]
pub fn is_public_key(text: &str) -> bool {
    unb64(text).is_some_and(|b| b.len() == 32)
}

/// One side's ephemeral X25519 key for a single session.
pub struct Ephemeral {
    private: agreement::EphemeralPrivateKey,
    public: String,
}

impl std::fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ephemeral")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

impl Ephemeral {
    /// A fresh ephemeral key.
    ///
    /// # Errors
    /// The OS refused randomness.
    pub fn generate() -> Result<Self, String> {
        let private =
            agreement::EphemeralPrivateKey::generate(&agreement::X25519, &SystemRandom::new())
                .map_err(|_| "could not generate an ephemeral key".to_string())?;
        let public = private
            .compute_public_key()
            .map_err(|_| "could not compute an ephemeral key".to_string())?;
        Ok(Self {
            private,
            public: b64(public.as_ref()),
        })
    }

    /// The public half, base64.
    #[must_use]
    pub fn public(&self) -> &str {
        &self.public
    }

    /// The shared secret with the peer's public half.
    ///
    /// # Errors
    /// The peer's key is malformed.
    pub fn agree(self, peer_b64: &str) -> Result<Vec<u8>, String> {
        let peer = unb64(peer_b64)
            .filter(|b| b.len() == 32)
            .ok_or("the peer's ephemeral key is malformed")?;
        agreement::agree_ephemeral(
            self.private,
            &agreement::UnparsedPublicKey::new(&agreement::X25519, peer),
            <[u8]>::to_vec,
        )
        .map_err(|_| "the key agreement failed".to_string())
    }
}

/// The text both sides sign in a `v2` hello: `role` is `fleet` or `outpost`.
#[must_use]
pub fn transcript_v2(
    role: &str,
    name: &str,
    outpost_nonce: &str,
    outpost_ephemeral: &str,
    fleet_nonce: &str,
    fleet_ephemeral: &str,
    session: &str,
) -> String {
    format!(
        "{PROTOCOL_V2}|{role}|{name}|{outpost_nonce}|{outpost_ephemeral}|{fleet_nonce}|{fleet_ephemeral}|{session}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_are_80_bits_and_forgiving_to_type() {
        let code = new_pairing_code().unwrap();
        assert_eq!(code.len(), 19, "{code}");
        assert_eq!(code.matches('-').count(), 3);
        let normalized = normalize_pairing_code(&code).unwrap();
        assert_eq!(
            normalize_pairing_code(&code.to_lowercase().replace('-', " ")),
            Some(normalized.clone())
        );
        assert_eq!(
            normalize_pairing_code("o1l0-0000-0000-0000").as_deref(),
            Some("0110000000000000")
        );
        assert_eq!(normalize_pairing_code("short"), None);
        assert_eq!(normalize_pairing_code("UUUU-UUUU-UUUU-UUUU"), None);
        assert_ne!(new_pairing_code().unwrap(), code);
    }

    #[test]
    fn a_pairing_exchange_verifies_only_with_the_same_code() {
        let key = pairing_key(&normalize_pairing_code("ABCD-EFGH-JKMN-PQRS").unwrap());
        let other = pairing_key(&normalize_pairing_code("ABCD-EFGH-JKMN-PQRT").unwrap());
        let machine = Identity::generate().unwrap();
        let fleet = Identity::generate().unwrap();
        let mac = pair_request_mac(&key, &machine.public_key(), "n1");
        assert!(verify_pair_request(&key, &machine.public_key(), "n1", &mac));
        assert!(!verify_pair_request(
            &other,
            &machine.public_key(),
            "n1",
            &mac
        ));
        assert!(!verify_pair_request(&key, &fleet.public_key(), "n1", &mac));
        let answer = pair_answer_mac(
            &key,
            "n1",
            &machine.public_key(),
            &fleet.public_key(),
            "win",
            "C:/r",
        );
        assert!(verify_pair_answer(
            &key,
            "n1",
            &machine.public_key(),
            &fleet.public_key(),
            "win",
            "C:/r",
            &answer
        ));
        assert!(!verify_pair_answer(
            &key,
            "n1",
            &machine.public_key(),
            &fleet.public_key(),
            "win",
            "C:/elsewhere",
            &answer
        ));
    }

    #[test]
    fn identities_sign_round_trip_and_hide_their_private_half() {
        let identity = Identity::generate().unwrap();
        let again = Identity::from_private_b64(&identity.private_b64()).unwrap();
        assert_eq!(identity.public_key(), again.public_key());
        let sig = identity.sign("hello");
        assert!(verify_signature(&identity.public_key(), "hello", &sig));
        assert!(!verify_signature(&identity.public_key(), "hullo", &sig));
        let other = Identity::generate().unwrap();
        assert!(!verify_signature(&other.public_key(), "hello", &sig));
        let shown = format!("{identity:?}");
        assert!(!shown.contains(&identity.private_b64()), "{shown}");
        assert!(shown.contains("SHA256:"), "{shown}");
        assert!(is_public_key(&identity.public_key()));
    }

    #[test]
    fn ephemeral_keys_agree() {
        let a = Ephemeral::generate().unwrap();
        let b = Ephemeral::generate().unwrap();
        let (a_public, b_public) = (a.public().to_string(), b.public().to_string());
        assert_eq!(a.agree(&b_public).unwrap(), b.agree(&a_public).unwrap());
        assert!(Ephemeral::generate().unwrap().agree("bad").is_err());
    }
}

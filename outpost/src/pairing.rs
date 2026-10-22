//! The outpost's side of pairing: trade a one-time code from
//! `ralphus-fleet pair` for a long-lived **machine credential**
//! ([`Credential`]): this machine's own Ed25519 key, made here and never
//! sent anywhere, and the fleet's public key, learned from whoever knew the
//! code (`ralphus_fleet::outpost_keys`).

use std::sync::Arc;
use std::time::Duration;

use ralphus_fleet::outpost_keys::{self, Identity, PAIR_PROTOCOL};
use ralphus_fleet::outpost_wire;
use serde_json::{Value, json};

/// What the machine keeps to connect: its key and the fleet's.
#[derive(Clone)]
pub struct Credential {
    pub machine: Arc<Identity>,
    pub fleet_public_key: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("machine", &self.machine)
            .field("fleet", &outpost_keys::fingerprint(&self.fleet_public_key))
            .finish()
    }
}

impl Credential {
    /// The credential as the secret text kept in the OS store.
    #[must_use]
    pub fn to_secret(&self) -> String {
        json!({
            "version": 1,
            "machine_key": self.machine.private_b64(),
            "fleet_public_key": self.fleet_public_key,
        })
        .to_string()
    }

    /// A credential from [`Self::to_secret`]'s text.
    ///
    /// # Errors
    /// The text is not a credential.
    pub fn from_secret(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text)
            .map_err(|_| "the stored machine credential is corrupt".to_string())?;
        let machine =
            Identity::from_private_b64(value["machine_key"].as_str().unwrap_or_default())?;
        let fleet_public_key = value["fleet_public_key"]
            .as_str()
            .filter(|k| outpost_keys::is_public_key(k))
            .ok_or("the stored machine credential has no fleet key")?
            .to_string();
        Ok(Self {
            machine: Arc::new(machine),
            fleet_public_key,
        })
    }
}

/// A completed pairing.
#[derive(Debug, Clone)]
pub struct Paired {
    /// The machine's name in the fleet's configuration.
    pub name: String,
    /// The fleet's `remote_root` for the machine.
    pub remote_root: String,
    pub credential: Credential,
}

/// Pair with the fleet at `fleet_url` using `code`.
///
/// # Errors
/// The code is malformed, refused (wrong, used, expired), or the answer did
/// not come from the fleet that made the code.
pub fn pair(fleet_url: &str, code: &str) -> Result<Paired, String> {
    let normalized = outpost_keys::normalize_pairing_code(code).ok_or(
        "that is not a pairing code: expect 16 letters and digits like ABCD-EFGH-JKMN-PQRS",
    )?;
    let key = outpost_keys::pairing_key(&normalized);
    let machine = Identity::generate()?;
    let machine_key = machine.public_key();
    let nonce = outpost_wire::random_hex(32)?;
    let mac = outpost_keys::pair_request_mac(&key, &machine_key, &nonce);
    let url = format!("{}/v1/outpost/pair", fleet_url.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let answer: Value = match agent
        .post(&url)
        .set("Content-Type", "application/json")
        .send_string(
            &json!({
                "protocol": PAIR_PROTOCOL,
                "machine_public_key": machine_key,
                "nonce": nonce,
                "mac": mac,
            })
            .to_string(),
        ) {
        Ok(response) => serde_json::from_str(&response.into_string().map_err(|e| e.to_string())?)
            .map_err(|e| format!("the fleet sent unparseable JSON: {e}"))?,
        Err(ureq::Error::Status(code, response)) => {
            let text = response.into_string().unwrap_or_default();
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            return Err(format!("the fleet refused to pair (HTTP {code}): {reason}"));
        }
        Err(e) => return Err(format!("could not reach the fleet at {url}: {e}")),
    };
    let name = answer["name"].as_str().unwrap_or_default();
    let remote_root = answer["remote_root"].as_str().unwrap_or_default();
    let fleet_public_key = answer["fleet_public_key"].as_str().unwrap_or_default();
    let answer_mac = answer["mac"].as_str().unwrap_or_default();
    if name.is_empty()
        || remote_root.is_empty()
        || !outpost_keys::is_public_key(fleet_public_key)
        || !outpost_keys::verify_pair_answer(
            &key,
            &nonce,
            &machine_key,
            fleet_public_key,
            name,
            remote_root,
            answer_mac,
        )
    {
        return Err(
            "the answer did not come from the fleet that made this code; not pairing with it"
                .to_string(),
        );
    }
    Ok(Paired {
        name: name.to_string(),
        remote_root: remote_root.to_string(),
        credential: Credential {
            machine: Arc::new(machine),
            fleet_public_key: fleet_public_key.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_round_trips_and_never_shows_its_private_key() {
        let fleet = Identity::generate().unwrap();
        let credential = Credential {
            machine: Arc::new(Identity::generate().unwrap()),
            fleet_public_key: fleet.public_key(),
        };
        let secret = credential.to_secret();
        let again = Credential::from_secret(&secret).unwrap();
        assert_eq!(again.machine.public_key(), credential.machine.public_key());
        assert_eq!(again.fleet_public_key, fleet.public_key());
        let shown = format!("{credential:?}");
        assert!(
            !shown.contains(&credential.machine.private_b64()),
            "{shown}"
        );
        assert!(Credential::from_secret("{}").is_err());
    }
}

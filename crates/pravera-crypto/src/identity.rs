//! The device's long-term ed25519 identity.
//!
//! This key *is* the device. Its public half yields the human-readable
//! [`DeviceId`], and it doubles as the TLS identity during the QUIC handshake,
//! so dialling a device ID and authenticating that device are the same act.
//! A peer cannot be impersonated without the private key.

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pravera_core::{DeviceId, Error, Result};
use zeroize::Zeroizing;

/// A device's secret key plus the public material derived from it.
pub struct Identity {
    signing_key: SigningKey,
}

impl Identity {
    /// Creates a brand-new random identity.
    pub fn generate() -> Self {
        let mut csprng = rand::rngs::OsRng;
        Identity {
            signing_key: SigningKey::generate(&mut csprng),
        }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }

    /// The short name users read aloud and type in.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::from_public_key(&self.public_key())
    }

    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key.sign(message)
    }

    /// The raw private key.
    ///
    /// This is the one thing in Pravera that must never be logged, copied to a
    /// second machine, or sent anywhere. It exists because the QUIC handshake
    /// needs the key material directly: `pravera-transport` hands it to iroh as
    /// the TLS identity, which is what makes dialling a device and
    /// authenticating it the same act.
    ///
    /// Bytes rather than a `SigningKey` deliberately. It keeps `pravera-crypto`
    /// from having to agree with iroh on an ed25519-dalek version, and it makes
    /// the call sites that touch key material trivially greppable.
    ///
    /// The returned value zeroes itself on drop. Do not copy it out.
    pub fn secret_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.signing_key.to_bytes())
    }

    /// Loads the identity from disk, generating and saving one if absent.
    ///
    /// A device's ID must survive restarts, so this is the normal startup path
    /// for the service.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Identity::generate();
                identity.save(path)?;
                Ok(identity)
            }
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            Error::Identity(format!(
                "identity key must be 32 bytes, found {}",
                bytes.len()
            ))
        })?;
        Ok(Identity {
            signing_key: SigningKey::from_bytes(&arr),
        })
    }

    /// Writes the private key, creating parent directories as needed and
    /// restricting permissions to the owner on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let secret = Zeroizing::new(self.signing_key.to_bytes());
        std::fs::write(path, secret.as_slice())?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

impl std::fmt::Debug for Identity {
    /// Never prints key material, so an accidental `{:?}` in a log cannot leak
    /// the device's private key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id())
            .finish_non_exhaustive()
    }
}

/// Checks a signature against a claimed public key.
pub fn verify(public_key: &[u8; 32], message: &[u8], signature: &Signature) -> Result<()> {
    let key = VerifyingKey::from_bytes(public_key)
        .map_err(|e| Error::Identity(format!("malformed public key: {e}")))?;
    key.verify(message, signature)
        .map_err(|_| Error::Identity("signature does not match".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_identity_has_a_usable_device_id() {
        let id = Identity::generate();
        let text = id.device_id().to_string();
        assert!(text.starts_with("PRV-"));
        assert_eq!(text.parse::<DeviceId>().unwrap(), id.device_id());
    }

    #[test]
    fn two_identities_differ() {
        assert_ne!(
            Identity::generate().public_key(),
            Identity::generate().public_key()
        );
    }

    #[test]
    fn signatures_verify_against_the_signing_identity() {
        let id = Identity::generate();
        let msg = b"pravera session challenge";
        let sig = id.sign(msg);
        assert!(verify(&id.public_key(), msg, &sig).is_ok());
    }

    #[test]
    fn a_signature_from_another_key_is_rejected() {
        let alice = Identity::generate();
        let mallory = Identity::generate();
        let msg = b"pravera session challenge";
        let sig = mallory.sign(msg);
        assert!(
            verify(&alice.public_key(), msg, &sig).is_err(),
            "impersonation must fail"
        );
    }

    #[test]
    fn a_tampered_message_is_rejected() {
        let id = Identity::generate();
        let sig = id.sign(b"grant viewer access");
        assert!(verify(&id.public_key(), b"grant admin access", &sig).is_err());
    }

    #[test]
    fn identity_survives_a_round_trip_to_disk() {
        let dir = std::env::temp_dir().join(format!("pravera-test-{}", std::process::id()));
        let path = dir.join("identity.key");
        let _ = std::fs::remove_file(&path);

        let created = Identity::load_or_create(&path).unwrap();
        let reloaded = Identity::load_or_create(&path).unwrap();

        assert_eq!(
            created.device_id(),
            reloaded.device_id(),
            "device ID must be stable across restarts"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_output_never_contains_key_material() {
        let id = Identity::generate();
        let rendered = format!("{id:?}");
        assert!(rendered.contains("device_id"));
        // The secret must not appear in any form.
        let secret_hex: String = id
            .signing_key
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!rendered.contains(&secret_hex));
    }
}

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// Crockford base32: no I, L, O or U, so a device ID read aloud or copied by
/// hand cannot be ambiguous.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 5 bytes of BLAKE3 over the public key. 40 bits encodes to exactly 8 base32
/// characters with no padding, which is what gives the `PRV-XXXX-XXXX` shape.
const ID_BYTES: usize = 5;
const ID_CHARS: usize = 8;

/// A short, human-transcribable name for a device, derived from its ed25519
/// public key.
///
/// The ID is a *convenience for dialing*, never an authentication factor: the
/// public key remains the identity, and a connection is only trusted once the
/// TLS handshake proves possession of the matching private key. A collision in
/// the truncated hash therefore cannot be used to impersonate anyone — it can
/// only fail to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId([u8; ID_BYTES]);

impl DeviceId {
    /// Derive the ID for a device from its ed25519 public key.
    pub fn from_public_key(public_key: &[u8; 32]) -> Self {
        let hash = blake3::hash(public_key);
        let mut bytes = [0u8; ID_BYTES];
        bytes.copy_from_slice(&hash.as_bytes()[..ID_BYTES]);
        DeviceId(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; ID_BYTES] {
        &self.0
    }

    /// The 8 significant characters, without the `PRV-` prefix or separator.
    pub fn to_compact(self) -> String {
        let mut out = String::with_capacity(ID_CHARS);
        let mut acc: u64 = 0;
        for b in self.0 {
            acc = (acc << 8) | b as u64;
        }
        // Emit most-significant group first so the text ordering matches the
        // byte ordering.
        for i in (0..ID_CHARS).rev() {
            let idx = ((acc >> (i * 5)) & 0x1f) as usize;
            out.push(ALPHABET[idx] as char);
        }
        out
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.to_compact();
        write!(f, "PRV-{}-{}", &c[..4], &c[4..])
    }
}

/// Maps a character to its base32 value, folding the Crockford confusables
/// (I/L -> 1, O -> 0) so a mistyped ID still resolves.
fn decode_char(c: char) -> Option<u64> {
    let c = c.to_ascii_uppercase();
    match c {
        'I' | 'L' => Some(1),
        'O' => Some(0),
        _ => ALPHABET
            .iter()
            .position(|&a| a as char == c)
            .map(|p| p as u64),
    }
}

impl FromStr for DeviceId {
    type Err = Error;

    /// Accepts `PRV-4K7M-92XQ`, `prv4k7m92xq`, `4K7M-92XQ` and any other
    /// spacing or casing — everything that is not a base32 digit is ignored,
    /// and a leading `PRV` prefix is stripped.
    fn from_str(s: &str) -> Result<Self> {
        let upper = s.trim().to_ascii_uppercase();
        let body = upper.strip_prefix("PRV").unwrap_or(&upper);

        let digits: Vec<u64> = body
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(decode_char)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                Error::Identity(format!(
                    "`{s}` contains a character that is not valid in a device ID"
                ))
            })?;

        if digits.len() != ID_CHARS {
            return Err(Error::Identity(format!(
                "device ID must have {ID_CHARS} characters, `{s}` has {}",
                digits.len()
            )));
        }

        let acc = digits.iter().fold(0u64, |acc, d| (acc << 5) | d);
        let mut bytes = [0u8; ID_BYTES];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = ((acc >> ((ID_BYTES - 1 - i) * 8)) & 0xff) as u8;
        }
        Ok(DeviceId(bytes))
    }
}

impl serde::Serialize for DeviceId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> serde::Deserialize<'de> for DeviceId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        <[u8; ID_BYTES]>::deserialize(d).map(DeviceId)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> DeviceId {
        DeviceId::from_public_key(&[7u8; 32])
    }

    #[test]
    fn display_has_the_expected_shape() {
        let text = id().to_string();
        assert_eq!(text.len(), 13, "PRV- plus 4 + - + 4");
        assert!(text.starts_with("PRV-"));
        assert_eq!(text.as_bytes()[8], b'-');
    }

    #[test]
    fn round_trips_through_display() {
        let original = id();
        let parsed: DeviceId = original.to_string().parse().unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn parsing_tolerates_user_typing() {
        let canonical = id();
        for variant in [
            canonical.to_string(),
            canonical.to_string().to_lowercase(),
            canonical.to_compact(),
            format!("  {}  ", canonical),
            canonical.to_string().replace('-', " "),
        ] {
            assert_eq!(
                variant.parse::<DeviceId>().unwrap(),
                canonical,
                "input: {variant:?}"
            );
        }
    }

    #[test]
    fn confusable_characters_fold_to_their_digits() {
        // A user who reads "1" as "I" still lands on the same device.
        let with_ones = "PRV-1111-1111".parse::<DeviceId>().unwrap();
        let with_eyes = "PRV-IIII-LLLL".parse::<DeviceId>().unwrap();
        assert_eq!(with_ones, with_eyes);
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert!("PRV-4K7M".parse::<DeviceId>().is_err());
        assert!("PRV-4K7M-92XQ-EXTRA".parse::<DeviceId>().is_err());
    }

    #[test]
    fn distinct_keys_give_distinct_ids() {
        let a = DeviceId::from_public_key(&[1u8; 32]);
        let b = DeviceId::from_public_key(&[2u8; 32]);
        assert_ne!(a, b);
    }
}

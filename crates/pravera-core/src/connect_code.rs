//! Turning a device's public key into something a person can pass along.
//!
//! ## Why a device ID is not enough to dial
//!
//! [`DeviceId`](crate::DeviceId) is five bytes of BLAKE3 over the public key.
//! That is deliberate: it is short enough to read down a phone line, and being
//! one-way means it is safe to say out loud. It also means it cannot be turned
//! back into a key, and a key is what a QUIC dial needs — the whole security
//! model rests on the peer proving possession of the matching private half.
//!
//! So there are two strings, and they do different jobs:
//!
//! - the **connect code**, 52 characters, is the *address*. It is the public
//!   key itself and it is what you paste in to reach a machine.
//! - the **device ID**, `PRV-XXXX-XXXX`, is the *fingerprint*. It is what both
//!   ends compare to confirm they are talking to who they think.
//!
//! Neither is a secret. A public key is public; knowing it lets someone try to
//! connect, and they still face the username and password on arrival.
//!
//! Once mDNS discovery lands in P4, a machine on the same network is picked
//! from a list and neither string has to be typed. The connect code is what
//! makes a session possible before that, and what still works when the two
//! machines are not on the same network at all.

use crate::error::{Error, Result};

/// Crockford base32, matching [`DeviceId`](crate::DeviceId): no I, L, O or U,
/// so nothing in a code is ambiguous when it is written down or read aloud.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 32 bytes is 256 bits, and 256 divided by 5 is 51.2 — so 52 characters, the
/// last of which carries four bits of padding.
const CODE_CHARS: usize = 52;

/// The number of characters between separators in the grouped form.
const GROUP: usize = 4;

/// Render a public key as a connect code.
///
/// Ungrouped and uppercase. Use [`grouped`] for something to put on screen.
pub fn to_code(public_key: &[u8; 32]) -> String {
    let mut out = String::with_capacity(CODE_CHARS);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;

    for byte in public_key {
        acc = (acc << 8) | *byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        // The leftover bits become the top of the final character, with zeros
        // below. `from_code` checks those zeros are still there.
        out.push(ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// A connect code broken into readable groups.
///
/// Purely for display. [`from_code`] ignores separators, so a person can paste
/// back whatever they were shown.
pub fn grouped(public_key: &[u8; 32]) -> String {
    let code = to_code(public_key);
    code.as_bytes()
        .chunks(GROUP)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

/// Read a public key back out of a connect code.
///
/// Forgiving about presentation and strict about content: case, spaces,
/// hyphens and the Crockford confusables (`I`/`L` for `1`, `O` for `0`) are all
/// handled, but a code of the wrong length or with a character that is not
/// base32 is refused rather than padded or truncated into some other machine's
/// key.
pub fn from_code(text: &str) -> Result<[u8; 32]> {
    let digits: Vec<u32> = text
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(decode_char)
        .collect::<std::result::Result<Vec<_>, char>>()
        .map_err(|c| Error::Identity(format!("`{c}` is not valid in a connect code")))?;

    if digits.len() != CODE_CHARS {
        return Err(Error::Identity(format!(
            "a connect code has {CODE_CHARS} characters; this one has {}",
            digits.len()
        )));
    }

    let mut key = [0u8; 32];
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut at = 0;

    for digit in digits {
        acc = (acc << 5) | digit;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            // The final character contributes 4 bits and produces no byte, so
            // this cannot overrun; the check is here because being wrong about
            // that would be a panic in a paste handler.
            if at < key.len() {
                key[at] = ((acc >> bits) & 0xff) as u8;
                at += 1;
            }
        }
    }

    if at != key.len() {
        return Err(Error::Identity("that connect code is incomplete".into()));
    }
    // The last character has four bits of padding that must be zero. A code
    // that fails this was not produced by `to_code`, which means it was
    // mistyped or invented, and either way it names no real device.
    if bits != 0 && (acc & ((1 << bits) - 1)) != 0 {
        return Err(Error::Identity(
            "that connect code has been altered or mistyped".into(),
        ));
    }

    Ok(key)
}

fn decode_char(c: char) -> std::result::Result<u32, char> {
    let upper = c.to_ascii_uppercase();
    match upper {
        'I' | 'L' => Ok(1),
        'O' => Ok(0),
        _ => ALPHABET
            .iter()
            .position(|&a| a as char == upper)
            .map(|p| p as u32)
            .ok_or(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceId;

    fn key(seed: u8) -> [u8; 32] {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8).wrapping_mul(37);
        }
        key
    }

    #[test]
    fn a_key_survives_being_written_down_and_typed_back() {
        for seed in [0u8, 1, 17, 200, 255] {
            let original = key(seed);
            assert_eq!(from_code(&to_code(&original)).unwrap(), original, "{seed}");
            assert_eq!(from_code(&grouped(&original)).unwrap(), original, "{seed}");
        }
    }

    #[test]
    fn a_code_is_the_length_the_documentation_promises() {
        let code = to_code(&key(3));
        assert_eq!(code.len(), CODE_CHARS);
        assert!(code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn the_grouped_form_reads_back_as_the_same_key() {
        let original = key(9);
        let shown = grouped(&original);

        assert!(shown.contains('-'));
        assert_eq!(shown.replace('-', "").len(), CODE_CHARS);
        assert_eq!(from_code(&shown).unwrap(), original);
    }

    #[test]
    fn spacing_and_case_do_not_matter() {
        let original = key(11);
        let code = to_code(&original);

        assert_eq!(from_code(&code.to_lowercase()).unwrap(), original);
        assert_eq!(from_code(&format!("  {code}  ")).unwrap(), original);

        let spaced: String = code
            .as_bytes()
            .chunks(7)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(from_code(&spaced).unwrap(), original);
    }

    #[test]
    fn a_confusable_character_resolves_to_what_the_writer_meant() {
        // Crockford exists so a code copied by hand still works. `1` and `I`
        // look identical in most typefaces, and so do `0` and `O`.
        let original = key(21);
        let code = to_code(&original);
        let mangled = code.replace('1', "I").replace('0', "O");

        assert_eq!(from_code(&mangled).unwrap(), original);
    }

    #[test]
    fn a_truncated_code_is_refused_rather_than_padded() {
        // Padding a short code would produce a valid-looking key for a machine
        // that does not exist, and the failure would surface as an unexplained
        // connection timeout.
        let code = to_code(&key(5));
        assert!(from_code(&code[..40]).is_err());
        assert!(from_code(&format!("{code}ZZZZ")).is_err());
        assert!(from_code("").is_err());
    }

    #[test]
    fn a_character_that_is_not_base32_is_named_in_the_error() {
        let mut code = to_code(&key(7));
        code.replace_range(10..11, "U");

        let error = from_code(&code).expect_err("U is not in the alphabet");
        assert!(error.to_string().contains('U'), "{error}");
    }

    #[test]
    fn a_code_with_a_corrupted_last_character_is_caught() {
        // The final character carries four bits of padding that must be zero.
        // Without this check, 16 different codes would decode to the same key
        // and only one of them would be the one that was shown.
        let original = key(13);
        let code = to_code(&original);

        let mut corrupted: Vec<char> = code.chars().collect();
        let last = corrupted.len() - 1;
        let value = decode_char(corrupted[last]).unwrap();
        corrupted[last] = ALPHABET[(value as usize + 1) % 32] as char;
        let corrupted: String = corrupted.into_iter().collect();

        assert!(
            from_code(&corrupted).is_err(),
            "a code that was not produced by to_code decoded anyway"
        );
    }

    #[test]
    fn two_keys_never_share_a_code() {
        let codes: Vec<String> = (0..64u8).map(|s| to_code(&key(s))).collect();
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), codes.len());
    }

    #[test]
    fn the_code_and_the_device_id_describe_the_same_machine() {
        // The two strings are shown together, and the whole point of showing
        // both is that a person can check they agree.
        let original = key(42);
        let recovered = from_code(&to_code(&original)).unwrap();

        assert_eq!(
            DeviceId::from_public_key(&recovered),
            DeviceId::from_public_key(&original)
        );
    }
}

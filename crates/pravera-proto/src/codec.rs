//! Length-delimited framing for the control stream.
//!
//! QUIC hands us an ordered byte stream, not messages, so every control message
//! is prefixed with its length: four little-endian bytes, then that many bytes
//! of postcard.
//!
//! ## The length prefix is a claim, not a fact
//!
//! It arrives from the network before anything has been authenticated. Two
//! rules follow, and both are enforced here rather than left to callers:
//!
//! 1. A prefix is validated against [`MAX_CONTROL_MESSAGE`] *before* anyone
//!    sizes a buffer from it. `read_exact` into a peer-chosen length is how a
//!    four-byte header turns into a four-gibibyte allocation.
//! 2. A body must be consumed exactly. `postcard::from_bytes` stops as soon as
//!    the type is satisfied and **ignores whatever follows**, so a peer could
//!    pad a valid message with arbitrary bytes and still be accepted.
//!    [`decode`] uses `take_from_bytes` and rejects any remainder, which keeps
//!    the framing and the payload from ever disagreeing about where a message
//!    ends.

use serde::{de::DeserializeOwned, Serialize};

use crate::error::{ProtocolError, Result};

/// Width of the length prefix, in bytes.
pub const LENGTH_PREFIX: usize = 4;

/// Ceiling on a single control message body.
///
/// Control traffic is handshakes, input events and small negotiations; the
/// largest realistic message is a monitor list or a clipboard text blob. One
/// mebibyte is far above anything legitimate and far below anything that
/// threatens the process. Bulk data never travels here: video takes datagrams,
/// files take their own stream with their own chunking.
pub const MAX_CONTROL_MESSAGE: usize = 1 << 20;

/// Serialise `value` into a complete length-prefixed frame.
///
/// Refuses to produce a message the peer would be obliged to reject, so an
/// oversized send fails locally where the cause is visible rather than arriving
/// at the far end as a bare disconnect.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let body = postcard::to_allocvec(value)?;
    if body.is_empty() || body.len() > MAX_CONTROL_MESSAGE {
        return Err(ProtocolError::Malformed);
    }

    let mut framed = Vec::with_capacity(LENGTH_PREFIX + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_le_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

/// Read a length prefix, returning how many body bytes follow.
///
/// Call this before allocating. A zero length is refused because no valid
/// message encodes to nothing: every message is an enum and contributes at
/// least a discriminant byte, so a zero-length body means the sender is
/// confused or probing.
pub fn body_length(prefix: [u8; LENGTH_PREFIX]) -> Result<usize> {
    let length = u32::from_le_bytes(prefix) as usize;
    if length == 0 || length > MAX_CONTROL_MESSAGE {
        return Err(ProtocolError::Malformed);
    }
    Ok(length)
}

/// Deserialise a message body, with the prefix already stripped.
///
/// The body must be consumed exactly. See the module docs for why a remainder
/// is an error rather than something to shrug at.
pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    if body.is_empty() || body.len() > MAX_CONTROL_MESSAGE {
        return Err(ProtocolError::Malformed);
    }
    let (value, rest) = postcard::take_from_bytes::<T>(body)?;
    if !rest.is_empty() {
        return Err(ProtocolError::Malformed);
    }
    Ok(value)
}

/// Deserialise a complete frame, prefix included.
///
/// The convenience form, for a caller holding exactly one whole message. A
/// stream reader wants [`body_length`] and [`decode`] instead, so it can read
/// the prefix and the body as two separate exact reads.
pub fn decode_framed<T: DeserializeOwned>(framed: &[u8]) -> Result<T> {
    let (body, rest) = split_frame(framed)?.ok_or(ProtocolError::Malformed)?;
    if !rest.is_empty() {
        return Err(ProtocolError::Malformed);
    }
    decode(body)
}

/// Split the first complete frame out of a buffer, returning `(body, rest)`.
///
/// `Ok(None)` means the buffer holds only part of a frame and more bytes are
/// needed. An oversized or zero length is `Err`, not `None`: waiting would
/// never resolve it, and treating it as "incomplete" is how a reader ends up
/// buffering forever on a hostile prefix.
pub fn split_frame(buffer: &[u8]) -> Result<Option<(&[u8], &[u8])>> {
    let Some(prefix) = buffer.get(..LENGTH_PREFIX) else {
        return Ok(None);
    };
    let length = body_length(prefix.try_into().expect("slice is LENGTH_PREFIX long"))?;

    let end = LENGTH_PREFIX + length;
    if buffer.len() < end {
        return Ok(None);
    }
    Ok(Some((&buffer[LENGTH_PREFIX..end], &buffer[end..])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ClientMessage, Hello};
    use pravera_core::Codec;

    fn hello() -> ClientMessage {
        ClientMessage::Hello(Hello {
            version: crate::VERSION,
            client_name: "laptop".into(),
            codecs: vec![Codec::H264],
        })
    }

    #[test]
    fn a_message_survives_the_round_trip_through_a_frame() {
        let framed = encode(&hello()).unwrap();
        assert_eq!(decode_framed::<ClientMessage>(&framed).unwrap(), hello());
    }

    #[test]
    fn the_prefix_says_exactly_how_long_the_body_is() {
        let framed = encode(&hello()).unwrap();
        let prefix: [u8; LENGTH_PREFIX] = framed[..LENGTH_PREFIX].try_into().unwrap();
        assert_eq!(body_length(prefix).unwrap(), framed.len() - LENGTH_PREFIX);
    }

    #[test]
    fn a_hostile_length_is_refused_before_anything_is_allocated() {
        // Four bytes on the wire claiming four gibibytes. It has to fail at the
        // prefix, because the next thing a reader does is size a buffer.
        assert_eq!(
            body_length([0xff, 0xff, 0xff, 0xff]),
            Err(ProtocolError::Malformed)
        );
        assert_eq!(
            body_length((MAX_CONTROL_MESSAGE as u32 + 1).to_le_bytes()),
            Err(ProtocolError::Malformed)
        );
        assert!(body_length((MAX_CONTROL_MESSAGE as u32).to_le_bytes()).is_ok());
    }

    #[test]
    fn a_zero_length_message_is_refused() {
        assert_eq!(body_length([0, 0, 0, 0]), Err(ProtocolError::Malformed));
        assert_eq!(decode::<ClientMessage>(&[]), Err(ProtocolError::Malformed));
    }

    #[test]
    fn padding_appended_to_a_valid_body_is_refused() {
        // postcard stops as soon as the type is satisfied and says nothing
        // about the rest. Without the take_from_bytes check every one of these
        // would decode happily, and the length prefix would be a lie.
        let framed = encode(&hello()).unwrap();
        let body = &framed[LENGTH_PREFIX..];

        for junk in [&[0u8][..], &[0xff, 0xff][..], &[0x41; 64][..]] {
            let mut padded = body.to_vec();
            padded.extend_from_slice(junk);
            assert_eq!(
                decode::<ClientMessage>(&padded),
                Err(ProtocolError::Malformed),
                "{} trailing bytes were accepted",
                junk.len()
            );
        }
    }

    #[test]
    fn a_truncated_body_is_refused_rather_than_half_decoded() {
        let framed = encode(&hello()).unwrap();
        let body = &framed[LENGTH_PREFIX..];
        for cut in 1..body.len() {
            assert!(
                decode::<ClientMessage>(&body[..cut]).is_err(),
                "a body cut to {cut} bytes decoded anyway"
            );
        }
    }

    #[test]
    fn an_incomplete_frame_asks_for_more_bytes_instead_of_failing() {
        let framed = encode(&hello()).unwrap();
        for cut in 0..framed.len() {
            assert_eq!(
                split_frame(&framed[..cut]),
                Ok(None),
                "a frame cut to {cut} bytes should be incomplete, not an error"
            );
        }
        assert!(split_frame(&framed).unwrap().is_some());
    }

    #[test]
    fn a_hostile_prefix_fails_immediately_instead_of_buffering_forever() {
        // Were this Ok(None), a reader would sit waiting for four gibibytes
        // that are never coming, holding the connection open while it waits.
        let mut buffer = vec![0xff, 0xff, 0xff, 0xff];
        buffer.extend_from_slice(b"whatever");
        assert_eq!(split_frame(&buffer), Err(ProtocolError::Malformed));
    }

    #[test]
    fn frames_split_off_one_at_a_time_leaving_the_rest_intact() {
        // Two messages arriving in one read is ordinary on a stream, and the
        // second must not be lost or folded into the first.
        let mut buffer = encode(&hello()).unwrap();
        buffer.extend_from_slice(&encode(&ClientMessage::Ping { nonce: 9 }).unwrap());

        let (first, rest) = split_frame(&buffer).unwrap().unwrap();
        assert_eq!(decode::<ClientMessage>(first).unwrap(), hello());

        let (second, tail) = split_frame(rest).unwrap().unwrap();
        assert_eq!(
            decode::<ClientMessage>(second).unwrap(),
            ClientMessage::Ping { nonce: 9 }
        );
        assert!(tail.is_empty());
    }

    #[test]
    fn decode_framed_refuses_a_buffer_with_a_second_message_in_it() {
        // This form is for a caller holding one message. Silently dropping a
        // trailing message here would drop input events on the floor.
        let mut buffer = encode(&hello()).unwrap();
        buffer.extend_from_slice(&encode(&ClientMessage::Ping { nonce: 1 }).unwrap());
        assert_eq!(
            decode_framed::<ClientMessage>(&buffer),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn encoding_refuses_to_produce_something_the_peer_must_reject() {
        let oversized = ClientMessage::Goodbye {
            reason: "x".repeat(MAX_CONTROL_MESSAGE + 1),
        };
        assert_eq!(encode(&oversized), Err(ProtocolError::Malformed));
    }

    #[test]
    fn noise_does_not_decode_into_a_message() {
        // Not a proof, but it catches a decoder that accepts anything. Most of
        // the byte space is an out-of-range enum discriminant.
        let mut accepted = 0;
        for seed in 0u32..2000 {
            let noise: Vec<u8> = (0..16u32)
                .map(|i| (seed.wrapping_mul(2_654_435_761).wrapping_add(i * 97) >> 3) as u8)
                .collect();
            if decode::<ClientMessage>(&noise).is_ok() {
                accepted += 1;
            }
        }
        assert_eq!(
            accepted, 0,
            "{accepted} noise buffers decoded as a valid message"
        );
    }
}

//! Media chunking and reassembly for unreliable datagrams.
//!
//! An encoded video frame is usually larger than one datagram, so it is split
//! across several and put back together at the far end. Chunks may arrive out
//! of order, twice, or not at all: none of those is an error, they are the
//! normal weather on an unreliable channel. What *is* an error is a chunk whose
//! header contradicts itself, and this module is strict about those.
//!
//! ## The header is hand-laid, not postcard
//!
//! Control messages get a derive. This one does not. It is written and parsed
//! by hand because it sits in the hot path on every packet of a 144 fps stream,
//! and because a fixed byte layout should be visible in the source rather than
//! implied by field order in a struct definition.
//!
//! ```text
//! offset  size  field
//!   0      4    frame_id        u32 le
//!   4      4    capture_micros  u32 le
//!   8      2    chunk_index     u16 le
//!  10      2    chunk_count     u16 le
//!  12      1    monitor         u8
//!  13      1    flags           u8
//!  14      ..   payload
//! ```
//!
//! ## One reassembler per media stream
//!
//! Frame IDs are only unique within a stream. Video and audio number their
//! frames independently, so pushing both into one [`Reassembler`] would collide
//! their IDs and splice audio into video. Route on [`ChunkFlags::AUDIO`] first,
//! then push into the reassembler for that stream.

use std::collections::{HashMap, VecDeque};

use bitflags::bitflags;

use crate::control::MonitorId;
use crate::error::{ProtocolError, Result};

/// UDP payload every path is assumed to carry without fragmenting.
///
/// The IPv6 minimum MTU is 1280; subtract the IPv6 and UDP headers and 1200 is
/// the number every QUIC implementation converges on.
const ASSUMED_UDP_PAYLOAD: usize = 1200;

/// QUIC's own overhead inside that UDP payload.
///
/// A QUIC DATAGRAM frame does not get the whole packet. Ahead of it sit the
/// short-header flags, the destination connection ID, and the packet number;
/// behind it, the AEAD tag; and the frame itself has a type byte and a length.
///
/// This was 38 bytes measured against iroh 1.0 on loopback, which the loopback
/// integration test found by asserting the wrong thing first: the protocol had
/// been budgeting the full 1200 and every media send would have failed at
/// runtime. 100 is the reservation, so a longer connection ID after migration
/// or an extra coalesced frame cannot overflow it.
const QUIC_OVERHEAD: usize = 100;

/// Bytes available to one media datagram.
///
/// Deliberately not tuned upward for the direct-link case: a path that carries
/// more is a bonus, and exploiting it belongs to path MTU discovery rather than
/// to a constant. [`crate::frame::MAX_CHUNK_PAYLOAD`] is what a sender may put
/// in one chunk; a session should still check the figure its own path reports.
pub const SAFE_DATAGRAM: usize = ASSUMED_UDP_PAYLOAD - QUIC_OVERHEAD;

/// Most chunks one frame may be split into.
///
/// Bounds memory. Without it, a peer could claim `chunk_count = 65535` and make
/// the reassembler hold 71 MiB for a frame it never finishes; with several such
/// frames in flight it is a memory-exhaustion attack costing the attacker a few
/// hundred bytes. 2048 chunks is roughly a 2.1 MiB frame, comfortably above a
/// 4K keyframe at a high bitrate.
pub const MAX_CHUNKS_PER_FRAME: u16 = 2048;

bitflags! {
    /// Per-chunk header flags.
    ///
    /// Every chunk of a frame carries the same flags. The reassembler enforces
    /// that, so a peer cannot mark one chunk of a frame as audio and have the
    /// meaning of the assembled frame depend on arrival order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct ChunkFlags: u8 {
        /// A keyframe: decodable on its own, no earlier frame required.
        const KEYFRAME = 1 << 0;
        /// The payload is Opus audio rather than video.
        const AUDIO    = 1 << 1;
    }
}

/// The fixed header on every media datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub frame_id: u32,
    /// Microseconds since the session started, sampled at capture.
    ///
    /// Relative to the session rather than the wall clock, and 32 bits, which
    /// wraps at roughly 71 minutes. That is fine for its two jobs, frame pacing
    /// and the latency breakdown, which both care about differences between
    /// nearby frames and not about absolute time.
    pub capture_micros: u32,
    pub chunk_index: u16,
    pub chunk_count: u16,
    pub monitor: MonitorId,
    pub flags: ChunkFlags,
}

impl ChunkHeader {
    /// Wire size of the header, in bytes.
    pub const SIZE: usize = 14;

    pub fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..4].copy_from_slice(&self.frame_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.capture_micros.to_le_bytes());
        out[8..10].copy_from_slice(&self.chunk_index.to_le_bytes());
        out[10..12].copy_from_slice(&self.chunk_count.to_le_bytes());
        out[12] = self.monitor.0;
        out[13] = self.flags.bits();
        out
    }

    /// Parse a header and check it against itself.
    ///
    /// Every field here came off the network. The checks are the ones whose
    /// absence would let a header lie about the shape of the frame it belongs
    /// to: a count of zero (a frame with no chunks), an index past the end (an
    /// out-of-bounds write into the slot vector), a count above the cap (a
    /// memory claim), or an undefined flag bit (a sender we do not understand).
    pub fn parse(bytes: &[u8]) -> Result<ChunkHeader> {
        let bytes: &[u8; Self::SIZE] = bytes
            .get(..Self::SIZE)
            .ok_or(ProtocolError::Malformed)?
            .try_into()
            .expect("sized");

        let chunk_count = u16::from_le_bytes([bytes[10], bytes[11]]);
        let chunk_index = u16::from_le_bytes([bytes[8], bytes[9]]);

        if chunk_count == 0 || chunk_count > MAX_CHUNKS_PER_FRAME || chunk_index >= chunk_count {
            return Err(ProtocolError::Malformed);
        }

        let flags = ChunkFlags::from_bits(bytes[13]).ok_or(ProtocolError::Malformed)?;

        Ok(ChunkHeader {
            frame_id: u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            capture_micros: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            chunk_index,
            chunk_count,
            monitor: MonitorId(bytes[12]),
            flags,
        })
    }
}

/// Payload bytes that fit alongside a header in one datagram.
pub const MAX_CHUNK_PAYLOAD: usize = SAFE_DATAGRAM - ChunkHeader::SIZE;

/// Largest frame that can be carried, in bytes.
pub const MAX_FRAME_BYTES: usize = MAX_CHUNKS_PER_FRAME as usize * MAX_CHUNK_PAYLOAD;

/// Per-frame metadata, identical on every chunk of that frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameMeta {
    pub frame_id: u32,
    pub capture_micros: u32,
    pub monitor: MonitorId,
    pub flags: ChunkFlags,
}

/// A reassembled media frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub meta: FrameMeta,
    pub data: Vec<u8>,
}

impl Frame {
    pub fn is_keyframe(&self) -> bool {
        self.meta.flags.contains(ChunkFlags::KEYFRAME)
    }

    pub fn is_audio(&self) -> bool {
        self.meta.flags.contains(ChunkFlags::AUDIO)
    }
}

/// Split an encoded frame into datagrams.
///
/// Allocates one `Vec` per chunk, which is the wrong shape for the hot path and
/// is deliberate for now: P1 is a walking skeleton, and a buffer pool belongs
/// with the rest of the zero-copy work in P7. The wire format does not change
/// when that lands.
pub fn split(meta: FrameMeta, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
    split_with(meta, payload, MAX_CHUNK_PAYLOAD)
}

/// Split with an explicit per-chunk payload cap, for paths whose MTU is
/// smaller than [`MAX_CHUNK_PAYLOAD`] (relayed QUIC paths).
///
/// `max_payload` is clamped to `[1, MAX_CHUNK_PAYLOAD]`. The receiver accepts
/// any `<= MAX_CHUNK_PAYLOAD` (`Reassembler::push`), so smaller chunks remain
/// valid — no wire change. Callers should query
/// `session.max_chunk_payload()` per frame and use
/// `min(MAX_CHUNK_PAYLOAD, live_limit)`.
pub fn split_with(meta: FrameMeta, payload: &[u8], max_payload: usize) -> Result<Vec<Vec<u8>>> {
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::Malformed);
    }
    let cap = max_payload.clamp(1, MAX_CHUNK_PAYLOAD);

    let chunk_count = payload.len().div_ceil(cap) as u16;

    Ok(payload
        .chunks(cap)
        .enumerate()
        .map(|(index, slice)| {
            let header = ChunkHeader {
                frame_id: meta.frame_id,
                capture_micros: meta.capture_micros,
                chunk_index: index as u16,
                chunk_count,
                monitor: meta.monitor,
                flags: meta.flags,
            };
            let mut datagram = Vec::with_capacity(ChunkHeader::SIZE + slice.len());
            datagram.extend_from_slice(&header.to_bytes());
            datagram.extend_from_slice(slice);
            datagram
        })
        .collect())
}

/// How many partial frames a [`Reassembler`] holds before evicting the oldest.
///
/// Four is enough to absorb the reordering a real path produces while bounding
/// the reassembler at roughly 8.5 MiB in the worst case.
pub const DEFAULT_MAX_PENDING: usize = 4;

/// True when `a` is later than `b` in a wrapping sequence space.
///
/// Frame IDs wrap after about 345 days at 144 fps. Comparing them with `>`
/// would, at that moment, treat every subsequent frame as ancient and drop the
/// stream permanently. Half the space is the standard cut, the same rule RTP
/// uses for sequence numbers.
fn is_newer(a: u32, b: u32) -> bool {
    let delta = a.wrapping_sub(b);
    delta != 0 && delta < u32::MAX / 2
}

struct Partial {
    meta: FrameMeta,
    chunks: Vec<Option<Vec<u8>>>,
    received: u16,
    bytes: usize,
}

/// Collects chunks into whole frames.
///
/// Holds at most [`DEFAULT_MAX_PENDING`] incomplete frames; a new frame beyond
/// that evicts the one whose first chunk arrived longest ago. Eviction is what
/// makes a peer that opens frames and never finishes them merely wasteful
/// rather than fatal.
pub struct Reassembler {
    pending: HashMap<u32, Partial>,
    /// Frame IDs in first-seen order, so eviction has something to pick.
    arrival: VecDeque<u32>,
    max_pending: usize,
    newest_completed: Option<u32>,
    dropped_incomplete: u64,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_PENDING)
    }

    /// `max_pending` is clamped to at least one, because a reassembler that
    /// holds nothing can never complete a multi-chunk frame.
    pub fn with_capacity(max_pending: usize) -> Self {
        Self {
            pending: HashMap::new(),
            arrival: VecDeque::new(),
            max_pending: max_pending.max(1),
            newest_completed: None,
            dropped_incomplete: 0,
        }
    }

    /// Feed one datagram.
    ///
    /// Returns the frame when this chunk completed it. `Ok(None)` covers every
    /// ordinary outcome: the frame is still incomplete, the chunk was a
    /// duplicate, or it belonged to a frame already emitted. `Err` is reserved
    /// for a header that contradicts itself or its siblings.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Option<Frame>> {
        let header = ChunkHeader::parse(datagram)?;
        let payload = &datagram[ChunkHeader::SIZE..];
        if payload.is_empty() || payload.len() > MAX_CHUNK_PAYLOAD {
            return Err(ProtocolError::Malformed);
        }

        // A chunk for a frame already emitted. Late duplicates are ordinary on
        // an unreliable path, and re-admitting one would resurrect a finished
        // frame and emit it a second time.
        if let Some(newest) = self.newest_completed {
            if !is_newer(header.frame_id, newest) {
                return Ok(None);
            }
        }

        let meta = FrameMeta {
            frame_id: header.frame_id,
            capture_micros: header.capture_micros,
            monitor: header.monitor,
            flags: header.flags,
        };

        match self.pending.get(&header.frame_id) {
            Some(partial) => {
                // Every chunk of a frame must describe the same frame. A
                // mismatch means either a broken sender or someone splicing
                // chunks between frames to change what the assembled frame
                // means.
                if partial.meta != meta || partial.chunks.len() != header.chunk_count as usize {
                    return Err(ProtocolError::Malformed);
                }
            }
            None => {
                if self.pending.len() >= self.max_pending {
                    self.evict_oldest();
                }
                self.pending.insert(
                    header.frame_id,
                    Partial {
                        meta,
                        chunks: vec![None; header.chunk_count as usize],
                        received: 0,
                        bytes: 0,
                    },
                );
                self.arrival.push_back(header.frame_id);
            }
        }

        let partial = self
            .pending
            .get_mut(&header.frame_id)
            .expect("just inserted or matched");
        let slot = &mut partial.chunks[header.chunk_index as usize];
        if slot.is_some() {
            return Ok(None); // duplicate; idempotent
        }
        *slot = Some(payload.to_vec());
        partial.received += 1;
        partial.bytes += payload.len();

        if partial.received != header.chunk_count {
            return Ok(None);
        }

        let partial = self.remove(header.frame_id).expect("present");
        let mut data = Vec::with_capacity(partial.bytes);
        for chunk in partial.chunks {
            data.extend_from_slice(&chunk.expect("every slot filled once received == count"));
        }

        self.newest_completed = Some(header.frame_id);
        Ok(Some(Frame {
            meta: partial.meta,
            data,
        }))
    }

    /// Incomplete frames currently held.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Frames evicted before they completed, over the life of this reassembler.
    ///
    /// A real measurement, which is the only kind the interface is allowed to
    /// show: this is the loss figure behind a stuttering stream.
    pub fn dropped_incomplete(&self) -> u64 {
        self.dropped_incomplete
    }

    /// Forget everything. Used when a session restarts and frame IDs begin
    /// again from zero, which would otherwise all look like ancient history.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.arrival.clear();
        self.newest_completed = None;
    }

    fn evict_oldest(&mut self) {
        while let Some(id) = self.arrival.pop_front() {
            if self.pending.remove(&id).is_some() {
                self.dropped_incomplete += 1;
                return;
            }
        }
    }

    fn remove(&mut self, frame_id: u32) -> Option<Partial> {
        if let Some(at) = self.arrival.iter().position(|&id| id == frame_id) {
            self.arrival.remove(at);
        }
        self.pending.remove(&frame_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(frame_id: u32) -> FrameMeta {
        FrameMeta {
            frame_id,
            capture_micros: 1_234,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::KEYFRAME,
        }
    }

    #[test]
    fn an_audio_packet_never_has_to_be_reassembled() {
        // The reason an audio packet is five milliseconds rather than ten or
        // twenty. One packet, one datagram: a lost datagram then costs exactly
        // its own five milliseconds. Split across two, either loss would
        // silence the whole packet and double the audible cost.
        let pcm = pravera_core::AudioFormat::new(pravera_core::AudioCodec::Pcm16).packet_bytes();
        assert!(
            pcm <= MAX_CHUNK_PAYLOAD,
            "{pcm} bytes of PCM needs {} chunks",
            pcm.div_ceil(MAX_CHUNK_PAYLOAD)
        );

        let chunks = split(
            FrameMeta {
                flags: ChunkFlags::AUDIO,
                ..meta(1)
            },
            &vec![0u8; pcm],
        )
        .expect("split");
        assert_eq!(chunks.len(), 1);
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn the_header_layout_is_pinned_to_these_exact_bytes() {
        // The one test that must fail if anyone reorders a field. Both ends
        // parse by offset, so a silent layout change is a silent wire break.
        // The index and count are picked to be valid as well as distinctive:
        // 266 < 779 <= MAX_CHUNKS_PER_FRAME, so the same fixture also proves
        // the layout survives a parse.
        let header = ChunkHeader {
            frame_id: 0x0403_0201,
            capture_micros: 0x0807_0605,
            chunk_index: 0x010a,
            chunk_count: 0x030b,
            monitor: MonitorId(0x0d),
            flags: ChunkFlags::KEYFRAME | ChunkFlags::AUDIO,
        };
        assert_eq!(ChunkHeader::SIZE, 14);
        assert_eq!(
            header.to_bytes(),
            [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0a, 0x01, 0x0b, 0x03, 0x0d, 0x03]
        );
        assert_eq!(ChunkHeader::parse(&header.to_bytes()).unwrap(), header);
    }

    #[test]
    fn a_chunk_and_its_header_fit_in_one_safe_datagram() {
        assert_eq!(MAX_CHUNK_PAYLOAD + ChunkHeader::SIZE, SAFE_DATAGRAM);
    }

    #[test]
    fn a_frame_survives_the_trip_through_datagrams() {
        let original = payload(MAX_CHUNK_PAYLOAD * 3 + 17);
        let datagrams = split(meta(1), &original).unwrap();
        assert_eq!(datagrams.len(), 4);
        assert!(datagrams.iter().all(|d| d.len() <= SAFE_DATAGRAM));

        let mut reassembler = Reassembler::new();
        let mut done = None;
        for datagram in &datagrams {
            if let Some(frame) = reassembler.push(datagram).unwrap() {
                done = Some(frame);
            }
        }
        let frame = done.expect("the last chunk should have completed the frame");
        assert_eq!(frame.data, original);
        assert_eq!(frame.meta, meta(1));
        assert!(frame.is_keyframe());
        assert!(!frame.is_audio());
    }

    #[test]
    fn a_single_chunk_frame_completes_immediately() {
        let datagrams = split(meta(7), b"small").unwrap();
        assert_eq!(datagrams.len(), 1);
        let frame = Reassembler::new().push(&datagrams[0]).unwrap().unwrap();
        assert_eq!(frame.data, b"small");
    }

    #[test]
    fn chunks_arriving_backwards_still_assemble_in_order() {
        let original = payload(MAX_CHUNK_PAYLOAD * 3);
        let mut datagrams = split(meta(2), &original).unwrap();
        datagrams.reverse();

        let mut reassembler = Reassembler::new();
        let mut done = None;
        for datagram in &datagrams {
            if let Some(frame) = reassembler.push(datagram).unwrap() {
                done = Some(frame);
            }
        }
        assert_eq!(
            done.unwrap().data,
            original,
            "chunks were concatenated in arrival order"
        );
    }

    #[test]
    fn an_incomplete_frame_is_never_emitted() {
        let datagrams = split(meta(3), &payload(MAX_CHUNK_PAYLOAD * 4)).unwrap();
        let mut reassembler = Reassembler::new();
        for datagram in &datagrams[..3] {
            assert_eq!(reassembler.push(datagram).unwrap(), None);
        }
        assert_eq!(reassembler.pending(), 1);
    }

    #[test]
    fn a_duplicated_chunk_changes_nothing() {
        let original = payload(MAX_CHUNK_PAYLOAD + 1);
        let datagrams = split(meta(4), &original).unwrap();
        let mut reassembler = Reassembler::new();

        assert_eq!(reassembler.push(&datagrams[0]).unwrap(), None);
        assert_eq!(
            reassembler.push(&datagrams[0]).unwrap(),
            None,
            "duplicate must not count"
        );
        assert_eq!(reassembler.push(&datagrams[0]).unwrap(), None);
        let frame = reassembler
            .push(&datagrams[1])
            .unwrap()
            .expect("now complete");
        assert_eq!(frame.data, original);
    }

    #[test]
    fn a_late_chunk_cannot_resurrect_a_finished_frame() {
        let datagrams = split(meta(5), &payload(MAX_CHUNK_PAYLOAD + 1)).unwrap();
        let mut reassembler = Reassembler::new();
        reassembler.push(&datagrams[0]).unwrap();
        assert!(reassembler.push(&datagrams[1]).unwrap().is_some());

        for datagram in &datagrams {
            assert_eq!(
                reassembler.push(datagram).unwrap(),
                None,
                "frame 5 was already emitted"
            );
        }
        assert_eq!(reassembler.pending(), 0);
    }

    #[test]
    fn a_chunk_from_an_older_frame_is_dropped_not_buffered() {
        let mut reassembler = Reassembler::new();
        let recent = split(meta(100), b"now").unwrap();
        assert!(reassembler.push(&recent[0]).unwrap().is_some());

        let stale = split(meta(4), &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();
        assert_eq!(reassembler.push(&stale[0]).unwrap(), None);
        assert_eq!(
            reassembler.pending(),
            0,
            "a stale frame must not occupy a pending slot"
        );
    }

    #[test]
    fn unfinished_frames_are_evicted_instead_of_accumulating() {
        // The memory-exhaustion defence: open far more frames than the cap and
        // never finish any of them.
        let mut reassembler = Reassembler::with_capacity(4);
        for id in 1..=64u32 {
            let datagrams = split(meta(id), &payload(MAX_CHUNK_PAYLOAD * 8)).unwrap();
            assert_eq!(reassembler.push(&datagrams[0]).unwrap(), None);
            assert!(
                reassembler.pending() <= 4,
                "pending grew past the cap at frame {id}"
            );
        }
        assert_eq!(reassembler.pending(), 4);
        assert_eq!(reassembler.dropped_incomplete(), 60);
    }

    #[test]
    fn eviction_takes_the_frame_that_has_waited_longest() {
        let mut reassembler = Reassembler::with_capacity(2);
        let a = split(meta(1), &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();
        let b = split(meta(2), &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();
        let c = split(meta(3), &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();

        reassembler.push(&a[0]).unwrap();
        reassembler.push(&b[0]).unwrap();
        reassembler.push(&c[0]).unwrap(); // evicts frame 1

        // Frame 2 survived and completes. Frame 1 is gone, and once frame 2 has
        // been emitted its leftover chunk is also too old to admit, so it is
        // discarded rather than opening a partial that can never finish.
        assert!(reassembler.push(&b[1]).unwrap().is_some());
        assert_eq!(reassembler.push(&a[1]).unwrap(), None);
        assert_eq!(reassembler.pending(), 1, "only frame 3 is still open");
    }

    #[test]
    fn a_chunk_count_of_zero_is_refused() {
        let mut header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 1,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        }
        .to_bytes();
        header[10] = 0;
        header[11] = 0;
        assert_eq!(ChunkHeader::parse(&header), Err(ProtocolError::Malformed));
    }

    #[test]
    fn an_index_past_the_end_is_refused() {
        // Without this check the index is a direct out-of-bounds subscript into
        // the slot vector.
        let mut header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 4,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        }
        .to_bytes();
        for index in [4u16, 5, u16::MAX] {
            header[8..10].copy_from_slice(&index.to_le_bytes());
            assert_eq!(
                ChunkHeader::parse(&header),
                Err(ProtocolError::Malformed),
                "index {index}"
            );
        }
    }

    #[test]
    fn a_chunk_count_above_the_cap_is_refused() {
        let mut header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 1,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        }
        .to_bytes();
        header[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(ChunkHeader::parse(&header), Err(ProtocolError::Malformed));

        header[10..12].copy_from_slice(&(MAX_CHUNKS_PER_FRAME + 1).to_le_bytes());
        assert_eq!(ChunkHeader::parse(&header), Err(ProtocolError::Malformed));

        header[10..12].copy_from_slice(&MAX_CHUNKS_PER_FRAME.to_le_bytes());
        assert!(ChunkHeader::parse(&header).is_ok());
    }

    #[test]
    fn an_undefined_flag_bit_is_refused() {
        let mut header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 1,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        }
        .to_bytes();
        header[13] = 0b1000_0000;
        assert_eq!(ChunkHeader::parse(&header), Err(ProtocolError::Malformed));
    }

    #[test]
    fn a_datagram_too_short_to_hold_a_header_is_refused() {
        let full = split(meta(1), b"x").unwrap().remove(0);
        for cut in 0..ChunkHeader::SIZE {
            assert_eq!(
                ChunkHeader::parse(&full[..cut]),
                Err(ProtocolError::Malformed)
            );
            assert_eq!(
                Reassembler::new().push(&full[..cut]),
                Err(ProtocolError::Malformed)
            );
        }
    }

    #[test]
    fn a_header_with_no_payload_is_refused() {
        let header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 1,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        };
        assert_eq!(
            Reassembler::new().push(&header.to_bytes()),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn an_oversized_payload_is_refused() {
        let header = ChunkHeader {
            frame_id: 1,
            capture_micros: 0,
            chunk_index: 0,
            chunk_count: 1,
            monitor: MonitorId::PRIMARY,
            flags: ChunkFlags::empty(),
        };
        let mut datagram = header.to_bytes().to_vec();
        datagram.extend_from_slice(&payload(MAX_CHUNK_PAYLOAD + 1));
        assert_eq!(
            Reassembler::new().push(&datagram),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn chunks_that_disagree_about_their_frame_are_refused() {
        // Splicing a chunk from one frame into another would let a peer change
        // what an assembled frame claims to be. Each of these differs from its
        // sibling in exactly one header field.
        let base = meta(9);
        let first = split(base, &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();

        let variants = [
            FrameMeta {
                capture_micros: 999,
                ..base
            },
            FrameMeta {
                monitor: MonitorId(3),
                ..base
            },
            FrameMeta {
                flags: ChunkFlags::AUDIO,
                ..base
            },
        ];
        for variant in variants {
            let mut reassembler = Reassembler::new();
            reassembler.push(&first[0]).unwrap();
            let spliced = split(variant, &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();
            assert_eq!(
                reassembler.push(&spliced[1]),
                Err(ProtocolError::Malformed),
                "{variant:?} was accepted into frame 9"
            );
        }
    }

    #[test]
    fn chunks_that_disagree_about_the_chunk_count_are_refused() {
        let first = split(meta(9), &payload(MAX_CHUNK_PAYLOAD * 2)).unwrap();
        let mut reassembler = Reassembler::new();
        reassembler.push(&first[0]).unwrap();

        let mut forged = first[1].clone();
        forged[10..12].copy_from_slice(&7u16.to_le_bytes());
        assert_eq!(reassembler.push(&forged), Err(ProtocolError::Malformed));
    }

    #[test]
    fn an_empty_frame_cannot_be_split() {
        assert_eq!(split(meta(1), &[]), Err(ProtocolError::Malformed));
    }

    #[test]
    fn a_frame_larger_than_the_cap_cannot_be_split() {
        // The sender refuses locally rather than emitting chunks the receiver
        // is guaranteed to reject.
        assert_eq!(
            split(meta(1), &vec![0u8; MAX_FRAME_BYTES + 1]),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn a_frame_at_exactly_the_cap_still_splits() {
        let datagrams = split(meta(1), &vec![0u8; MAX_FRAME_BYTES]).unwrap();
        assert_eq!(datagrams.len(), MAX_CHUNKS_PER_FRAME as usize);
    }

    #[test]
    fn frame_ids_keep_working_after_they_wrap() {
        // At 144 fps a u32 wraps in about 345 days. A naive `>` comparison
        // would treat every frame after the wrap as ancient and stall the
        // stream permanently.
        assert!(is_newer(0, u32::MAX));
        assert!(is_newer(5, u32::MAX - 5));
        assert!(!is_newer(u32::MAX, 0));
        assert!(!is_newer(7, 7));

        let mut reassembler = Reassembler::new();
        let last = split(meta(u32::MAX), b"before").unwrap();
        assert!(reassembler.push(&last[0]).unwrap().is_some());

        let wrapped = split(meta(0), b"after").unwrap();
        let frame = reassembler.push(&wrapped[0]).unwrap();
        assert_eq!(frame.map(|f| f.data), Some(b"after".to_vec()));
    }

    #[test]
    fn reset_lets_a_restarted_session_number_from_zero_again() {
        let mut reassembler = Reassembler::new();
        assert!(reassembler
            .push(&split(meta(9000), b"old").unwrap()[0])
            .unwrap()
            .is_some());

        // Without the reset, frame 1 of the new session looks like ancient
        // history and every frame is dropped.
        assert_eq!(
            reassembler
                .push(&split(meta(1), b"new").unwrap()[0])
                .unwrap(),
            None
        );
        reassembler.reset();
        assert!(reassembler
            .push(&split(meta(1), b"new").unwrap()[0])
            .unwrap()
            .is_some());
    }

    #[test]
    fn two_frames_can_be_in_flight_at_once() {
        // Ordinary on a real path: the tail of one frame overlaps the head of
        // the next, and interleaving must not mix their payloads.
        let a = split(meta(1), &payload(MAX_CHUNK_PAYLOAD + 10)).unwrap();
        let b = split(meta(2), &payload(MAX_CHUNK_PAYLOAD + 20)).unwrap();

        let mut reassembler = Reassembler::new();
        assert_eq!(reassembler.push(&a[0]).unwrap(), None);
        assert_eq!(reassembler.push(&b[0]).unwrap(), None);
        assert_eq!(reassembler.pending(), 2);

        let first = reassembler.push(&a[1]).unwrap().expect("frame 1 completes");
        assert_eq!(first.data, payload(MAX_CHUNK_PAYLOAD + 10));
        let second = reassembler.push(&b[1]).unwrap().expect("frame 2 completes");
        assert_eq!(second.data, payload(MAX_CHUNK_PAYLOAD + 20));
    }
}

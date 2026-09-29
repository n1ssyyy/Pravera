//! Messages on a terminal stream.
//!
//! A remote shell produces and consumes bytes at wildly varying rates — a
//! keystroke is one byte, `cat` on a large file is megabytes a second — and it
//! runs for as long as the person keeps it open. That shape is exactly what the
//! clipboard documentation warns the control stream cannot absorb, so a
//! terminal gets its own QUIC stream, one per shell, opened by the host after
//! [`crate::ClientMessage::OpenTerminal`] is granted and closed when the shell
//! exits.
//!
//! ```text
//! client                          host
//!   |--- OpenTerminal{cols,rows} -->|   control stream, permission-checked
//!   |<------ TerminalStarted -------|   control stream
//!   |<======= terminal stream ======|   host opens, both ends talk
//!   |---- TerminalIn::Input ------->|   keystrokes, paste
//!   |---- TerminalIn::Resize ------>|   the pane changed size
//!   |<--- TerminalOut::Output ------|   whatever the shell printed
//!   |<--- TerminalOut::Exited ------|   last message before the stream ends
//! ```
//!
//! The stream is reliable and ordered, which is what a terminal wants: unlike
//! video, a lost byte in `rm -rf` output is not a cosmetic artifact, and
//! unlike datagrams there is nothing to reassemble.
//!
//! ## Why there is no close message
//!
//! Closing the stream *is* closing the terminal. The host kills the shell when
//! its stream goes away, so a panel that is dismissed resets the stream rather
//! than announcing its departure — one fewer message, and no way for a client
//! to believe it closed something that is still running.

use pravera_core::Permission;
use serde::{Deserialize, Serialize};

/// Ceiling on one [`TerminalIn::Input`] chunk.
///
/// Keystrokes arrive one or two bytes at a time; the reason this ceiling is not
/// tiny is paste. A large paste is bursty but finite, and 4 KiB covers a
/// generous screenful. Above it the input is refused rather than truncated — a
/// paste that silently loses its tail looks like a working clipboard that ate
/// half a command.
pub const MAX_INPUT_CHUNK: usize = 4096;

/// Ceiling on one [`TerminalOut::Output`] chunk.
///
/// Sized against [`crate::MAX_CONTROL_MESSAGE`] rather than against taste: a
/// frame must fit comfortably inside the largest message any Pravera stream
/// will carry, and 64 KiB is far below that cap while being several screens of
/// text. The host splits larger reads across chunks, so this bounds memory per
/// frame, never throughput.
pub const MAX_OUTPUT_CHUNK: usize = 64 * 1024;

/// Widest a terminal may be asked for.
///
/// Bounds the grid a client allocates per cell, so a hostile
/// [`TerminalIn::Resize`] cannot turn a six-byte message into gigabytes of
/// cells. No real terminal emulator runs meaningfully wider; a pane this size
/// would be unreadable long before it hit the limit.
pub const MAX_COLUMNS: u16 = 1024;

/// Tallest a terminal may be asked for. See [`MAX_COLUMNS`] for why the bound
/// exists at all.
pub const MAX_ROWS: u16 = 1024;

/// Whether a terminal size is one this protocol will act on.
///
/// Zero is the interesting case: a zero-column or zero-row grid has no cells,
/// and every write into it would be a silent no-op, so a resize that produced
/// one is refused rather than honoured.
pub fn is_valid_size(cols: u16, rows: u16) -> bool {
    cols >= 1 && cols <= MAX_COLUMNS && rows >= 1 && rows <= MAX_ROWS
}

/// Client to host, on the terminal stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalIn {
    /// Bytes typed or pasted, exactly as they should reach the shell.
    ///
    /// Raw bytes rather than characters: the client's emulator owns key
    /// encoding, because escape sequences for arrows and function keys are
    /// the shell's business and re-interpreting them here would mean this
    /// protocol learning every sequence a program on the host might ask for.
    Input {
        bytes: Vec<u8>,
    },
    /// The pane changed size. The host resizes its pseudoconsole, and well
    /// behaved programs redraw to fit.
    Resize {
        cols: u16,
        rows: u16,
    },
}

impl TerminalIn {
    /// Whether this is worth acting on.
    ///
    /// Checked on the host before the bytes reach the shell. An oversized
    /// input is refused whole — see [`MAX_INPUT_CHUNK`] for why truncation is
    /// worse than refusal — and an impossible size is refused because a grid
    /// with no cells cannot be resized into anything.
    pub fn is_well_formed(&self) -> bool {
        match self {
            TerminalIn::Input { bytes } => bytes.len() <= MAX_INPUT_CHUNK,
            TerminalIn::Resize { cols, rows } => is_valid_size(*cols, *rows),
        }
    }
}

/// Host to client, on the terminal stream.
///
/// Carries the raw VT stream the shell produced. Parsing it into a grid is the
/// client's job (`pravera-term`); the wire carries exactly what the
/// pseudoconsole emitted, because re-encoding rendered cells would spend
/// bandwidth reproducing information the bytes already contain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalOut {
    /// Output from the shell, in order, possibly mid-escape-sequence. Chunks
    /// are cut wherever the host's read landed; only concatenating them in
    /// stream order is meaningful.
    Output {
        bytes: Vec<u8>,
    },
    /// The shell exited. This is the final message on the stream; the sender
    /// finishes the stream immediately after it.
    Exited {
        exit_code: i32,
    },
}

impl TerminalOut {
    /// Whether this is worth sending.
    ///
    /// The host is the trusted end, so this is hygiene rather than defence:
    /// refusing to frame more than [`MAX_OUTPUT_CHUNK`] keeps a bug in the
    /// read loop from turning into an enormous allocation on the far end.
    pub fn is_well_formed(&self) -> bool {
        match self {
            TerminalOut::Output { bytes } => !bytes.is_empty() && bytes.len() <= MAX_OUTPUT_CHUNK,
            TerminalOut::Exited { .. } => true,
        }
    }

    /// What a shell that was killed rather than allowed to finish reports.
    ///
    /// Negative codes are unreachable from a normal Unix or Windows exit, so
    /// one on the wire means the host tore the terminal down — the client can
    /// show "closed" rather than pretend a number came from the shell.
    pub const KILLED: i32 = -1;
}

/// What a role must hold to open a terminal at all.
///
/// [`Permission::CONTROL`], deliberately, and deliberately not a dedicated
/// flag: a shell is keyboard control taken to its conclusion. Anyone trusted
/// to inject keys into the desktop can already start one locally, so a
/// separate grant would add a row to the permissions matrix while protecting
/// nothing. The honest boundary in this protocol is between watching and
/// doing, and a terminal sits firmly on the doing side of it.
pub const REQUIRED_PERMISSION: Permission = Permission::CONTROL;

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn every_terminal_message_survives_a_round_trip() {
        let incoming = [
            TerminalIn::Input {
                bytes: b"ls -la\r\n".to_vec(),
            },
            TerminalIn::Input { bytes: vec![0x1b] },
            TerminalIn::Resize {
                cols: 120,
                rows: 40,
            },
        ];
        for message in incoming {
            assert_eq!(round_trip(&message), message);
        }

        let outgoing = [
            TerminalOut::Output {
                bytes: vec![0x1b, 0x5b, 0x31, 0x6d],
            },
            TerminalOut::Exited { exit_code: 0 },
            TerminalOut::Exited {
                exit_code: i32::MIN,
            },
        ];
        for message in outgoing {
            assert_eq!(round_trip(&message), message);
        }
    }

    #[test]
    fn a_keystroke_costs_very_little_on_the_wire() {
        // Input rides the terminal stream at typing rate. If a single press
        // ever grew past a handful of bytes, a fast typist would be paying for
        // framing rather than for text.
        let encoded =
            postcard::to_allocvec(&TerminalIn::Input { bytes: vec![b'x'] }).expect("encode");
        assert!(
            encoded.len() <= 4,
            "one keystroke encoded as {} bytes",
            encoded.len()
        );
    }

    #[test]
    fn an_oversized_paste_is_refused_rather_than_truncated() {
        // A paste missing its tail is worse than one that did not happen: the
        // person believes the whole command arrived.
        let fits = TerminalIn::Input {
            bytes: vec![b'x'; MAX_INPUT_CHUNK],
        };
        let does_not = TerminalIn::Input {
            bytes: vec![b'x'; MAX_INPUT_CHUNK + 1],
        };
        assert!(fits.is_well_formed());
        assert!(!does_not.is_well_formed());
    }

    #[test]
    fn an_impossible_pane_size_is_refused() {
        assert!(is_valid_size(80, 24));
        assert!(is_valid_size(MAX_COLUMNS, MAX_ROWS));

        // A zero dimension describes a grid with no cells in it. Every later
        // write would be a silent no-op, so it is refused here where the
        // refusal is cheap.
        assert!(!is_valid_size(0, 24));
        assert!(!is_valid_size(80, 0));

        assert!(!TerminalIn::Resize { cols: 0, rows: 10 }.is_well_formed());
        assert!(!TerminalIn::Resize {
            cols: MAX_COLUMNS + 1,
            rows: 10
        }
        .is_well_formed());
        assert!(TerminalIn::Resize {
            cols: MAX_COLUMNS,
            rows: MAX_ROWS
        }
        .is_well_formed());
    }

    #[test]
    fn an_empty_output_chunk_is_not_worth_a_frame() {
        // The read loop cuts chunks wherever a read lands, so empty reads are
        // ordinary; they are filtered where they happen rather than framed and
        // sent. Pinning that here keeps the wire free of no-op frames.
        assert!(!TerminalOut::Output { bytes: vec![] }.is_well_formed());
        assert!(TerminalOut::Output {
            bytes: vec![b'x'; MAX_OUTPUT_CHUNK]
        }
        .is_well_formed());
        assert!(!TerminalOut::Output {
            bytes: vec![b'x'; MAX_OUTPUT_CHUNK + 1]
        }
        .is_well_formed());
    }

    #[test]
    fn a_killed_terminal_is_distinguishable_from_a_real_exit_code() {
        // Shells report small non-negative codes. Anything negative therefore
        // means the host did the killing, and a client may say so honestly
        // rather than presenting a number the shell never produced.
        assert!(TerminalOut::Exited { exit_code: 130 }.is_well_formed());
        assert!(TerminalOut::KILLED < 0);
        assert_eq!(
            round_trip(&TerminalOut::Exited {
                exit_code: TerminalOut::KILLED
            }),
            TerminalOut::Exited {
                exit_code: TerminalOut::KILLED
            }
        );
    }
}

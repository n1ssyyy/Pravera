//! Moving things that are not pictures: the clipboard, and files.
//!
//! Both ride reliable QUIC streams rather than the media datagrams, because
//! both are useless if they arrive with holes in them. Video can lose a frame
//! and carry on; half a paste is not a paste.
//!
//! ## Both directions are separate grants
//!
//! `CLIPBOARD_READ` and `CLIPBOARD_WRITE`, `FILE_READ` and `FILE_WRITE`. Being
//! allowed to put something on a machine is not the same as being allowed to
//! take whatever is already there, and the roles say so separately. As
//! everywhere else in Pravera the check that counts happens host-side, in
//! `pravera-host`, at dispatch.
//!
//! ## What this crate does not do
//!
//! It does not decide *whether* something may move — that is a permission
//! question and belongs to the host. It reads and writes what it is told to,
//! and reports what happened.

//! ## Files never touch the control stream
//!
//! Not their bytes and not their listings. Each gets a fresh QUIC stream, so a
//! 40 GB copy runs alongside the session rather than in front of it. See
//! [`pravera_proto::files`] for the conversation and [`transfer`] for what
//! happens to the disk at each end.

pub mod browse;
pub mod clipboard;
pub mod transfer;

pub use browse::{browse, list, places};
pub use clipboard::{Clipboard, Content};
pub use transfer::{Sink, Source};

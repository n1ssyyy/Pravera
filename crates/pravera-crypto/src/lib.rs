//! Identity and secret handling for Pravera.
//!
//! Two distinct things live here and should not be confused:
//!
//! - [`identity`] is about *devices*. An ed25519 keypair names a machine and
//!   authenticates it during the QUIC handshake.
//! - [`password`] is about *people*. Argon2id hashes guard the per-host user
//!   accounts that decide what a connected human is allowed to do.
//!
//! A session needs both: the device proves which machine is calling, and the
//! password proves who is sitting at it.

pub mod identity;
pub mod password;

pub use identity::{verify as verify_signature, Identity};

//! The host side of a Pravera session.
//!
//! Two pieces:
//!
//! - [`HostSession`], a pure state machine that decides what every client
//!   message is allowed to do. **This is the only place permissions are
//!   enforced.**
//! - [`serve`], the loop that reads the control stream, feeds the state machine
//!   and carries out what it asks for.
//!
//! The split exists so the rules can be tested without a network. Everything
//! interesting is in [`session`]; this file is plumbing.

pub mod accounts;
pub mod agent;
pub mod audio;
pub mod cursor;
pub mod files;
pub mod media;
pub mod secure;
pub mod session;
pub mod store;
pub mod terminal;

pub use accounts::{Account, FileStore};
pub use agent::{Agent, AgentStats};
pub use audio::{AudioStats, AudioStreamer};
pub use files::{serve_files, Gate};
pub use media::{monitors, screen_for, StreamStats, Streamer};
pub use session::{ClipboardAnswer, ClipboardAsk, Effect, HostConfig, HostSession, Response};
pub use store::{MemoryStore, UserStore};
pub use terminal::serve_terminal;

use std::time::Duration;

use pravera_transport::{Result, Session, TransportError};
use tracing::{debug, info, warn};

/// What the driver does with the effects the state machine produces.
///
/// Implemented in P2 and P3 by the capture, encode and input subsystems. A
/// recording implementation is what makes the loop testable today.
pub trait SessionHooks {
    /// Replay an authorised input event on this machine.
    ///
    /// Already permission-checked and already validated. An implementation must
    /// not second-guess it, and must not do its own authorisation: two places
    /// deciding the same thing is how they end up disagreeing.
    fn inject(&mut self, event: pravera_proto::InputEvent);

    /// Start or reconfigure capture and encoding.
    fn stream(&mut self, config: &pravera_proto::SessionConfig);

    /// Emit a keyframe on the next captured frame.
    fn keyframe(&mut self);

    /// Read or write this machine's clipboard.
    ///
    /// Already permission-checked, like [`SessionHooks::inject`]. The default
    /// answers [`ClipboardAnswer::Unavailable`], which is the honest reply for
    /// a driver with no clipboard of its own — a headless test harness, or a
    /// platform Pravera cannot reach one on yet. It fails closed: the peer is
    /// told the request did not work and nothing is copied either way.
    fn clipboard(&mut self, ask: &ClipboardAsk) -> ClipboardAnswer {
        let _ = ask;
        ClipboardAnswer::Unavailable
    }
}

/// How long an unauthenticated peer may hold a connection open.
///
/// Until someone has logged in, a connection costs this host memory and a slot
/// and has given nothing in return. After login the timeout is lifted: a live
/// session is legitimately quiet whenever the person at the other end is
/// reading rather than typing.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the peer to acknowledge the final message before
/// closing anyway.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Run one client connection to completion.
///
/// Returns `Ok(())` for every ordinary ending: the client said goodbye, hung
/// up, or ran out of password attempts. An `Err` means the connection itself
/// failed.
pub async fn serve<S, H>(session: &Session, host: &mut HostSession<S>, hooks: &mut H) -> Result<()>
where
    S: UserStore,
    H: SessionHooks,
{
    // The version was settled by the TLS handshake. Every message on this
    // connection is that version's, so the state machine holds `Hello` to it.
    host.set_version(session.protocol_version());
    let mut control = session.accept_control().await?;
    debug!(peer = %host.peer(), "control stream open");

    // Only now. QUIC delivers accepted streams in the order the peer opened
    // them, and that ordering is the only thing separating the control stream
    // from the first bulk one — a file server started any earlier would take
    // the control stream for a transfer.
    //
    // The gate starts shut, so a client that opens a bulk stream in the same
    // breath as its connection is refused rather than raced.
    let gate = Gate::closed();
    let files = tokio::spawn(serve_files(session.clone(), gate.clone()));
    loop {
        let incoming = if host.is_authenticated() {
            control.recv().await
        } else {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, control.recv()).await {
                Ok(result) => result,
                Err(_) => {
                    warn!(peer = %host.peer(), "handshake timed out");
                    session.close("handshake timed out");
                    return Ok(());
                }
            }
        };

        let message = match incoming {
            Ok(message) => message,
            // An ordinary hangup, not a failure.
            Err(TransportError::StreamClosed) => break,
            Err(e) => return Err(e),
        };

        let response = host.handle(message);

        // Republished after every message rather than only after the login.
        // The state machine is the authority; this is a mirror, and a mirror
        // that is only refreshed once is a mirror that goes stale the first
        // time the authority changes its mind.
        gate.open(host.permissions());

        // Reply before acting. A `SessionStarted` has to reach the client
        // before the first media datagram does, or the client receives frames
        // in a format it has not been told about yet. The same order lets a
        // final refusal arrive ahead of the disconnect that follows it.
        if let Some(reply) = &response.reply {
            control.send(reply).await?;
        }

        match response.effect {
            Some(Effect::Inject(event)) => hooks.inject(event),
            Some(Effect::Stream(config)) => hooks.stream(&config),
            Some(Effect::Keyframe) => hooks.keyframe(),
            // The one effect that owes a reply: the answer lives in the
            // clipboard rather than in the state machine, so it is produced
            // here. Exactly one message goes back, because the client is
            // blocked waiting for it.
            Some(Effect::Clipboard(ask)) => {
                let answer = hooks.clipboard(&ask);
                control.send(&clipboard_reply(answer)).await?;
            }
            Some(Effect::Disconnect) => break,
            // A terminal lives on its own task and its own stream, for as long
            // as its tab is open — possibly much longer than this loop's
            // interest in any single message. It tears itself down when the
            // connection dies underneath it.
            Some(Effect::OpenTerminal { cols, rows }) => {
                tokio::spawn(terminal::serve_terminal(session.clone(), cols, rows));
            }
            Some(Effect::SendSas) => {
                // `sas.dll` is only available to SYSTEM with the policy set by
                // the service installer; an agent running as the user will fail
                // here with a string that stays in the host log. The client was
                // already told `SasSent` — the request was heard — so the failure
                // is not re-sent as a second reply.
                if let Err(err) = crate::secure::send_sas() {
                    warn!(%err, "SendSAS was asked for but did not happen");
                } else {
                    info!("Secure Attention Sequence sent");
                }
            }
            None => {}
        }
    }

    // Shut before the connection closes, so a transfer that is mid-chunk stops
    // at its next check rather than finishing on a permission nobody holds any
    // more. Aborting the accept loop stops new ones from starting; the
    // connection closing below ends the transfers already running.
    gate.close();
    files.abort();

    // Let the last reply land before tearing the connection down. Closing a
    // QUIC connection abandons anything still in flight, and the messages that
    // precede a disconnect are precisely the ones the peer most needs: a
    // refused login, a version mismatch, a goodbye.
    //
    // Bounded, because a peer that never acknowledges would otherwise hold this
    // task open for as long as it liked.
    if tokio::time::timeout(SHUTDOWN_GRACE, control.flush())
        .await
        .is_err()
    {
        debug!(peer = %host.peer(), "peer did not acknowledge the final message");
    }
    session.close("session ended");

    debug!(peer = %host.peer(), "session finished");
    Ok(())
}

/// Turn what the driver managed into what the client is told.
///
/// A failure collapses to `Internal`, which says nothing about this machine.
/// The reason — no window station, another program holding the clipboard open,
/// a platform with no clipboard at all — is in the host's log, where it is
/// useful, rather than on the wire, where it describes the host to a peer.
fn clipboard_reply(answer: ClipboardAnswer) -> pravera_proto::HostMessage {
    use pravera_proto::{HostMessage, ProtocolError};
    match answer {
        ClipboardAnswer::Update(update) => HostMessage::Clipboard(update),
        ClipboardAnswer::Written(seq) => HostMessage::ClipboardSet { seq },
        ClipboardAnswer::Unavailable => HostMessage::Failed(ProtocolError::Internal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pravera_proto::{ClipboardSeq, ClipboardUpdate, HostMessage, ProtocolError};

    #[test]
    fn a_clipboard_that_could_not_be_reached_says_nothing_about_why() {
        // The peer learns that it failed. Whether this machine has no window
        // station, or another program is holding the clipboard open, is a
        // description of the host and stays in the host's log.
        assert_eq!(
            clipboard_reply(ClipboardAnswer::Unavailable),
            HostMessage::Failed(ProtocolError::Internal)
        );
    }

    #[test]
    fn a_write_answers_with_the_sequence_it_produced() {
        // Without it the client's next poll is told its own paste is a change
        // to copy back, and the two machines copy it to each other forever.
        assert_eq!(
            clipboard_reply(ClipboardAnswer::Written(ClipboardSeq(7))),
            HostMessage::ClipboardSet {
                seq: ClipboardSeq(7)
            }
        );
    }

    #[test]
    fn an_unchanged_clipboard_is_an_answer_rather_than_a_silence() {
        // The client is blocked on this reply; answering nothing would hang it.
        assert_eq!(
            clipboard_reply(ClipboardAnswer::Update(ClipboardUpdate::Unchanged)),
            HostMessage::Clipboard(ClipboardUpdate::Unchanged)
        );
    }
}

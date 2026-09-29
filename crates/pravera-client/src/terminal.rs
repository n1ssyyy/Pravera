//! The client end of a terminal stream.
//!
//! `Client::open_terminal` asks for a shell; the host answers
//! [`HostMessage::TerminalStarted`] and then opens the stream itself, so this
//! module is the accepting side. From there one task owns the stream for the
//! terminal's whole life: bytes typed travel in as
//! [`TerminalIn`](pravera_proto::TerminalIn), bytes printed travel out as
//! [`TerminalEvent::Output`], and neither the interface nor anything else
//! touches the stream directly.
//!
//! Closing is dropping. The command channel closes, the pump notices, the
//! stream resets, and the host kills the shell — the protocol's definition of
//! a closed terminal, so there is no close message and no way to close one
//! half by mistake.

use pravera_proto::{HostMessage, TerminalIn, TerminalOut};
use tokio::sync::mpsc;
use tracing::debug;

use crate::error::{ClientError, Result};
use crate::Client;

/// What a terminal told the interface.
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// Bytes the shell printed, in order, possibly mid-escape-sequence.
    ///
    /// Raw on purpose: parsing into a grid is `pravera-term`'s job, and doing
    /// it here would mean every consumer of this crate inheriting one
    /// emulator's opinions.
    Output(Vec<u8>),
    /// The shell exited. Terminal in the other sense: nothing follows.
    Exited(i32),
    /// The stream ended without the shell saying it had — the connection
    /// dropped, or the host tore the session down. Nothing follows this
    /// either, and it is not the shell's doing, so it carries no exit code.
    Closed,
}

/// A shell on the far machine.
///
/// Cheap to clone for the parts of an interface that send input from more
/// than one place; the events arrive on the one receiver `open_terminal`
/// handed out, because a terminal with two output streams is two terminals.
pub struct Terminal {
    commands: mpsc::UnboundedSender<TerminalIn>,
}

impl Terminal {
    /// Bytes for the shell: typed, or a paste.
    ///
    /// Refused here rather than sent when it cannot fit
    /// [`pravera_proto::MAX_INPUT_CHUNK`], for the same reason the protocol
    /// refuses it there: a paste that silently loses its tail is worse than
    /// one that did not happen.
    pub fn input(&self, bytes: Vec<u8>) {
        if bytes.len() > pravera_proto::MAX_INPUT_CHUNK {
            debug!(
                bytes = bytes.len(),
                "dropped terminal input larger than the protocol carries"
            );
            return;
        }
        if self.commands.send(TerminalIn::Input { bytes }).is_err() {
            debug!("input was typed into a terminal that has already ended");
        }
    }

    /// The pane changed size. The host resizes its console, and well behaved
    /// programs redraw to fit.
    pub fn resize(&self, cols: u16, rows: u16) {
        if !pravera_proto::terminal::is_valid_size(cols, rows) {
            return;
        }
        if self
            .commands
            .send(TerminalIn::Resize { cols, rows })
            .is_err()
        {
            debug!("a resize was sent to a terminal that has already ended");
        }
    }

    /// Whether the pump is still running. A terminal that has exited stays
    /// usable as an object — its last screen is still drawn — but sends
    /// nothing anywhere.
    pub fn is_alive(&self) -> bool {
        !self.commands.is_closed()
    }
}

impl Client {
    /// Ask the host for a shell in a console of the given size. Needs
    /// `CONTROL`, like everything that types.
    ///
    /// Returns the terminal and the receiver its output arrives on. The
    /// receiver is the interface's to drain; a full receiver applies
    /// backpressure to the network, which applies it to the shell — a
    /// terminal nobody reads pauses rather than growing without bound.
    pub async fn open_terminal(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Result<(Terminal, mpsc::UnboundedReceiver<TerminalEvent>)> {
        self.require_login("opening a terminal")?;

        let reply = self
            .control
            .request(&pravera_proto::ClientMessage::OpenTerminal { cols, rows })
            .await?;

        match reply {
            HostMessage::TerminalStarted => {}
            HostMessage::Failed(error) => return Err(ClientError::Refused(error)),
            other => {
                return Err(ClientError::Unexpected {
                    expected: "TerminalStarted",
                    got: crate::error::describe(&other),
                });
            }
        }

        // The host opens the stream after saying it would, so this accept is
        // already satisfied or about to be. Only the host initiates terminals,
        // and this client initiates everything else it accepts nothing of —
        // file streams it opens itself — so the two uses of bulk streams never
        // cross on this side.
        let stream = self.session.accept_bulk().await?;

        let (commands, mut inbox) = mpsc::unbounded_channel::<TerminalIn>();
        let (events, updates) = mpsc::unbounded_channel::<TerminalEvent>();

        tokio::spawn(async move {
            pump(stream, &mut inbox, &events).await;
        });

        Ok((Terminal { commands }, updates))
    }
}

/// One terminal's whole life: relay commands out, relay output in, end when
/// either side does.
async fn pump(
    mut stream: pravera_transport::BulkStream,
    commands: &mut mpsc::UnboundedReceiver<TerminalIn>,
    events: &mpsc::UnboundedSender<TerminalEvent>,
) {
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(message) if message.is_well_formed() => {
                        if stream.send(&message).await.is_err() {
                            let _ = events.send(TerminalEvent::Closed);
                            break;
                        }
                    }
                    // Malformed sizes were refused at the `Terminal` methods;
                    // arriving here means a patched caller, and ignoring it
                    // costs nothing.
                    Some(_) => {}
                    // The last handle went away. Resetting the stream is the
                    // close: the host sees it and kills the shell.
                    None => {
                        stream.cancel();
                        break;
                    }
                }
            }
            outgoing = stream.recv::<TerminalOut>() => {
                match outgoing {
                    Ok(TerminalOut::Output { bytes }) => {
                        if events.send(TerminalEvent::Output(bytes)).is_err() {
                            // Nobody is listening any more. Closing the stream
                            // closes the terminal, which is what the
                            // disappeared receiver implies.
                            stream.cancel();
                            break;
                        }
                    }
                    Ok(TerminalOut::Exited { exit_code }) => {
                        let _ = events.send(TerminalEvent::Exited(exit_code));
                        break;
                    }
                    Err(error) => {
                        debug!(%error, "a terminal's stream ended");
                        let _ = events.send(TerminalEvent::Closed);
                        break;
                    }
                }
            }
        }
    }
}

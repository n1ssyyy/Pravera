//! Driving a session from the interface.
//!
//! iced's update loop is synchronous and must never block; a Pravera session
//! is a long-lived async conversation plus a decoder thread. This module is the
//! seam between them.
//!
//! The [`Client`] itself lives inside one task and is never touched from
//! anywhere else, so there is no lock on the control stream and no way for two
//! parts of the interface to interleave two requests. Everything the UI wants
//! to do arrives as a [`Command`] on a channel; everything it needs to know
//! comes back as an [`Event`].
//!
//! Frames do not travel that way. They come out of the decoder's mailbox, which
//! the view reads directly on each redraw — a channel would queue them, and a
//! queue of video frames is a queue of increasingly stale pictures. See
//! `pravera_client::video` for why that trade goes the way it does.

use std::sync::Arc;
use std::time::Duration;

use pravera_client::{
    AudioSink, AudioStats, AudioStream, Client, ClientConfig, ClientError, VideoStats, VideoStream,
};
use pravera_core::{DeviceId, Permission, QualityProfile, Resolution};
use pravera_proto::{InputEvent, Monitor, MonitorId, SessionConfig};
use pravera_transport::{PeerAddress, RouteKind, Session, Transport};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::net::clipboard::Clipboards;
use crate::widget::video::Picture;

/// How often the driver measures the control round-trip.
///
/// Every second is often enough to notice a route changing and rare enough
/// that the measurement costs nothing. The figure shown in the overlay is this
/// one, and it is labelled as what it is: the control stream's round trip, not
/// the time a pixel takes to arrive.
const PING_EVERY: Duration = Duration::from_secs(1);

/// How often the two clipboards are compared.
///
/// Twice a second is under the threshold where switching windows and pasting
/// feels like waiting, and the question costs one counter read locally and one
/// small round trip to a host that answers it from a counter of its own. See
/// [`crate::net::clipboard`] for why this is asked rather than announced.
const CLIPBOARD_EVERY: Duration = Duration::from_millis(500);

/// How long one handshake step may take before the attempt fails with a
/// sentence instead of spinning forever. Dial, login, session start and the
/// monitor list are each awaited bare — a stalled hole-punch or a relay that
/// answers the handshake but never the login left the dialog spinner going
/// with no `Ending::Failed` at all.
const HANDSHAKE_STEP: Duration = Duration::from_secs(30);

/// Run one handshake step with a deadline. A timeout names the step that
/// hung, so the dialog can say which half of the handshake never answered.
async fn timed<T>(
    step: &'static str,
    future: impl std::future::Future<Output = Result<T, ClientError>>,
) -> Result<T, String> {
    match tokio::time::timeout(HANDSHAKE_STEP, future).await {
        Ok(result) => result.map_err(describe),
        Err(_) => Err(format!(
            "{step} timed out after {} seconds. The machine answered the dial but stopped there — \
             check it is still running this version of Pravera.",
            HANDSHAKE_STEP.as_secs()
        )),
    }
}

/// What the interface asks a live session to do.
#[derive(Debug, Clone)]
pub enum Command {
    Input(InputEvent),
    Keyframe,
    SetProfile(QualityProfile),
    SelectMonitor(MonitorId),
    /// Ask the host for a shell in a console of this size. The terminal comes
    /// back in [`Event::TerminalOpened`], because only the task that owns the
    /// client can run the request.
    OpenTerminal { cols: u16, rows: u16 },
    /// Ask the host to generate Ctrl+Alt+Del on its console.
    SendSas,
    Disconnect,
}

/// What a live session tells the interface.
#[derive(Debug, Clone)]
pub enum Event {
    /// The host reconfigured the stream, and a fresh decoder is running.
    ///
    /// The new [`VideoStream`] travels with the message because only the task
    /// that owns the client can build one, and the old decoder holds state for
    /// a codec and frame size that no longer apply.
    Reconfigured {
        config: Box<SessionConfig>,
        video: Arc<VideoStream>,
    },
    /// A terminal was asked for, and this is what became of it. The terminal
    /// and its event receiver cannot be cloned, so they travel in a
    /// [`Carry`](crate::net::Carry).
    TerminalOpened(
        Result<
            crate::net::Carry<(
                pravera_client::Terminal,
                mpsc::UnboundedReceiver<pravera_client::TerminalEvent>,
            )>,
            String,
        >,
    ),
    /// A control round-trip completed. A real measurement, not an estimate.
    Latency(Duration),
    /// The session ended, for whatever reason. Terminal.
    Ended(Ending),
}

/// Why a session stopped.
#[derive(Debug, Clone)]
pub enum Ending {
    /// The person asked to disconnect.
    Requested,
    /// The host said goodbye.
    Remote(String),
    /// Something broke. The string is shown to the person, so it says what
    /// they can do about it where there is anything to say.
    Failed(String),
}

impl Ending {
    pub fn message(&self) -> String {
        match self {
            Ending::Requested => "Disconnected.".into(),
            Ending::Remote(reason) if reason.is_empty() => "The host ended the session.".into(),
            Ending::Remote(reason) => format!("The host ended the session: {reason}"),
            Ending::Failed(error) => error.clone(),
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Ending::Failed(_))
    }
}

/// Everything needed to reach a machine and be let in.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub peer: PeerAddress,
    pub username: String,
    pub password: String,
    pub profile: QualityProfile,
    pub monitor: MonitorId,
    /// Cap the stream below the host's native resolution. `None` is native.
    pub max_resolution: Option<Resolution>,
    /// Whether to ask the host for its sound as well as its screen.
    pub audio: bool,
}

/// A session the interface holds on to.
///
/// Dropping it disconnects: the command channel closes, the driver task
/// notices, and the connection is closed politely rather than abandoned.
pub struct Link {
    commands: mpsc::UnboundedSender<Command>,
    /// The connection itself, for anything that does not go through the
    /// driver task — which today means file transfers.
    ///
    /// They get their own QUIC streams and never touch the control stream, so
    /// they do not have to be serialised behind the driver's one-command-at-a-
    /// time loop. Cloning a session shares the connection rather than opening
    /// a second one.
    session: Session,
    video: Arc<VideoStream>,
    /// Playback, if the host agreed to send sound and this machine could open
    /// a device to hear it on.
    ///
    /// Outlives the video stream on purpose: switching monitor rebuilds the
    /// picture, and taking the sound down with it would put an audible gap in
    /// the middle of a session.
    audio: Option<Arc<AudioStream>>,
    /// Whether sound was asked for when the session opened.
    ///
    /// Kept because it is the difference between a session that is quiet
    /// because somebody chose quiet and a session that is quiet because
    /// something did not work. Only the second one is worth saying out loud.
    wanted_audio: bool,
    config: SessionConfig,
    host_name: String,
    device: DeviceId,
    route: Option<RouteKind>,
    /// The displays this login is allowed to know about. Empty for a role
    /// without `MULTI_MONITOR`, which is not an error: the host refuses to
    /// enumerate them, and there is nothing to choose between.
    monitors: Vec<Monitor>,
    /// What the host said this login may do. A display hint only — every one
    /// of these is decided again, host-side, on the message that uses it.
    permissions: Permission,
    /// Rises with every picture handed to the interface, so the widget can
    /// skip an upload it has already done.
    generation: u64,
}

impl Link {
    /// The connection, for a request that does not go through the driver.
    ///
    /// Cloning is how a file transfer gets at it: the clone shares the same
    /// QUIC connection, and opening a bulk stream on it does not disturb the
    /// control stream the driver owns.
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// What playback has done, if this session has sound.
    ///
    /// `None` means there is nothing to report rather than nothing happening,
    /// which is the difference between showing a row of zeroes and showing no
    /// row at all.
    pub fn audio_stats(&self) -> Option<AudioStats> {
        self.audio.as_ref().map(|stream| stream.stats())
    }

    /// Whether sound is actually playing, as opposed to having been agreed to.
    pub fn is_playing_audio(&self) -> bool {
        self.audio
            .as_ref()
            .is_some_and(|stream| stream.is_running())
    }

    /// Sound was asked for and there is none.
    ///
    /// Worth showing, because it is a thing the person expects to be happening
    /// that is not. Silence they chose themselves is not worth a word.
    pub fn is_missing_audio(&self) -> bool {
        self.wanted_audio && !self.is_playing_audio()
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    /// The displays that can be switched between. Empty means there is no
    /// choice to offer, whether because the host has one display or because
    /// this login may not enumerate them.
    pub fn monitors(&self) -> &[Monitor] {
        &self.monitors
    }

    /// What this login was granted.
    ///
    /// Used to say so in the interface, never to decide anything: the host
    /// refuses what it refuses regardless of what is drawn here.
    pub fn permissions(&self) -> Permission {
        self.permissions
    }

    pub fn can_control(&self) -> bool {
        self.permissions.allows(Permission::CONTROL)
    }

    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// How the packets are getting there, if the transport has settled on a
    /// path yet. Queried live: QUIC migrates `Direct ↔ Relay` mid-session
    /// and a snapshot taken at `connect` lies after migration (the badge
    /// said `direct` while datagrams went via relay).
    pub fn route(&self) -> Option<RouteKind> {
        let live = self.session.route().kind;
        live.or(self.route)
    }

    pub fn stats(&self) -> VideoStats {
        self.video.stats()
    }

    /// Whether the decoder is still running.
    pub fn is_streaming(&self) -> bool {
        self.video.is_running()
    }

    /// The newest picture, if one has arrived since the last call.
    ///
    /// Called once per redraw. Returning `None` is the normal case on a still
    /// desktop and means "keep showing what you have", not "show nothing".
    pub fn next_picture(&mut self) -> Option<Picture> {
        let frame = self.video.latest()?;
        self.generation += 1;
        Some(Picture::new(
            frame.resolution,
            frame.pixels.to_vec(),
            self.generation,
        ))
    }

    /// Whether enough has been lost that a keyframe is worth asking for.
    ///
    /// Checked on the same cadence as redraws and acted on immediately: until
    /// one arrives the picture stays visibly broken.
    pub fn take_keyframe_request(&self) -> bool {
        self.video.wants_keyframe()
    }

    /// Ask the session to do something.
    ///
    /// Silently does nothing once the driver has stopped. That is not a
    /// swallowed error: the driver stops only when the session has ended, and
    /// the interface already learns that through [`Event::Ended`].
    pub fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            debug!("a command was sent to a session that has already ended");
        }
    }

    /// Adopt the stream the host reconfigured to.
    ///
    /// The old decoder was already stopped by the driver before this one
    /// started, so there is never a moment with two of them competing for the
    /// same datagrams.
    pub fn adopt(&mut self, config: SessionConfig, video: Arc<VideoStream>) {
        self.config = config;
        self.video = video;
        // The widget caches by generation, and the new stream's first frame
        // must not be mistaken for one it has already uploaded.
        self.generation += 1;
    }
}

/// Connect, log in, start a stream, and hand back a live session.
///
/// Every failure is a [`String`] because every one of them is shown to a
/// person. The messages are written for that: they say what went wrong in the
/// terms the person used, without leaking a path or a stack.
///
/// The [`Transport`] is the one endpoint this machine owns, not a fresh one:
/// dialling out and accepting share a key, and two endpoints holding the same
/// key would publish two discovery records for one machine, the second
/// overwriting the first.
pub async fn connect(
    transport: Transport,
    credentials: Credentials,
) -> Result<(Link, mpsc::UnboundedReceiver<Event>), String> {
    let mut client = timed("Dialling", Client::connect(
        &transport,
        &credentials.peer,
        &ClientConfig::default(),
    ))
    .await?;

    timed(
        "Logging in",
        client.authenticate(&credentials.username, &credentials.password),
    )
    .await?;

    let config = timed(
        "Starting the session",
        client.start_session(
            credentials.monitor,
            credentials.profile,
            credentials.max_resolution,
            credentials.audio,
        ),
    )
    .await?;

    // Opened before the picture, so the sink exists to hand over. A machine
    // with no output device is not a reason to refuse a session — the person
    // asked to see the host, and they still can.
    let audio = match config.audio {
        Some(format) => match AudioStream::start(format) {
            Ok(stream) => Some(Arc::new(stream)),
            Err(error) => {
                warn!(%error, "this machine cannot play the session's audio");
                None
            }
        },
        None => None,
    };
    let sink = audio.as_ref().map(|stream| stream.sink());

    let video = Arc::new(client.video(sink.clone()).map_err(describe)?);

    // A role without `MULTI_MONITOR` is refused the list, and that refusal is
    // not a reason to abandon a session that is otherwise working. An empty
    // list means the picker is not offered, which is the truth.
    let monitors = match client.monitors().await {
        Ok(monitors) => monitors,
        Err(error) => {
            debug!(%error, "this login may not enumerate displays");
            Vec::new()
        }
    };

    let permissions = client.permissions();
    let host_name = client.welcome().host_name.clone();
    let device = client.device_id();
    let route = client.route().kind;

    let (commands, inbox) = mpsc::unbounded_channel();
    let (events, updates) = mpsc::unbounded_channel();

    // Taken before the client moves into the driver task. The clone shares the
    // one QUIC connection, so file transfers ride the same path as the video
    // without a second dial and without going through the driver's queue.
    let client_session = client.session().clone();

    tokio::spawn(drive(client, transport, inbox, events, video.clone(), sink));

    Ok((
        Link {
            commands,
            session: client_session,
            video,
            audio,
            wanted_audio: credentials.audio,
            config,
            host_name,
            device,
            route,
            monitors,
            permissions,
            generation: 0,
        },
        updates,
    ))
}

/// Log in to a machine and open a shell on it, with no picture anywhere.
///
/// The standalone sibling of [`connect`]: same handshake, same login, but
/// instead of a display the connection carries a terminal. It is how the
/// Devices menu's `Terminal` entry reaches a machine that has no session open.
///
/// The stream alone does not keep a connection up. The client owns the
/// control stream the host watches for liveness, and the transport owns the
/// endpoint every stream rides on; letting either fall out of scope here is
/// how a terminal opened, printed nothing, and went quiet. So both move into
/// a holder task that pings like a session's driver does, and the returned
/// [`TerminalHold`] is the tab's end of it — dropping the tab drops the hold,
/// which says goodbye and closes the connection.
pub async fn connect_terminal(transport: Transport, credentials: TerminalCredentials) -> Result<OpenedTerminal, String> {
    let mut client = Client::connect(&transport, &credentials.peer, &ClientConfig::default())
        .await
        .map_err(describe)?;

    client
        .authenticate(&credentials.username, &credentials.password)
        .await
        .map_err(describe)?;

    let (terminal, events) = client
        .open_terminal(DEFAULT_COLS, DEFAULT_ROWS)
        .await
        .map_err(describe)?;

    let host_name = client.welcome().host_name.clone();
    let (release, released) = tokio::sync::oneshot::channel();
    tokio::spawn(hold_terminal(client, transport, released));

    Ok(OpenedTerminal {
        host_name,
        terminal,
        events: crate::net::wake::relay(events),
        hold: TerminalHold {
            _release: release,
        },
    })
}

/// A terminal that just opened, with everything its tab needs to own.
pub struct OpenedTerminal {
    pub host_name: String,
    pub terminal: pravera_client::Terminal,
    pub events: mpsc::UnboundedReceiver<pravera_client::TerminalEvent>,
    pub hold: TerminalHold,
}

impl std::fmt::Debug for OpenedTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedTerminal").field("host_name", &self.host_name).finish_non_exhaustive()
    }
}

/// The tab's grip on a terminal's connection. Nothing is ever sent on it:
/// dropping it is the signal, so no path out of a tab can forget to close.
pub struct TerminalHold {
    _release: tokio::sync::oneshot::Sender<()>,
}

/// Keeps a standalone terminal's connection alive until its tab lets go, or
/// until the connection fails on its own — the terminal's stream reports
/// that side of it.
async fn hold_terminal(mut client: Client, transport: Transport, mut released: tokio::sync::oneshot::Receiver<()>) {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // Fired or dropped, it means the same thing.
            _ = &mut released => break,
            _ = ping.tick() => {
                if let Err(error) = client.ping().await {
                    debug!(%error, "a terminal's connection failed");
                    drop(transport);
                    return;
                }
            }
        }
    }

    if let Err(error) = client.disconnect("terminal closed").await {
        debug!(%error, "the terminal's goodbye did not land");
    }
    drop(transport);
}

/// The pane a standalone terminal opens with, before the first real layout
/// corrects it.
pub const DEFAULT_COLS: u16 = 120;
pub const DEFAULT_ROWS: u16 = 30;

/// The connection facts a standalone terminal needs. No monitor, no profile,
/// no sound: those belong to a picture, and this is not one.
#[derive(Debug, Clone)]
pub struct TerminalCredentials {
    pub peer: PeerAddress,
    pub username: String,
    pub password: String,
}

/// The task that owns the client for the rest of the session.
async fn drive(
    mut client: Client,
    // Held, not used. An endpoint whose last handle goes out of scope closes
    // every connection it opened, and the session would end for no visible
    // reason.
    transport: Transport,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<Event>,
    mut video: Arc<VideoStream>,
    audio: Option<AudioSink>,
) {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // `None` for a role that may not share a clipboard, and for a machine with
    // none to share. Neither is a reason to refuse the rest of the session.
    let mut clipboards = Clipboards::open(client.permissions());
    let mut sweep = tokio::time::interval(CLIPBOARD_EVERY);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let ending = loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    // The interface dropped the link. Disconnect politely
                    // rather than letting the connection time out.
                    None | Some(Command::Disconnect) => break Ending::Requested,
                    Some(command) => {
                        if let Some(ending) =
                            apply(&mut client, command, &events, &mut video, &audio).await
                        {
                            break ending;
                        }
                    }
                }
            }
            _ = ping.tick() => {
                match client.ping().await {
                    Ok(rtt) => {
                        // A closed receiver means the interface is gone.
                        if events.send(Event::Latency(rtt)).is_err() {
                            break Ending::Requested;
                        }
                    }
                    Err(error) => break ending_for(error),
                }
            }
            _ = sweep.tick(), if clipboards.is_some() => {
                let sync = clipboards.as_mut().expect("guarded by the branch condition");
                match share_clipboard(&mut client, sync).await {
                    Ok(true) => {}
                    // It gave up. Every reason it does so is one that will not
                    // improve — a role without the permission, a clipboard that
                    // refused six times running — and none of them is a reason
                    // to end a session that is otherwise fine.
                    Ok(false) => clipboards = None,
                    Err(ending) => break ending,
                }
            }
        }
    };

    let requested = matches!(ending, Ending::Requested);
    let _ = events.send(Event::Ended(ending));

    if requested {
        if let Err(error) = client.disconnect("closed from the interface").await {
            debug!(%error, "the goodbye did not land");
        }
    }
    drop(transport);
}

/// One pass over both clipboards.
///
/// `Ok(false)` means clipboard sharing has stopped and the session carries on
/// without it. `Err` means the connection itself failed, which is the only
/// thing here worth ending a session over.
///
/// Push first, then poll. The order decides a simultaneous change on both
/// machines, and it decides it in favour of the one the person is physically
/// sitting at — and it means the poll that follows a push is answered
/// `Unchanged` rather than describing the paste that was just sent.
async fn share_clipboard(client: &mut Client, sync: &mut Clipboards) -> Result<bool, Ending> {
    if let Some(text) = sync.outgoing() {
        match client.set_clipboard(&text).await {
            Ok(seq) => sync.pushed(text, seq),
            Err(ClientError::Refused(error)) => {
                if !sync.refused(error) {
                    return Ok(false);
                }
            }
            Err(error) => return Err(ending_for(error)),
        }
    }

    if let Some(since) = sync.poll_from() {
        match client.clipboard_since(since).await {
            Ok(update) => sync.incoming(update),
            Err(ClientError::Refused(error)) => {
                if !sync.refused(error) {
                    return Ok(false);
                }
            }
            Err(error) => return Err(ending_for(error)),
        }
    }

    Ok(!sync.is_stopped())
}

/// Carry out one command. `Some` means the session is over.
async fn apply(
    client: &mut Client,
    command: Command,
    events: &mpsc::UnboundedSender<Event>,
    video: &mut Arc<VideoStream>,
    audio: &Option<AudioSink>,
) -> Option<Ending> {
    let reconfigured = match command {
        Command::Input(event) => {
            return client.send_input(event).await.err().map(ending_for);
        }
        Command::Keyframe => {
            return client.request_keyframe().await.err().map(ending_for);
        }
        // A terminal is not a reconfiguration and must not rebuild the video
        // path below: it is a second thing this connection is doing, opened
        // and handed back whole.
        Command::OpenTerminal { cols, rows } => {
            let opened = client
                .open_terminal(cols, rows)
                .await
                .map(|(terminal, output)| crate::net::Carry::new((terminal, crate::net::wake::relay(output))))
                .map_err(describe);
            if events.send(Event::TerminalOpened(opened)).is_err() {
                return Some(Ending::Requested);
            }
            return None;
        }
        Command::SendSas => {
            if let Err(error) = client.send_sas().await {
                warn!(%error, "Secure Attention Sequence was not delivered");
            } else {
                tracing::info!("Secure Attention Sequence sent");
            }
            return None;
        }
        Command::SetProfile(profile) => client.set_profile(profile).await,
        Command::SelectMonitor(monitor) => client.select_monitor(monitor).await,
        Command::Disconnect => return Some(Ending::Requested),
    };

    let config = match reconfigured {
        Ok(config) => config,
        Err(error) => return Some(ending_for(error)),
    };

    // Stop first, start second. Both decoders read from the same connection,
    // so an overlap would have them taking datagrams from each other and both
    // producing torn pictures.
    video.stop();
    let fresh = match client.video(audio.clone()) {
        Ok(fresh) => Arc::new(fresh),
        Err(error) => {
            warn!(%error, "the stream was reconfigured to something undecodable");
            return Some(ending_for(error));
        }
    };
    *video = fresh.clone();

    events
        .send(Event::Reconfigured {
            config: Box::new(config),
            video: fresh,
        })
        .err()
        .map(|_| Ending::Requested)
}

fn ending_for(error: ClientError) -> Ending {
    match error {
        ClientError::Ended(reason) => Ending::Remote(reason),
        other => Ending::Failed(describe(other)),
    }
}

/// Turn a client error into something worth showing a person.
///
/// Deliberately plain. A refused password says so and stops; it does not
/// suggest the account might not exist, because the host does not say and
/// guessing on its behalf would undo the reason it does not.
fn describe(error: ClientError) -> String {
    match error {
        ClientError::Denied => "That username and password were not accepted.".into(),
        ClientError::VersionMismatch { ours, theirs } => format!(
            "That machine is running a different version of Pravera \
             (it speaks protocol {theirs}, this one speaks {ours}). Update whichever is older."
        ),
        ClientError::NoSharedCodec { .. } => {
            "This machine has no video decoder the host can send to.".into()
        }
        ClientError::TooSoon(what) => format!("Not possible yet: {what}."),
        ClientError::Ended(reason) if reason.is_empty() => "The host ended the session.".into(),
        ClientError::Ended(reason) => format!("The host ended the session: {reason}"),
        ClientError::Transport(_) => {
            "Could not reach that machine. Check the connect code and that Pravera is \
             accepting connections there."
                .into()
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_password_does_not_hint_at_whether_the_account_exists() {
        // The host deliberately refuses to say. Restating it more helpfully
        // here would give back exactly what that refusal was protecting.
        let shown = describe(ClientError::Denied);
        let lowered = shown.to_lowercase();

        assert!(!lowered.contains("exist"), "{shown}");
        assert!(!lowered.contains("unknown user"), "{shown}");
        assert!(!lowered.contains("no such"), "{shown}");
        assert!(lowered.contains("not accepted"), "{shown}");
    }

    #[test]
    fn a_version_mismatch_says_which_end_to_update() {
        let shown = describe(ClientError::VersionMismatch { ours: 3, theirs: 7 });
        assert!(shown.contains('3') && shown.contains('7'), "{shown}");
        assert!(shown.to_lowercase().contains("update"), "{shown}");
    }

    #[test]
    fn an_unreachable_host_suggests_the_two_things_that_are_usually_wrong() {
        let shown = describe(ClientError::Transport(
            pravera_transport::TransportError::StreamClosed,
        ));
        assert!(shown.to_lowercase().contains("connect code"), "{shown}");
        assert!(shown.to_lowercase().contains("accepting"), "{shown}");
    }

    #[test]
    fn a_message_shown_to_a_person_never_carries_a_path_or_a_type_name() {
        // These strings land in a dialog. A `TransportError`'s inner cause is
        // written for an operator reading a log, not for someone trying to
        // connect to their own laptop.
        for error in [
            ClientError::Denied,
            ClientError::TooSoon("starting a stream"),
            ClientError::NoSharedCodec { offered: vec![] },
            ClientError::Transport(pravera_transport::TransportError::DatagramsUnsupported),
        ] {
            let shown = describe(error);
            assert!(!shown.contains("::"), "{shown}");
            assert!(!shown.contains('\\'), "{shown}");
            assert!(!shown.contains(".rs"), "{shown}");
        }
    }

    #[test]
    fn a_remote_goodbye_is_reported_as_the_host_ending_it_not_as_a_failure() {
        // The difference matters in the interface: a failure offers a retry
        // and an ordinary ending does not.
        let remote = ending_for(ClientError::Ended("shutting down".into()));
        assert!(!remote.is_failure());
        assert!(remote.message().contains("shutting down"));

        let broken = ending_for(ClientError::Transport(
            pravera_transport::TransportError::StreamClosed,
        ));
        assert!(broken.is_failure());
    }

    #[test]
    fn an_ending_with_no_reason_still_reads_as_a_sentence() {
        assert_eq!(
            Ending::Remote(String::new()).message(),
            "The host ended the session."
        );
        assert_eq!(Ending::Requested.message(), "Disconnected.");
    }
}

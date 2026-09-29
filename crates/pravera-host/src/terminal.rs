//! Serving a remote shell on a bulk stream.
//!
//! Runs alongside [`crate::serve`] like the file server does, with one
//! difference in who opens the stream: a terminal is created by the host, so
//! the host opens the bulk stream after [`pravera_proto::HostMessage::TerminalStarted`]
//! has gone out, and the client is the one accepting. See
//! `pravera_proto::terminal` for the conversation this module is the far side
//! of.
//!
//! ## The shell is a real console, not a pipe
//!
//! A shell on an anonymous pipe never draws a progress bar, never colours a
//! prompt, and never answers an arrow key — programs only do those things when
//! `isatty` says yes. So the host allocates a pseudoconsole (ConPTY), which is
//! Windows' own terminal-in-between: the shell sees a full console, and this
//! side sees the same VT byte stream a terminal emulator would. That stream is
//! forwarded verbatim; parsing it is the client's job (`pravera-term`).
//!
//! ## Where blocking goes
//!
//! Console pipes block, and a blocked worker thread stalls every task sharing
//! it. So the two directions of the console are pumped by dedicated blocking
//! threads — one reading the shell's output, one writing its input — and the
//! async task only ever shuttles between those channels and the network, where
//! waiting is the whole job.

use pravera_proto::{TerminalIn, TerminalOut};
use pravera_transport::{BulkStream, Session};
use tracing::{debug, info, warn};

/// Runs one terminal to completion on its own task.
///
/// Reached from `serve` when the state machine produced
/// `Effect::OpenTerminal`. The `TerminalStarted` reply has already gone out by
/// then, so every failure past this point keeps a promise the protocol made:
/// the stream is opened and immediately closed, and the client sees an empty
/// terminal that ended rather than one that silently never appeared.
pub async fn serve_terminal(session: Session, cols: u16, rows: u16) {
    let Ok(mut stream) = session.open_bulk().await else {
        debug!("a terminal's stream could not be opened; nothing to close");
        return;
    };

    #[cfg(windows)]
    {
        if let Err(error) = conpty::run(&mut stream, cols, rows).await {
            warn!(%error, "a terminal could not be started");
        }
    }
    #[cfg(not(windows))]
    {
        // Hosting is Windows-only today; the empty stream is still the honest
        // answer, and the client's "shell exited" is a truthful rendering of it.
        let _ = (cols, rows);
        let _ = &mut stream;
    }
}

#[cfg(windows)]
pub(crate) mod conpty {
    use std::io::{Read, Write};
    use std::os::windows::io::FromRawHandle;
    use std::sync::Arc;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
    };
    use windows::Win32::System::Pipes::CreatePipe;
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, TerminateProcess, UpdateProcThreadAttribute,
        WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    use super::*;
    use pravera_transport::Result;

    /// Chunks between the console and the network, and between the network and
    /// the console's input. A bounded channel is the backpressure: a shell
    /// producing output faster than the link carries it blocks in its read
    /// until the pipe drains, which is exactly what a terminal wants —
    /// nothing is dropped, and nothing grows without bound.
    const CHANNEL: usize = 32;

    /// One end of an anonymous pipe, wrapped so the compiler sees who owns it.
    struct PipeEnd(HANDLE);

    impl PipeEnd {
        /// Hand the handle to a `File`, which closes it from then on.
        ///
        /// The `forget` is the whole point. Without it this end's own `Drop`
        /// ran as the method returned and closed the handle the `File` had
        /// just been given, so the reader failed on its first read, the
        /// terminal reported the shell as killed (-1) while it was still
        /// starting, and the `File` later closed a handle value Windows may
        /// already have given to something else.
        fn into_file(self) -> std::fs::File {
            let raw = self.0 .0;
            std::mem::forget(self);
            unsafe { std::fs::File::from_raw_handle(raw as _) }
        }
    }

    impl Drop for PipeEnd {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn pipe() -> std::io::Result<(PipeEnd, PipeEnd)> {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        // Neither end inheritable. ConPTY duplicates the handles it is given,
        // and the child's stdio is wired by the pseudoconsole, not by these.
        // An inheritable end would be copied into every other process this
        // host starts while the terminal is open — `schtasks`, `pnputil`, any
        // `std::process::Command` — and a pipe with a stray writer in some
        // other process never reports end-of-file, so the terminal would
        // never learn that its shell had gone.
        unsafe {
            CreatePipe(&mut read, &mut write, None, 0)
                .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
        }
        Ok((PipeEnd(read), PipeEnd(write)))
    }

    /// The pseudoconsole handle, closed exactly once by whoever gets there
    /// first: the exit watcher, or the drop.
    ///
    /// Closing it is not tidying up — it is the only thing that ends the
    /// output. ConPTY holds its end of the output pipe open for as long as
    /// the console exists, so a shell that exits leaves the reader blocked
    /// forever and the client staring at a terminal that never says it ended.
    /// The watcher closes it the moment the shell exits; the reader then sees
    /// end-of-file, and the `Exited` message goes out.
    ///
    /// Stored as the handle's integer value so the slot is `Send`.
    #[derive(Clone)]
    struct Console(Arc<std::sync::Mutex<Option<isize>>>);

    impl Console {
        fn new(pty: HPCON) -> Console {
            Console(Arc::new(std::sync::Mutex::new(Some(pty.0 as isize))))
        }

        /// Close the console if it is still open. `ClosePseudoConsole` can
        /// wait for the output pipe to drain, which the reader thread is
        /// doing, so it must never be called from the thread that reads.
        fn close(&self) {
            let taken = self.0.lock().ok().and_then(|mut slot| slot.take());
            if let Some(pty) = taken {
                unsafe { ClosePseudoConsole(HPCON(pty as _)) };
            }
        }

        fn resize(&self, cols: u16, rows: u16) {
            if let Ok(slot) = self.0.lock() {
                if let Some(pty) = *slot {
                    unsafe {
                        let _ = ResizePseudoConsole(
                            HPCON(pty as _),
                            COORD {
                                X: cols as i16,
                                Y: rows as i16,
                            },
                        );
                    }
                }
            }
        }
    }

    /// The shell, its console, and everything needed to take them down.
    ///
    /// Dropping it terminates: a terminal that goes away — client closed the
    /// tab, session ended, task aborted — must not leave a shell running on a
    /// console nobody reads.
    struct Shell {
        console: Console,
        process: HANDLE,
        thread: HANDLE,
        /// Set by the watcher once the process has exited, so the drop path
        /// does not terminate an already-dead process for a second exit code.
        exited: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for Shell {
        fn drop(&mut self) {
            unsafe {
                if !self.exited.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = TerminateProcess(self.process, 1);
                }
                self.console.close();
                let _ = CloseHandle(self.process);
                let _ = CloseHandle(self.thread);
            }
        }
    }

    // A bag of Windows handles, which are plain integers with no thread
    // affinity: the resize comes from the async task, the exit wait runs on a
    // blocking thread, and the drop can land on whichever thread ends up
    // holding the last reference. Every access is to the handle value itself.
    unsafe impl Send for Shell {}

    /// A shell that started, on a console of the asked size, with the two
    /// `File`s this side of its console: what the shell writes and what it
    /// reads.
    fn spawn(cols: u16, rows: u16) -> std::io::Result<(Shell, std::fs::File, std::fs::File)> {
        let (in_read, in_write) = pipe()?;
        let (out_read, out_write) = pipe()?;

        let pty = unsafe {
            CreatePseudoConsole(
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
                in_read.0,
                out_write.0,
                0,
            )
        }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
        // ConPTY owns its ends now; ours would only leak.
        drop(in_read);
        drop(out_write);

        let spawn_result = unsafe { spawn_process(pty) };
        if let Err(error) = spawn_result {
            unsafe { ClosePseudoConsole(pty) };
            return Err(error);
        }

        Ok((
            spawn_result.expect("checked above"),
            // From here these are plain files, each the handle's one owner.
            in_write.into_file(),
            out_read.into_file(),
        ))
    }

    /// `CreateProcess` with the pseudoconsole threaded through the attribute
    /// list — the incantation that makes the child believe it has a console.
    ///
    /// # Safety
    /// Console and attribute-list juggling; every handle comes straight from
    /// the calls above and every failure path closes what it opened.
    unsafe fn spawn_process(pty: HPCON) -> std::io::Result<Shell> {
        let mut list_size = 0usize;
        // Asking for the size of an attribute list is signalled by an error,
        // not by a return value. Anything else at this call is a real failure.
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut list_size);
        if list_size == 0 {
            return Err(std::io::Error::other(
                "the attribute list size could not be determined",
            ));
        }
        let mut attribute_list = vec![0u8; list_size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(attribute_list.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut list_size)
            .map_err(|e| std::io::Error::from_raw_os_error(e.code().0))?;
        let attach_console = UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(pty.0 as *const std::ffi::c_void),
            std::mem::size_of::<usize>(),
            None,
            None,
        )
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0));

        if attach_console.is_err() {
            DeleteProcThreadAttributeList(list);
            return Err(attach_console.expect_err("checked above"));
        }

        let mut startup = STARTUPINFOEXW {
            StartupInfo: Default::default(),
            lpAttributeList: list,
        };
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // Explicitly no standard handles. Without the flag, a host whose own
        // stdio is redirected — the agent the service starts, or anything run
        // under a supervisor — hands those handles to the shell, which then
        // writes its prompt into the host's log instead of the console, and
        // the terminal on the other end stays blank.
        // Invalid rather than null, as WezTerm's pty does: a null handle is
        // a value some programs try to use, an invalid one is plainly absent.
        startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdOutput = INVALID_HANDLE_VALUE;
        startup.StartupInfo.hStdError = INVALID_HANDLE_VALUE;
        let mut info = PROCESS_INFORMATION::default();

        // PowerShell first: it is the shell Windows steers people towards, and
        // every supported build has it. `cmd` is the fallback for a machine
        // that has been trimmed of it, so the terminal still opens.
        let mut command = to_wide("powershell.exe -NoLogo");
        let launched = CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            None,
            PCWSTR::null(),
            &startup as *const STARTUPINFOEXW as *const _,
            &mut info,
        );
        if let Err(error) = launched {
            warn!(%error, "PowerShell could not be started; trying cmd");
            command = to_wide("cmd.exe");
            if let Err(error) = CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(command.as_mut_ptr())),
                None,
                None,
                false,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                None,
                PCWSTR::null(),
                &startup as *const STARTUPINFOEXW as *const _,
                &mut info,
            ) {
                DeleteProcThreadAttributeList(list);
                return Err(std::io::Error::from_raw_os_error(error.code().0));
            }
        }
        DeleteProcThreadAttributeList(list);

        let shell = Shell {
            console: Console::new(pty),
            process: info.hProcess,
            thread: info.hThread,
            exited: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        // One thread per shell, parked in a wait for its whole life. Cheap,
        // and it turns the process handle into an exit with no polling. The
        // handle travels as its integer value: `HANDLE` is not `Send` (it
        // wraps a raw pointer), but the handle itself has no thread affinity.
        //
        // When the shell exits, the console goes too: see [`Console`] for why
        // that is what lets the terminal end.
        let signal = shell.exited.clone();
        let console = shell.console.clone();
        let process_value = shell.process.0 as isize;
        std::thread::spawn(move || unsafe {
            let process = HANDLE(process_value as *mut _);
            WaitForSingleObject(process, u32::MAX);
            signal.store(true, std::sync::atomic::Ordering::Release);
            console.close();
        });

        Ok(shell)
    }

    fn to_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Serve one terminal to completion.
    pub(super) async fn run(stream: &mut BulkStream, cols: u16, rows: u16) -> Result<()> {
        let (shell, mut shell_input, mut shell_output) = match spawn(cols, rows) {
            Ok(running) => running,
            Err(error) => {
                // The promise: a stream that opens and ends at once. The client
                // sees a terminal that was offered and immediately closed.
                warn!(%error, "the shell could not be started");
                return Ok(());
            }
        };
        let shell = Arc::new(std::sync::Mutex::new(Some(shell)));
        info!(cols, rows, "a terminal opened");

        // Out of the console, towards the network. A blocking thread with a
        // bounded channel: when the link is slower than the shell, the shell's
        // pipe fills and the shell itself waits — the shape a real terminal has.
        let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(CHANNEL);
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 8 * 1024];
            loop {
                match shell_output.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if outgoing_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        warn!(%error, "reading the shell's console failed");
                        break;
                    }
                }
            }
        });

        // Into the console, from the network. Writes block when the console is
        // busy, so they belong on their own thread like every other blocking
        // call here.
        //
        // Unbounded on purpose. A bounded channel here would make the relay
        // wait on a busy console, and a console is busy precisely when its
        // shell is blocked writing output that only the relay can drain — the
        // two would wait on each other forever. What arrives is typing and
        // pastes the client already chunks, so the queue stays small.
        let (incoming_tx, mut incoming_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        std::thread::spawn(move || {
            while let Some(bytes) = incoming_rx.blocking_recv() {
                if let Err(error) = shell_input.write_all(&bytes) {
                    warn!(%error, "writing to the shell's console failed");
                    break;
                }
                let _ = shell_input.flush();
            }
        });

        let result = pump(stream, &shell, &mut outgoing_rx, incoming_tx).await;
        info!("a terminal closed");

        // Teardown order matters. The reader thread may be parked sending a
        // chunk nobody will take; dropping the receiver frees it. Only then is
        // the console closed, and off this thread: on older Windows builds
        // `ClosePseudoConsole` waits for the output to drain, and a runtime
        // worker parked on that would stall every task sharing it.
        drop(outgoing_rx);
        tokio::task::spawn_blocking(move || drop(shell));
        result
    }

    /// The relay: console output to the stream, stream input to the console,
    /// resizes to the console, and the first of {shell exited, stream ended}
    /// ends the whole thing. Both ends close the same way, so whichever side
    /// goes first tears down the other rather than leaving it running.
    async fn pump(
        stream: &mut BulkStream,
        shell: &Arc<std::sync::Mutex<Option<Shell>>>,
        outgoing_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
        incoming_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                chunk = outgoing_rx.recv() => {
                    match chunk {
                        Some(bytes) => {
                            stream.send(&TerminalOut::Output { bytes }).await?;
                        }
                        // The console pipe closed: the shell is gone, or about
                        // to be. Its exit code was fetched by the watcher.
                        None => {
                            let code = exit_code(&shell);
                            stream.send(&TerminalOut::Exited { exit_code: code }).await?;
                            stream.flush().await?;
                            return Ok(());
                        }
                    }
                }
                incoming = stream.recv::<TerminalIn>() => {
                    match incoming {
                        Ok(message) if message.is_well_formed() => match message {
                            TerminalIn::Input { bytes } => {
                                // Never waits: see the note on the channel.
                                let _ = incoming_tx.send(bytes);
                            }
                            TerminalIn::Resize { cols, rows } => {
                                if let Ok(slot) = shell.lock() {
                                    if let Some(shell) = slot.as_ref() {
                                        shell.console.resize(cols, rows);
                                    }
                                }
                            }
                        },
                        // Malformed input is ignored rather than fatal: a
                        // hostile or broken peer does not get to kill its own
                        // shell with it.
                        Ok(_) => {}
                        // The stream went away. Closing this side is what kills
                        // the shell — that is the protocol's definition of
                        // "closed", and the `Shell` guard does the killing when
                        // the last handle to it drops.
                        Err(error) => {
                            debug!(%error, "a terminal's stream was closed by the client");
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    /// What the watcher thread read off the process handle. Always produces
    /// something: a shell that never reported gets "killed", because from the
    /// client's side a console that vanished with no code *was* a teardown,
    /// and saying so is honest.
    fn exit_code(shell: &std::sync::Mutex<Option<Shell>>) -> i32 {
        // Take the guard for good: this is the end of the terminal either way.
        if let Ok(mut slot) = shell.lock() {
            if let Some(shell) = slot.take() {
                let mut code = 0u32;
                if let Err(error) = unsafe { GetExitCodeProcess(shell.process, &mut code) } {
                    warn!(%error, "the shell's exit code could not be read");
                    return TerminalOut::KILLED;
                }
                // STILL_ACTIVE would mean the process outlived its pipe, which
                // does not happen through normal exits; report a teardown.
                if code == 259 {
                    warn!("the console's output ended while the shell was still running");
                    return TerminalOut::KILLED;
                }
                return code as i32;
            }
        }
        TerminalOut::KILLED
    }
}

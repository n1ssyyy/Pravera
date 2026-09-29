//! The service body: what runs between the SCM starting it and stopping it.
//!
//! A Windows service is a callback contract, not a program with a `main`. The
//! SCM calls [`service_main`] on a thread of its choosing; that has to register
//! a control handler and then report its state on time, or Windows decides the
//! service hung and kills it. So the loop below does nothing slow between
//! status reports, and everything it does do is a syscall that returns at once.
//!
//! # The loop
//!
//! Once a second: is there a console session, and is there a live agent in it?
//! If the session changed, the old agent belongs to a desktop that is gone and
//! a new one is launched. If the agent died in a session that is still there,
//! it is launched again after a pause — not immediately, because an agent that
//! crashes on startup would otherwise be relaunched a thousand times a minute
//! and fill the event log instead of the screen.
//!
//! Session changes also arrive as control events, which is faster than
//! noticing on the next tick, but the tick is what makes it correct: control
//! events can be missed, and a poll cannot.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tracing::{error, info, warn};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use crate::agent::{self, Agent};

/// How often the loop looks at the world.
const TICK: Duration = Duration::from_secs(1);

/// How long to wait before starting an agent again after one exited.
///
/// An agent that fails immediately — no display, a corrupt settings file —
/// would otherwise be restarted every second forever. Five seconds is short
/// enough that a genuine crash is invisible to somebody connecting, and long
/// enough that a permanent failure is a line in the log every five seconds
/// rather than a flood.
const RESTART_AFTER: Duration = Duration::from_secs(5);

/// Set by the control handler when Windows asks the service to stop.
static STOPPING: AtomicBool = AtomicBool::new(false);
/// Bumped by the control handler on any session change, so the loop knows to
/// look again without waiting for its next tick.
static SESSION_CHANGED: AtomicU32 = AtomicU32::new(0);
/// The status handle, as an integer because a raw handle is not `Sync`.
static STATUS: AtomicU32 = AtomicU32::new(0);
static STATUS_HIGH: AtomicU32 = AtomicU32::new(0);

fn status_handle() -> SERVICE_STATUS_HANDLE {
    let low = u64::from(STATUS.load(Ordering::Relaxed));
    let high = u64::from(STATUS_HIGH.load(Ordering::Relaxed));
    SERVICE_STATUS_HANDLE(((high << 32) | low) as *mut core::ffi::c_void)
}

fn set_status_handle(handle: SERVICE_STATUS_HANDLE) {
    let value = handle.0 as u64;
    STATUS.store(value as u32, Ordering::Relaxed);
    STATUS_HIGH.store((value >> 32) as u32, Ordering::Relaxed);
}

/// Tell the SCM where the service is up to.
///
/// `wait_hint` is a promise: Windows will wait that long for the next report
/// before deciding the service is stuck. It is only meaningful for the
/// *pending* states, and it is zero everywhere else so as not to claim a delay
/// that is not coming.
fn report(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, wait_hint: u32) {
    static CHECKPOINT: AtomicU32 = AtomicU32::new(0);

    let pending = state == SERVICE_START_PENDING || state == SERVICE_STOP_PENDING;
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            // Session changes are the whole reason this service exists in the
            // shape it does. Shutdown is accepted so the agent is told to go
            // before the machine does.
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
        } else {
            0
        },
        dwWin32ExitCode: NO_ERROR.0,
        dwServiceSpecificExitCode: 0,
        // Must advance on every pending report, or Windows reads a repeated
        // checkpoint as no progress.
        dwCheckPoint: if pending {
            CHECKPOINT.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        },
        dwWaitHint: wait_hint,
    };

    unsafe { SetServiceStatus(status_handle(), &status) }.ok();
}

/// What Windows calls when it wants something.
///
/// Runs on an SCM thread, not the service's own, so it does the least possible:
/// set a flag and return. Anything slower risks the SCM's own timeout, and the
/// work belongs to the loop anyway.
unsafe extern "system" fn control(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOPPING.store(true, Ordering::Relaxed);
            report(SERVICE_STOP_PENDING, 5_000);
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            SESSION_CHANGED.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    NO_ERROR.0
}

/// The service's body, called by the SCM.
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows::core::PWSTR) {
    let name = windows::core::HSTRING::from(crate::SERVICE_NAME);
    let handle = match unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None)
    } {
        Ok(handle) => handle,
        // Nothing can be reported, because reporting needs the handle that
        // just failed to arrive. Returning ends the service.
        Err(_) => return,
    };
    set_status_handle(handle);

    report(SERVICE_START_PENDING, 10_000);
    report(SERVICE_RUNNING, 0);
    info!("Pravera service started");

    let outcome = supervise();

    match outcome {
        Ok(()) => report(SERVICE_STOPPED, 0),
        Err(error) => {
            error!(%error, "the Pravera service stopped on an error");
            let status = SERVICE_STATUS {
                dwServiceType: SERVICE_WIN32_OWN_PROCESS,
                dwCurrentState: SERVICE_STOPPED,
                dwWin32ExitCode: ERROR_SERVICE_SPECIFIC_ERROR.0,
                dwServiceSpecificExitCode: 1,
                ..Default::default()
            };
            unsafe { SetServiceStatus(status_handle(), &status) }.ok();
        }
    }
}

/// Keep an agent alive in whichever session has the console.
///
/// Separate from the SCM plumbing so the decision it makes is readable on its
/// own, and so the same logic could be driven by a test or by a foreground run
/// without a service manager anywhere.
fn supervise() -> anyhow::Result<()> {
    let mut agent: Option<Agent> = None;
    let mut retry_after: Option<Instant> = None;
    let mut last_change = SESSION_CHANGED.load(Ordering::Relaxed);

    while !STOPPING.load(Ordering::Relaxed) {
        let change = SESSION_CHANGED.load(Ordering::Relaxed);
        if change != last_change {
            last_change = change;
            // A session change means whatever desktop the agent was on may no
            // longer exist. Clearing the backoff too, because this is new
            // information rather than another go at the same thing.
            retry_after = None;
        }

        let console = agent::console_session();

        // The session went away, or moved to a different user. Either way the
        // agent belongs to a desktop that is no longer the one being shared.
        if let Some(live) = &agent {
            if Some(live.session) != console || !live.is_alive() {
                let session = live.session;
                let still_alive = live.is_alive();
                let code = live.exit_code();
                agent = None;
                if still_alive {
                    info!(session, "the console moved; letting that agent go");
                } else if code == Some(pravera_core::lifecycle::EXIT_ALREADY_RUNNING as u32) {
                    info!(
                        session,
                        "Pravera already covers this session (single instance); not restarting"
                    );
                    // Do not schedule a retry — the user's own Pravera holds
                    // the port and the service would otherwise respawn every
                    // 5s against a running copy, focus-stealing each time.
                    retry_after = None;
                } else {
                    warn!(session, code = ?code, "Pravera stopped in that session");
                    retry_after = Some(Instant::now() + RESTART_AFTER);
                }
            }
        }

        if agent.is_none() {
            if let Some(session) = console {
                let ready = retry_after.is_none_or(|at| Instant::now() >= at);
                if ready {
                    // Another Pravera already holds the single-instance port
                    // (the user's own launch) — do not compete with it.
                    // Probing the port is cheaper than launching and reading
                    // EXIT_ALREADY_RUNNING, and avoids the focus-steal from the
                    // second's flag write. Only `AddrInUse` counts; any other
                    // bind error is not another instance.
                    let port_held = match std::net::TcpListener::bind(format!(
                        "127.0.0.1:{}",
                        pravera_core::lifecycle::SINGLE_INSTANCE_PORT
                    )) {
                        Ok(l) => {
                            drop(l);
                            false
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => true,
                        Err(e) => {
                            tracing::warn!(%e, "service port probe failed; assuming free");
                            false
                        }
                    };
                    if port_held {
                        // Don't spam: just wait for the port to be free or the
                        // console session to change. The user's Pravera will
                        // show the window itself on next launch via the flag.
                        std::thread::sleep(TICK);
                        continue;
                    }
                    match agent::launch(session) {
                        Ok(started) => {
                            info!(session, "Pravera started in the console session");
                            agent = Some(started);
                            retry_after = None;
                        }
                        Err(error) => {
                            warn!(session, %error, "could not start Pravera in that session");
                            retry_after = Some(Instant::now() + RESTART_AFTER);
                        }
                    }
                }
            }
        }

        std::thread::sleep(TICK);
    }

    // The agent is deliberately left running. It belongs to a signed-in
    // person's desktop, and stopping the service — which is often the first
    // half of an upgrade — should not close a window somebody is using or drop
    // a session somebody is working in. A reboot takes it with everything else.
    info!("Pravera service stopping");
    Ok(())
}

/// Hand this process to the service control manager.
pub fn run() -> ExitCode {
    // Logging goes to stderr, which the SCM discards. That is deliberate for
    // now: the alternative is a log file this service would have to rotate,
    // and every interesting failure is also reported through the service
    // status the `status` command reads.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let name = windows::core::HSTRING::from(crate::SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: windows::core::PWSTR(name.as_ptr() as *mut u16),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];

    match unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } {
        Ok(()) => ExitCode::SUCCESS,
        // The usual cause by far: somebody ran `pravera-service run` at a
        // prompt. Saying so is more use than the error code.
        Err(error) => {
            eprintln!(
                "pravera-service: this command is what the service manager calls, not something \
                 to run by hand.\nRegister it with: pravera-service install\n\n({error})"
            );
            ExitCode::FAILURE
        }
    }
}

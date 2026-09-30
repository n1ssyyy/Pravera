//! Doing one thing as an administrator, and nothing else.
//!
//! Pravera is an ordinary program on purpose: its manifest says `asInvoker`, so
//! double-clicking it never raises a UAC prompt and the person's own desktop is
//! not run with more rights than it needs. A handful of jobs do need the rights
//! (registering the boot service, installing the virtual display driver), and
//! the answer to that used to be a sentence telling the person to right-click
//! the file and start over. That was worse than it sounds: a second launch is
//! handed to the first one that is already running, so an elevated start
//! usually never got as far as doing the job.
//!
//! What happens now is that the running Pravera starts a copy of itself with
//! the `runas` verb, which is what makes Windows show the consent prompt. That
//! copy is not the application. It does one named job, writes down what
//! happened, and exits: it opens no window, takes no single-instance lock, and
//! never touches the log the running Pravera is writing. The running Pravera
//! waits for it off the interface thread, then asks the system what is true now
//! rather than trusting a code, so a job that half-worked is shown as what it
//! is.
//!
//! ## What comes back
//!
//! An exit code, and a line of text in a file the caller named. The code is
//! the answer ([`code`]); the text is the words to show, and may be missing
//! (a different administrator's temp directory is not always writable), so the
//! caller never depends on it.
//!
//! Declining the prompt is not a failure. Windows reports it as
//! `ERROR_CANCELLED` from the launch itself, before any process exists, and it
//! is passed on as [`Outcome::Declined`] so the interface can say "nothing
//! changed" instead of showing an error.

use std::path::Path;

/// A job that needs administrator rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    /// Register the boot service, or point it at this copy.
    RegisterService,
    /// Remove the boot service.
    UnregisterService,
    /// Add the 1920x1080 virtual display, installing its driver if needed.
    AddDisplay,
    /// Stage the virtual display driver.
    InstallDriver,
}

impl Job {
    pub const ALL: [Job; 4] = [
        Job::RegisterService,
        Job::UnregisterService,
        Job::AddDisplay,
        Job::InstallDriver,
    ];

    /// The command-line flag that makes this executable do the job and exit.
    pub const fn flag(self) -> &'static str {
        match self {
            Job::RegisterService => "--register-service",
            Job::UnregisterService => "--unregister-service",
            Job::AddDisplay => "--add-display",
            Job::InstallDriver => "--install-driver",
        }
    }

    pub fn from_flag(flag: &str) -> Option<Job> {
        Job::ALL.into_iter().find(|job| job.flag() == flag)
    }
}

/// The flag that names the file the finished job writes its sentence to.
pub const RESULT_FLAG: &str = "--result";

/// What the elevated copy exits with.
pub mod code {
    /// The job is done.
    pub const DONE: i32 = 0;
    /// The job was tried and did not work; the result file says why.
    pub const FAILED: i32 = 1;
    /// This copy did not get administrator rights, so it did not try.
    pub const NOT_ELEVATED: i32 = 2;
}

/// How a request for an elevated job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done. The text is a detail worth showing ("1920x1080"), possibly empty.
    Done(String),
    /// The person said no to the Windows prompt. Nothing was changed, and
    /// nothing is wrong.
    Declined,
    /// It did not work, in words fit to show.
    Failed(String),
}

// ------------------------------------------------------ the elevated copy

/// Run `job` in this process, which is the elevated copy, and return the exit
/// code. Called from [`crate::run`] before anything else that could start an
/// application.
pub fn perform(job: Job, result: Option<&Path>) -> i32 {
    let (code, text) = match attempt(job) {
        Ok(detail) => (code::DONE, detail),
        Err(Failure::NotElevated) => (
            code::NOT_ELEVATED,
            "Windows did not give Pravera administrator rights.".to_string(),
        ),
        Err(Failure::Refused(reason)) => (code::FAILED, reason),
    };
    if let Some(path) = result {
        // Best effort. The exit code is the answer; this is only its wording.
        let _ = std::fs::write(path, &text);
    }
    code
}

enum Failure {
    NotElevated,
    Refused(String),
}

fn attempt(job: Job) -> Result<String, Failure> {
    if !pravera_capture::is_elevated() {
        return Err(Failure::NotElevated);
    }
    match job {
        Job::RegisterService => match pravera_service::ensure_installed() {
            pravera_service::Installed::Registered
            | pravera_service::Installed::Repointed
            | pravera_service::Installed::Unchanged => Ok(String::new()),
            pravera_service::Installed::Refused(reason) => Err(Failure::Refused(reason)),
            pravera_service::Installed::NotElevated => Err(Failure::NotElevated),
            pravera_service::Installed::NotRegistered | pravera_service::Installed::Elsewhere => {
                Err(Failure::Refused(
                    "the service manager gave no answer".to_string(),
                ))
            }
        },
        Job::UnregisterService => {
            // Already gone is what was asked for.
            if !pravera_service::is_registered() {
                return Ok(String::new());
            }
            pravera_service::uninstall()
                .map(|_| String::new())
                .map_err(Failure::Refused)
        }
        Job::AddDisplay => pravera_capture::add_virtual_display(1920, 1080, 60)
            .map(|shown| format!("{}x{}", shown.resolution.width, shown.resolution.height))
            .map_err(|error| Failure::Refused(error.to_string())),
        Job::InstallDriver => pravera_capture::try_auto_install()
            .map(|staged| {
                if staged.is_some() {
                    "installed".to_string()
                } else {
                    "already installed".to_string()
                }
            })
            .map_err(Failure::Refused),
    }
}

// ------------------------------------------------------- the running copy

/// Ask for `job` to be done with administrator rights, and wait for it.
///
/// Blocking, and possibly for a long time: it sits behind a consent prompt a
/// person has to read. Call it off the interface thread.
///
/// An already-elevated Pravera does the job itself; there is nobody to ask.
pub fn request(job: Job) -> Outcome {
    if pravera_capture::is_elevated() {
        return match attempt(job) {
            Ok(detail) => Outcome::Done(detail),
            Err(Failure::NotElevated) => Outcome::Failed("Pravera has no administrator rights.".into()),
            Err(Failure::Refused(reason)) => Outcome::Failed(reason),
        };
    }
    request_with_prompt(job)
}

#[cfg(windows)]
fn request_with_prompt(job: Job) -> Outcome {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return Outcome::Failed(format!("Pravera could not find its own location: {error}"))
        }
    };

    // Created here, empty, so that it belongs to the person whose Pravera this
    // is and the elevated copy only has to write into it.
    let result = std::env::temp_dir().join(format!(
        "pravera-elevated-{}-{}.txt",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis())
            .unwrap_or(0)
    ));
    let _ = std::fs::write(&result, "");

    let arguments = arguments_for(job, &result);
    tracing::info!(flag = job.flag(), "asking Windows for administrator rights");
    let launched = platform::launch_and_wait("runas", &exe, &arguments, WAIT_AT_MOST);

    let text = std::fs::read_to_string(&result).unwrap_or_default();
    let _ = std::fs::remove_file(&result);

    outcome_of(launched, text.trim())
}

#[cfg(not(windows))]
fn request_with_prompt(_job: Job) -> Outcome {
    Outcome::Failed("This is only available on Windows.".into())
}

/// Longer than anybody takes to answer a prompt, short enough that a prompt
/// left open overnight is not a waiting task forever.
const WAIT_AT_MOST: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The command line the elevated copy is started with.
pub fn arguments_for(job: Job, result: &Path) -> String {
    format!("{} {} \"{}\"", job.flag(), RESULT_FLAG, result.display())
}

/// How a launch ended, before any words are chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launched {
    /// The process ran and exited with this code.
    Exited(i32),
    /// The person declined the prompt (`ERROR_CANCELLED`).
    Declined,
    /// Windows would not start it, in its words.
    CouldNotStart(String),
    /// Started, and still running when the wait ran out.
    StillRunning,
}

/// Turn how a launch ended, and what the job wrote, into what is shown.
pub fn outcome_of(launched: Launched, text: &str) -> Outcome {
    match launched {
        Launched::Declined => Outcome::Declined,
        Launched::CouldNotStart(reason) => Outcome::Failed(format!(
            "Windows would not start the administrator step: {reason}"
        )),
        Launched::StillRunning => Outcome::Failed(
            "The administrator step is still running. Check Settings again in a moment.".into(),
        ),
        Launched::Exited(code::DONE) => Outcome::Done(text.to_string()),
        Launched::Exited(code) => Outcome::Failed(if text.is_empty() {
            match code {
                code::NOT_ELEVATED => "Windows did not give Pravera administrator rights.".to_string(),
                other => format!("The administrator step failed (exit code {other})."),
            }
        } else {
            text.to_string()
        }),
    }
}

#[cfg(windows)]
mod platform {
    use super::Launched;
    use std::path::Path;
    use std::time::Duration;
    use windows::core::{HRESULT, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    /// `ERROR_CANCELLED`: what `ShellExecuteEx` reports when the consent
    /// prompt is declined.
    const ERROR_CANCELLED: u32 = 1223;

    /// Whether this is the answer Windows gives to a declined prompt.
    pub fn is_declined(error: &windows::core::Error) -> bool {
        error.code() == HRESULT::from_win32(ERROR_CANCELLED)
    }

    /// Start `exe` with `verb`, hidden, and wait for it to finish.
    pub fn launch_and_wait(verb: &str, exe: &Path, arguments: &str, wait: Duration) -> Launched {
        let verb = HSTRING::from(verb);
        let file = HSTRING::from(exe.as_os_str());
        let parameters = HSTRING::from(arguments);

        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            // The handle is what makes waiting for it possible at all.
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };

        // SAFETY: `info` is fully initialised and its strings outlive the call.
        if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
            return if is_declined(&error) {
                Launched::Declined
            } else {
                Launched::CouldNotStart(error.message())
            };
        }
        let process = info.hProcess;
        if process.is_invalid() {
            // Started by something that is not a new process (an existing
            // handler took the request). There is nothing to wait for.
            return Launched::CouldNotStart("no process was started".into());
        }

        // SAFETY: `process` is the handle `ShellExecuteExW` just gave us, and is
        // closed exactly once below.
        let waited = unsafe { WaitForSingleObject(process, wait.as_millis().min(u32::MAX as u128 - 1) as u32) };
        let launched = if waited == WAIT_OBJECT_0 {
            let mut exit = 0u32;
            // SAFETY: as above.
            match unsafe { GetExitCodeProcess(process, &mut exit) } {
                Ok(()) => Launched::Exited(exit as i32),
                Err(error) => Launched::CouldNotStart(error.message()),
            }
        } else {
            Launched::StillRunning
        };
        // SAFETY: as above.
        unsafe {
            let _ = CloseHandle(process);
        }
        launched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    #[test]
    fn every_job_has_its_own_flag_and_finds_itself_again() {
        for job in Job::ALL {
            assert_eq!(Job::from_flag(job.flag()), Some(job));
        }
        let mut flags: Vec<_> = Job::ALL.iter().map(|job| job.flag()).collect();
        flags.sort_unstable();
        flags.dedup();
        assert_eq!(flags.len(), Job::ALL.len(), "two jobs share a flag");
        assert_eq!(Job::from_flag("--hidden"), None);
    }

    #[test]
    fn the_flags_do_not_collide_with_the_ones_the_service_and_installer_own() {
        for job in Job::ALL {
            for taken in [
                pravera_service::SERVICE_FLAG,
                pravera_service::AGENT_FLAG,
                "--install",
                "--uninstall",
                "--setup",
                "--updated",
            ] {
                assert_ne!(job.flag(), taken);
            }
        }
    }

    #[test]
    fn the_result_file_survives_a_path_with_spaces() {
        let arguments = arguments_for(
            Job::RegisterService,
            Path::new(r"C:\Users\Ada Lovelace\AppData\Local\Temp\r.txt"),
        );
        let parsed = crate::install::Cli::parse(
            arguments
                .split_inclusive(' ')
                .take(2)
                .map(|piece| OsString::from(piece.trim())),
        );
        assert_eq!(parsed.job, Some(Job::RegisterService));
        assert!(arguments.ends_with("r.txt\""), "{arguments}");
        assert!(arguments.contains("\"C:\\Users\\Ada Lovelace"), "{arguments}");
    }

    #[test]
    fn a_declined_prompt_is_not_a_failure() {
        assert_eq!(outcome_of(Launched::Declined, ""), Outcome::Declined);
    }

    #[test]
    fn a_finished_job_is_done_and_keeps_its_detail() {
        assert_eq!(
            outcome_of(Launched::Exited(code::DONE), "1920x1080"),
            Outcome::Done("1920x1080".into())
        );
    }

    #[test]
    fn a_failed_job_shows_its_own_words_and_falls_back_to_the_code() {
        assert_eq!(
            outcome_of(Launched::Exited(code::FAILED), "the driver is unsigned"),
            Outcome::Failed("the driver is unsigned".into())
        );
        match outcome_of(Launched::Exited(code::FAILED), "") {
            Outcome::Failed(text) => assert!(text.contains('1'), "{text}"),
            other => panic!("{other:?}"),
        }
        match outcome_of(Launched::Exited(code::NOT_ELEVATED), "") {
            Outcome::Failed(text) => assert!(text.contains("administrator"), "{text}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_job_that_was_never_started_is_a_failure_not_a_success() {
        assert!(matches!(
            outcome_of(Launched::CouldNotStart("no".into()), ""),
            Outcome::Failed(_)
        ));
        assert!(matches!(outcome_of(Launched::StillRunning, ""), Outcome::Failed(_)));
    }

    #[test]
    fn an_unelevated_copy_refuses_without_touching_anything() {
        // Only meaningful where this test run is not itself elevated: an
        // elevated run would go on to do the real job.
        if pravera_capture::is_elevated() {
            return;
        }
        for job in Job::ALL {
            let dir = std::env::temp_dir().join(format!("pravera-elevate-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let result = dir.join("result.txt");
            assert_eq!(perform(job, Some(&result)), code::NOT_ELEVATED, "{job:?}");
            let said = std::fs::read_to_string(&result).unwrap();
            assert!(said.contains("administrator"), "{said}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_launched_process_is_waited_for_and_its_exit_code_comes_back() {
        // The launch, wait and exit-code plumbing, without a prompt: the same
        // call the elevated copy goes through, with the ordinary `open` verb
        // and a shell that does nothing but exit. Hidden, so no window.
        let cmd = std::env::var_os("COMSPEC").map(PathBuf::from).expect("COMSPEC is set");
        let launched = platform::launch_and_wait(
            "open",
            &cmd,
            "/c exit 3",
            std::time::Duration::from_secs(30),
        );
        assert_eq!(launched, Launched::Exited(3));
    }

    #[cfg(windows)]
    #[test]
    fn declining_the_prompt_is_recognised_from_the_error_windows_gives() {
        use windows::core::HRESULT;
        let declined = windows::core::Error::from(HRESULT::from_win32(1223));
        assert!(platform::is_declined(&declined));
        let other = windows::core::Error::from(HRESULT::from_win32(5));
        assert!(!platform::is_declined(&other));
    }
}

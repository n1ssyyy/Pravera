//! Starting with the machine.
//!
//! A homelab box reboots at three in the morning and nobody is there to open
//! anything. For it to be reachable afterwards, Pravera has to start itself.
//!
//! ## At sign-in, not at boot
//!
//! What this registers runs when a person signs in, because that is the only
//! moment a user-session program can start, and it is a real limit rather than
//! a detail: a machine sitting at the sign-in screen after a reboot is running
//! no Pravera and is not reachable. Making a machine reachable from cold boot
//! means a service running as SYSTEM, which is P5.
//!
//! Between now and then the arrangement that works is a machine that signs in
//! by itself. That is a Windows setting rather than something Pravera can do
//! for someone — deciding on its own that a machine should sign in without a
//! password would be a much larger thing than starting an app.
//!
//! ## What gets registered
//!
//! The path to this executable and `--hidden`, nothing else. Moving or
//! rebuilding the executable leaves an entry pointing at where it used to be,
//! so [`enable`] rewrites the entry every time rather than checking whether one
//! already exists.

use std::path::PathBuf;

/// The name of the entry, on both platforms.
const NAME: &str = "Pravera";

/// The flag the registered command passes, so a machine that starts itself
/// comes up as an icon rather than a window nobody asked for.
pub const HIDDEN_FLAG: &str = "--hidden";

/// Whether Pravera is registered to start at sign-in.
pub fn is_enabled() -> bool {
    platform::is_enabled().unwrap_or(false)
}

/// Whether the start-at-sign-in entry starts `exe` rather than some other
/// copy of Pravera. Where the entry cannot be read back, any entry counts.
pub fn registered_for(exe: &std::path::Path) -> bool {
    if !is_enabled() {
        return false;
    }
    platform::describe()
        .map(|entry| {
            // The task XML escapes `&`; nothing else a path holds.
            let entry = entry.replace("&amp;", "&").to_lowercase();
            entry.contains(&exe.display().to_string().to_lowercase())
        })
        .unwrap_or(true)
}

/// Register, or remove the registration.
///
/// The error is the sentence shown to the person, because there is nothing
/// they can do with a registry error code.
pub fn set(enabled: bool) -> Result<(), String> {
    let result = if enabled {
        platform::enable(&executable()?)
    } else {
        platform::disable()
    };

    match &result {
        Ok(()) => tracing::info!(enabled, "the sign-in entry was changed"),
        Err(error) => tracing::warn!(enabled, %error, "the sign-in entry could not be changed"),
    }

    result.map_err(|error| {
        if enabled {
            format!("Pravera could not register itself to start at sign-in: {error}")
        } else {
            format!("Pravera could not remove its start-at-sign-in entry: {error}")
        }
    })
}

/// Where this executable is.
fn executable() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|error| format!("Pravera could not find its own location: {error}"))
}

/// The command line to register, for the platforms that register a string.
///
/// Quoted, because `C:\Program Files\...` contains a space and an unquoted
/// entry would run `C:\Program.exe` with the rest as arguments — the oldest
/// trick in the Windows book, and a privilege-escalation vector rather than
/// merely a bug.
///
/// Windows no longer goes through here: a scheduled task carries the
/// executable and its arguments as separate fields, so there is nothing to
/// quote and nothing to be misparsed. Its equivalent checks live beside the
/// task definition in `platform`.
#[cfg(not(windows))]
fn command_line(exe: &std::path::Path) -> String {
    format!("\"{}\" {HIDDEN_FLAG}", exe.display())
}

#[cfg(windows)]
mod platform {
    //! A Task Scheduler entry that runs at sign-in.
    //!
    //! This used to be `HKCU\...\Run`, which is simpler, per-user, and visible
    //! in Task Manager's Startup tab. It stopped being an option the moment
    //! Pravera started asking for elevation: Windows will not raise a Run entry
    //! through UAC at sign-in, because there is nobody to answer the prompt yet,
    //! so an elevated executable registered there is one that silently never
    //! starts.
    //!
    //! A scheduled task marked `HighestAvailable` starts elevated with no
    //! prompt, which is the same arrangement every other remote-access tool
    //! uses before it has a real service. It is still findable and removable:
    //! it appears in Task Scheduler under its own name, and [`disable`] removes
    //! it.
    //!
    //! ## Whose privileges the task asks for
    //!
    //! Only an elevated process may create a `HighestAvailable` task. Windows
    //! answers "Access is denied" to an ordinary one, and Pravera is an
    //! ordinary process by design (its manifest says `asInvoker`), so asking
    //! for `HighestAvailable` from the Settings switch made the switch fail
    //! for every person who did not happen to have started Pravera as an
    //! administrator. The task therefore asks for what its creator has: an
    //! elevated Pravera registers a task that starts elevated, and an ordinary
    //! one registers a task that starts ordinary, which is what it would have
    //! got by being double-clicked. A person who wants Pravera elevated at
    //! sign-in gets that from the boot service, which is the mechanism built
    //! for it.
    //!
    //! The task is written as XML rather than assembled from `schtasks` flags,
    //! because the flags leave the defaults in place and two of those defaults
    //! are wrong for this: a task is stopped after three days, and is not
    //! started at all on battery. A laptop host would come up only when plugged
    //! in, and any host would stop being reachable after a long weekend.
    //!
    //! Only elements whose defaults are wrong are written. Two more that the
    //! schema knows — `DisallowStartOnRemoteAppSession` and
    //! `UseUnifiedSchedulingEngine` — default to exactly what this task wants,
    //! and `schtasks`' XML parser rejects them on some Windows builds
    //! ("unexpected node (35,7)"), so naming them costs the registration and
    //! buys nothing. Omitting an element is the schema's answer for "default",
    //! and the default is what we mean.

    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    /// Keeps `schtasks` from flashing a console window on a desktop somebody is
    /// using. The same flag the Tailscale scan needs, for the same reason.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The task's name. A leading `\` would put it at the root of the library,
    /// which is where Windows' own tasks live; this sits in the same place a
    /// person would look for anything they installed.
    fn task_name() -> String {
        super::NAME.to_string()
    }

    fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
        Command::new("schtasks.exe")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
    }

    /// The task's definition, for telling which executable it starts.
    pub fn describe() -> std::io::Result<String> {
        let output = schtasks(&["/Query", "/TN", &task_name(), "/XML"])?;
        let out = String::from_utf8_lossy(&output.stdout).into_owned();
        if out.contains("<Task") {
            return Ok(out);
        }
        Ok(String::from_utf8_lossy(&output.stderr).into_owned())
    }

    pub fn is_enabled() -> std::io::Result<bool> {
        // Check the scheduled task — but a task that exists yet is disabled
        // must read as "off", otherwise Settings would claim Pravera starts
        // at sign-in when Task Scheduler would not launch it.
        let task_output = schtasks(&["/Query", "/TN", &task_name(), "/XML"])?;
        if task_output.status.success() {
            let xml = String::from_utf8_lossy(&task_output.stdout);
            // Fallback: some builds return XML on stderr when the output
            // is piped.
            let xml = if xml.trim().is_empty() {
                let alt = String::from_utf8_lossy(&task_output.stderr);
                if alt.contains("<Task") { alt } else { xml }
            } else {
                xml
            };
            // Disabled tasks contain `<Enabled>false</Enabled>` in
            // `<Settings>` (and also in the trigger). A task that is
            // present but disabled should not count as autostart.
            let is_disabled = xml.contains("<Enabled>false</Enabled>")
                || xml.contains("<Enabled>false<");
            if is_disabled && xml.contains("<Task") {
                // Confirm it's the Settings block — cheap heuristic:
                // disabled tasks always have at least one false.
                return Ok(false);
            }
            // Task exists and is not disabled.
            return Ok(true);
        }
        // No such task — also check the legacy Run key. Earlier versions
        // used `HKCU\...\Run\Pravera`; a machine that upgraded and never
        // cleared it would still start via that entry, and `is_enabled`
        // must not lie about it.
        if is_run_entry_enabled() {
            return Ok(true);
        }
        // `/Query` exits non-zero when there is no such task, which is the
        // question being asked rather than a failure to answer it.
        // Distinguish "no task" from "access denied" — the latter needs
        // elevation and should bubble up as an error so the caller can
        // surface it, but for `is_enabled` (polled every frame) we treat
        // it as "unknown — assume off" rather than failing the draw.
        let stderr = String::from_utf8_lossy(&task_output.stderr).to_ascii_lowercase();
        if stderr.contains("access is denied") || stderr.contains("denied") {
            return Ok(false);
        }
        Ok(false)
    }

    fn is_run_entry_enabled() -> bool {
        use windows::core::w;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
            REG_VALUE_TYPE,
        };
        let mut key = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
                None,
                KEY_READ,
                &mut key,
            )
        };
        if opened != ERROR_SUCCESS {
            return false;
        }
        let name: Vec<u16> = super::NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut ty = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        let queried = unsafe {
            RegQueryValueExW(
                key,
                windows::core::PCWSTR(name.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut size),
            )
        };
        unsafe {
            let _ = RegCloseKey(key);
        }
        queried == ERROR_SUCCESS && size > 0
    }

    /// Whether this process holds the rights a `HighestAvailable` task needs
    /// from its creator.
    fn elevated() -> bool {
        pravera_capture::is_elevated()
    }

    pub fn enable(exe: &Path) -> std::io::Result<()> {
        // A machine that ran a version of Pravera from before the task existed
        // still has that version's sign-in entry, pointing at an executable
        // Windows will not raise through UAC. It never starts, but it is listed
        // in Task Manager's Startup tab as though it does, and leaving it there
        // means anybody diagnosing "why did it not come up" is looking at the
        // wrong entry. Removing it is part of registering the thing that
        // replaced it.
        forget_the_old_run_entry();

        create(&task_name(), exe, elevated())
    }

    /// Register the task under `name`.
    ///
    /// `name` is a parameter so a test can register one that is not the real
    /// one and remove it again.
    fn create(name: &str, exe: &Path, highest: bool) -> std::io::Result<()> {
        let xml = definition_named(name, exe, highest)?;

        // UTF-16 with a byte-order mark. `schtasks /XML` reads the file as
        // ANSI otherwise, and a path with a non-ASCII character in it — a
        // username with an accent is enough — comes back mangled.
        let mut bytes = vec![0xFF, 0xFE];
        for unit in xml.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }

        // Per process, so two Praveras registering at once do not overwrite each
        // other's definition between the write and the read.
        let path = std::env::temp_dir().join(format!("pravera-autostart-{}.xml", std::process::id()));
        std::fs::write(&path, &bytes)?;

        let result = schtasks(&[
            "/Create",
            "/TN",
            name,
            "/XML",
            &path.to_string_lossy(),
            // Replace whatever is registered. The executable may have moved
            // since last time, and a task pointing at where Pravera used to be
            // is worse than no task: it looks registered and does nothing.
            "/F",
        ]);

        // The definition is not secret, but leaving files in the temp
        // directory is how temp directories get the way they are.
        let _ = std::fs::remove_file(&path);

        let output = result?;
        if output.status.success() {
            return Ok(());
        }

        // `schtasks` reports failures on stdout as often as stderr.
        let said = String::from_utf8_lossy(&output.stderr);
        let said = if said.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            said
        };
        let low = said.to_ascii_lowercase();
        if low.contains("access is denied") || low.contains("denied") {
            // Only reachable when a task made by an elevated Pravera is in the
            // way: an ordinary process may replace its own task, not that one.
            return Err(std::io::Error::other(
                "Windows would not replace the existing sign-in entry, because an administrator's Pravera made it. Remove it in Task Scheduler (it is called Pravera), or turn it off from an elevated Pravera, then try again.",
            ));
        }
        Err(std::io::Error::other(said.trim().to_string()))
    }

    pub fn disable() -> std::io::Result<()> {
        // "Do not start with the machine" has to mean every way Pravera ever
        // arranged to, not just the current one.
        forget_the_old_run_entry();

        let output = schtasks(&["/Delete", "/TN", &task_name(), "/F"])?;
        if output.status.success() {
            // Confirm it really went — `schtasks /Delete` can report
            // success yet leave a disabled stub on some builds when not
            // elevated. If it still queries as present, try disabling.
            if !is_enabled().unwrap_or(true) {
                return Ok(());
            }
        }

        // Already gone is the state that was asked for.
        if !is_enabled().unwrap_or(true) {
            return Ok(());
        }

        // Try disabling instead of deleting — deleting a
        // `HighestAvailable` task from an unelevated `asInvoker` process
        // is denied, but disabling may still succeed via the same ACL,
        // and a disabled task does not start at logon.
        let disable_out = schtasks(&["/Change", "/TN", &task_name(), "/DISABLE"]);
        if let Ok(out) = disable_out {
            if out.status.success() || !is_enabled().unwrap_or(true) {
                return Ok(());
            }
        }

        // If we are here, the delete was denied (likely need elevation).
        // Surface a sentence that tells the person what to do, rather
        // than the raw `ERROR: Access is denied.`.
        let said = String::from_utf8_lossy(&output.stderr);
        let said = if said.trim().is_empty() {
            String::from_utf8_lossy(&output.stdout)
        } else {
            said
        };
        let low = said.to_ascii_lowercase();
        if low.contains("access is denied") || low.contains("denied") {
            return Err(std::io::Error::other(
                "Windows would not remove the sign-in entry, because an administrator's Pravera made it. Remove it in Task Scheduler (it is called Pravera), or turn it off from an elevated Pravera.",
            ));
        }
        Err(std::io::Error::other(said.trim().to_string()))
    }

    /// Delete the sign-in entry earlier versions used, if one is still there.
    ///
    /// Deliberately silent. There is normally nothing to delete, the caller has
    /// asked for something else entirely, and a person who has never run an
    /// older build has no use for the news that a key they never had is still
    /// absent.
    fn forget_the_old_run_entry() {
        use windows::core::w;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        };

        let mut key = HKEY::default();
        let opened = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
                None,
                KEY_SET_VALUE,
                &mut key,
            )
        };
        if opened != ERROR_SUCCESS {
            return;
        }

        // The name is the same on both sides of the change, which is what makes
        // this findable at all.
        let name: Vec<u16> = super::NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let removed =
            unsafe { RegDeleteValueW(key, windows::core::PCWSTR(name.as_ptr())) } == ERROR_SUCCESS;
        unsafe {
            let _ = RegCloseKey(key);
        }

        if removed {
            tracing::info!("removed the sign-in entry an earlier version of Pravera left behind");
        }
    }

    /// Who the task runs as.
    ///
    /// `DOMAIN\user`, which is what the logon trigger and the principal both
    /// need. Without it the task is registered against whoever happened to
    /// create it, which is the same account today and not necessarily the
    /// account that signs in tomorrow.
    fn current_user() -> String {
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        let user = std::env::var("USERNAME").unwrap_or_default();
        if domain.is_empty() {
            user
        } else {
            format!("{domain}\\{user}")
        }
    }

    /// XML entities, so a path or a username containing `&` or `<` produces a
    /// task rather than a parse error.
    fn escape(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    }

    /// `highest` asks for the highest privileges the account has, which only an
    /// elevated creator is allowed to; otherwise the task runs as an ordinary
    /// process of the signed-in user.
    #[cfg(test)]
    fn definition(exe: &Path, highest: bool) -> std::io::Result<String> {
        definition_named(&task_name(), exe, highest)
    }

    fn definition_named(name: &str, exe: &Path, highest: bool) -> std::io::Result<String> {
        let run_level = if highest {
            "HighestAvailable"
        } else {
            "LeastPrivilege"
        };
        let user = escape(&current_user());
        let command = escape(&exe.display().to_string());
        let flag = super::HIDDEN_FLAG;

        Ok(format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Pravera when {user} signs in, so this machine can be reached remotely.</Description>
    <URI>\{name}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{flag}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            name = escape(name),
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn an_elevated_creator_registers_a_task_that_starts_elevated() {
            // Without this the task starts Pravera unelevated, and an
            // unelevated Pravera cannot inject input into an elevated window,
            // which is most of why it asks for elevation at all.
            let xml = definition(Path::new(r"C:\Pravera\pravera.exe"), true).expect("a definition");
            assert!(
                xml.contains("<RunLevel>HighestAvailable</RunLevel>"),
                "{xml}"
            );
        }

        #[test]
        fn an_ordinary_creator_registers_a_task_it_is_allowed_to_create() {
            // The bug: this was `HighestAvailable` unconditionally, and Task
            // Scheduler answers "Access is denied" to an ordinary process
            // asking for that, so the Settings switch could never turn on.
            let xml = definition(Path::new(r"C:\Pravera\pravera.exe"), false).expect("a definition");
            assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"), "{xml}");
            assert!(!xml.contains("HighestAvailable"), "{xml}");
        }

        #[test]
        #[ignore = "creates and deletes a real scheduled task; run with --ignored"]
        fn an_ordinary_process_can_register_and_remove_the_sign_in_task() {
            // The whole path against the real Task Scheduler, under a name
            // that is not the real entry, and only when this test is not
            // itself elevated: an elevated run would prove nothing about the
            // case that failed.
            if elevated() {
                eprintln!("skipped: this run is elevated");
                return;
            }
            let name = format!("PraveraTest-{}", std::process::id());
            let exe = std::env::current_exe().expect("this test binary has a path");

            // What the switch used to ask for, and what Windows said to it.
            let refused = create(&format!("{name}-highest"), &exe, true)
                .expect_err("an ordinary process may not register a HighestAvailable task");
            assert!(
                refused.to_string().contains("administrator"),
                "the refusal should say what to do about it: {refused}"
            );

            create(&name, &exe, false).expect("an ordinary process may register its own task");

            let queried = schtasks(&["/Query", "/TN", &name, "/XML"]).expect("schtasks runs");
            let listed = String::from_utf8_lossy(&queried.stdout).into_owned();
            let removed = schtasks(&["/Delete", "/TN", &name, "/F"]).expect("schtasks runs");

            assert!(queried.status.success(), "the task was not registered: {listed}");
            // Task Scheduler writes `LeastPrivilege` back as the absence of a
            // run level, because it is the default.
            assert!(!listed.contains("HighestAvailable"), "{listed}");
            assert!(listed.contains("<LogonTrigger>"), "{listed}");
            assert!(removed.status.success(), "the test task was left behind: {name}");
        }

        #[test]
        fn the_task_is_not_stopped_after_three_days() {
            // `schtasks` defaults to a 72-hour execution limit. A host that
            // stops being reachable after a long weekend is a host nobody can
            // rely on.
            let xml = definition(Path::new(r"C:\Pravera\pravera.exe"), false).expect("a definition");
            assert!(
                xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
                "{xml}"
            );
        }

        #[test]
        fn a_laptop_still_starts_pravera_on_battery() {
            let xml = definition(Path::new(r"C:\Pravera\pravera.exe"), false).expect("a definition");
            assert!(
                xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"),
                "{xml}"
            );
            assert!(
                xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"),
                "{xml}"
            );
        }

        #[test]
        fn the_registered_task_starts_hidden() {
            let xml = definition(Path::new(r"C:\Pravera\pravera.exe"), false).expect("a definition");
            assert!(
                xml.contains(&format!(
                    "<Arguments>{}</Arguments>",
                    super::super::HIDDEN_FLAG
                )),
                "{xml}"
            );
        }

        #[test]
        fn a_path_with_an_ampersand_in_it_still_produces_valid_xml() {
            // `C:\Users\A & B\pravera.exe` is a legal path and an illegal XML
            // document, and the failure would be a task that never registers.
            let xml = definition(Path::new(r"C:\Users\A & B\pravera.exe"), false).expect("a definition");
            assert!(xml.contains(r"C:\Users\A &amp; B\pravera.exe"), "{xml}");
            assert!(
                !xml.contains("B\\pravera.exe</Command>\n      <Arguments>&"),
                "{xml}"
            );
        }
    }
}

#[cfg(not(windows))]
mod platform {
    //! The freedesktop autostart directory: a `.desktop` file in
    //! `~/.config/autostart`, which every desktop environment reads.
    //!
    //! This starts Pravera when a graphical session begins, and a headless
    //! Linux box has no graphical session at all — so on Linux this is for a
    //! desktop machine somebody signs into. Unattended Linux hosting is a
    //! systemd unit, which is `pravera-service` in P5.

    use std::path::PathBuf;

    fn entry() -> std::io::Result<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .ok_or_else(|| std::io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
        Ok(base.join("autostart").join("pravera.desktop"))
    }

    pub fn is_enabled() -> std::io::Result<bool> {
        Ok(entry()?.exists())
    }

    /// The entry's text, for telling which executable it starts.
    pub fn describe() -> std::io::Result<String> {
        std::fs::read_to_string(entry()?)
    }

    pub fn enable(exe: &std::path::Path) -> std::io::Result<()> {
        let command = super::command_line(exe);
        let path = entry()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &path,
            format!(
                "[Desktop Entry]\n\
                 Type=Application\n\
                 Name=Pravera\n\
                 Comment=Remote desktop\n\
                 Exec={command}\n\
                 Terminal=false\n\
                 X-GNOME-Autostart-enabled=true\n"
            ),
        )
    }

    pub fn disable() -> std::io::Result<()> {
        match std::fs::remove_file(entry()?) {
            Ok(()) => Ok(()),
            // Already gone is the state that was asked for.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn the_registered_command_is_quoted() {
        // An unquoted `C:\Program Files\Pravera\pravera.exe` runs
        // `C:\Program.exe` if such a thing exists, with the rest as arguments.
        // That is a way in, not just a broken shortcut.
        let command = command_line(&executable().expect("this test binary has a path"));
        assert!(command.starts_with('"'), "{command}");
        assert!(command.contains("\" "), "{command}");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_registered_command_starts_hidden() {
        // Without this, a machine that starts itself opens a window nobody
        // asked for, on a machine that may have no screen to open it on.
        let command = command_line(&executable().expect("this test binary has a path"));
        assert!(command.ends_with(HIDDEN_FLAG), "{command}");
    }

    #[cfg(not(windows))]
    #[test]
    fn the_registered_command_names_this_executable() {
        let command = command_line(&executable().expect("this test binary has a path"));
        let exe = executable().unwrap();
        assert!(
            command.contains(&exe.display().to_string()),
            "{command} does not name {}",
            exe.display()
        );
    }

    #[test]
    fn asking_the_state_never_panics() {
        // Called on every draw of the Settings screen, on machines where the
        // registry key or the config directory may not be reachable.
        let _ = is_enabled();
    }
}

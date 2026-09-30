//! Virtual displays for machines with no monitor.
//!
//! A machine with nothing plugged into a graphics output has no screen worth
//! capturing: Windows either lists nothing, or lists a placeholder (a desktop
//! no active path drives, or the `Generic Non-PnP Monitor` it invents for an
//! output with no EDID). The picture would be black, and hosting would be
//! refused — which is exactly the machine an unattended host exists for.
//!
//! The correct fix is an Indirect Display Driver: a signed user-mode driver
//! that advertises a monitor the way a dock advertises a real one. Pravera
//! does not ship its own driver; it adopts
//! [`MttVDD`](https://github.com/VirtualDrivers/Virtual-Display-Driver)
//! (release 24.12.24), which is maintained, signed, and installed through the
//! normal driver store.
//!
//! ## Automatic when elevated, explicit otherwise
//!
//! The whole package is embedded in this binary (`EMBEDDED_PACKAGE`), so a
//! portable `pravera.exe` needs nothing beside it. An elevated process (the
//! agent the service starts with the user's elevated token, or a run as
//! administrator) does everything itself. A standard-user process cannot stage
//! a driver or create a device, so it reports that it needs administrator rights
//! instead of failing with a cryptic code; the interface then asks Windows for
//! them (see `pravera_ui::elevate`). There is deliberately no
//! test-pattern fallback: a headless machine streams a real display or refuses.
//!
//! What is deliberately not done unconditionally is creating the display. The
//! display is a root-enumerated device node, and creating that node adds a
//! monitor to whoever's desktop this is — so the automatic paths only create it
//! when [`needs_virtual_display`] says the machine has no real screen.
//!
//! ## How the driver is driven
//!
//! Four things have to be true, and each was once the reason nothing appeared:
//!
//! 1. **The publisher is trusted.** The package is signed by *SignPath
//!    Foundation*, not by Microsoft, so the first install on a machine asks
//!    "Would you like to install this device software?". On a headless box
//!    nobody answers and the install waits forever. The catalogue's signer is
//!    added to the machine's Trusted Publishers first — the same step the
//!    upstream installer takes — and every SetupAPI call runs non-interactive,
//!    so a refusal is an error instead of a hang.
//! 2. **The settings file is well-formed.** The driver reads
//!    `vdd_settings.xml` from `HKLM\SOFTWARE\MikeTheTech\VirtualDisplayDriver`
//!    `VDDPATH` (default `C:\VirtualDisplayDriver`) when its device starts,
//!    with no error handling: a malformed file leaves it a monitor with no
//!    modes, and a non-numeric value crashes the driver host. The file is
//!    validated and, when it would not serve, rewritten atomically.
//! 3. **The device node exists.** `SetupDiCreateDeviceInfoW` with
//!    `DICD_GENERATE_ID` takes a bare device id (`MttVDD`, giving
//!    `ROOT\MTTVDD\0000`). Passing `Root\MttVDD` there is
//!    `ERROR_INVALID_DEVINST_NAME` (`0x800F0205`), which is why creation
//!    always failed before. The hardware id `Root\MttVDD` is a separate
//!    property, and it is what the driver is matched and bound on.
//! 4. **Its monitor is the screen.** On a machine whose only other display is
//!    a placeholder, the virtual monitor is made primary so the taskbar and
//!    windows land on the screen someone is actually looking at.
//!
//! Nodes are found by hardware id, so a display installed by the upstream
//! installer (`ROOT\DISPLAY\000n`) is adopted rather than duplicated.

use std::path::{Path, PathBuf};

use pravera_core::Resolution;

use crate::{CaptureError, CaptureOptions, CaptureSource, Display, DisplayId, FrameStream, Result};

/// The signed driver package, embedded so the executable is self-sufficient.
///
/// `Signed-Driver-v24.12.24-x64.zip` from `VirtualDrivers/Virtual-Display-Driver`
/// release 25.5.2. The `.inf`/`.cat` prove the signature; the `.dll` is the
/// whole UMDF driver; the settings XML is the upstream default (one monitor,
/// 1920x1080 among its modes, 60 Hz among its global rates).
const EMBEDDED_PACKAGE: &[(&str, &[u8])] = &[
    (
        "MttVDD.inf",
        include_bytes!("../../pravera-ui/assets/idd/MttVDD.inf"),
    ),
    (
        "MttVDD.dll",
        include_bytes!("../../pravera-ui/assets/idd/MttVDD.dll"),
    ),
    (
        "mttvdd.cat",
        include_bytes!("../../pravera-ui/assets/idd/mttvdd.cat"),
    ),
    (
        "vdd_settings.xml",
        include_bytes!("../../pravera-ui/assets/idd/vdd_settings.xml"),
    ),
];

/// Where the driver reads its settings when `VDDPATH` does not say otherwise.
#[cfg(windows)]
const DEFAULT_SETTINGS_DIR: &str = r"C:\VirtualDisplayDriver";

/// The hardware id the driver's INF matches (`[Standard.NTamd64]`).
#[cfg(windows)]
const DRIVER_HW_ID: &str = r"Root\MttVDD";

/// The root-enumerated device id a new node is created under. No enumerator
/// prefix: `DICD_GENERATE_ID` adds `ROOT\` and the instance suffix itself.
#[cfg(windows)]
const DRIVER_DEVICE_ID: &str = "MttVDD";

/// How long to wait for the virtual monitor to reach the desktop after its
/// device starts. Device start is asynchronous, and the first start after an
/// install also loads the UMDF host.
#[cfg(windows)]
const ARRIVAL: std::time::Duration = std::time::Duration::from_secs(20);

/// After a failed setup, how long the automatic paths wait before trying the
/// whole sequence again. The explicit button ignores it.
#[cfg(windows)]
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// One setup at a time, process-wide. The startup check, the eight-second poll
/// and a starting session can all ask at once; two concurrent creations would
/// make two device nodes, and so two monitors.
#[cfg(windows)]
static SETUP: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// The last setup failure, so the poll does not rerun a failing driver
/// install every eight seconds.
#[cfg(windows)]
static LAST_FAILURE: parking_lot::Mutex<Option<(std::time::Instant, String)>> =
    parking_lot::Mutex::new(None);

/// Where the embedded package is laid out for installation: machine-wide, so
/// the service's agent and a person's own launch use the same copy.
pub fn idd_package_dir() -> Option<PathBuf> {
    Some(pravera_core::paths::service_data_dir().join("idd"))
}

/// The crate's bundled `assets/idd/` when running from `cargo run`.
fn bundled_assets_dir() -> Option<PathBuf> {
    // `CARGO_MANIFEST_DIR` is set at compile time for this crate — the absolute
    // path to `crates/pravera-capture/`. Its sibling is `pravera-ui`.
    let manifest = env!("CARGO_MANIFEST_DIR");
    let candidate = Path::new(manifest)
        .parent()
        .and_then(|crates| crates.parent())
        .map(|root| root.join("crates/pravera-ui/assets/idd"))
        .or_else(|| Some(Path::new(manifest).join("../pravera-ui/assets/idd")));
    candidate.filter(|p| p.is_dir())
}

/// An on-disk copy of the package, if one exists: next to the executable, in
/// the crate's `assets/idd/`, or where [`idd_package_dir`] lays it out.
///
/// Informational. Installation always uses the embedded copy, laid out fresh,
/// so a stale or partial directory can never be what gets installed.
pub fn find_bundled_package() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(exe) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("idd")))
    {
        candidates.push(exe);
    }
    if let Some(assets) = bundled_assets_dir() {
        candidates.push(assets);
    }
    if let Some(data) = idd_package_dir() {
        candidates.push(data);
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("MttVDD.inf").is_file())
}

/// Whether a package is available. Always true: it is embedded.
pub fn has_bundled_package() -> bool {
    !EMBEDDED_PACKAGE.is_empty()
}

/// Write the embedded package out to `dir`, creating it if needed.
fn extract_embedded_package(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, bytes) in EMBEDDED_PACKAGE {
        let path = dir.join(name);
        // Skip when the bytes are already there: rewriting a catalogue for no
        // reason is asking for an antivirus argument.
        if let Ok(existing) = std::fs::read(&path) {
            if existing.as_slice() == *bytes {
                continue;
            }
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

fn embedded(name: &str) -> &'static [u8] {
    EMBEDDED_PACKAGE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .unwrap_or(b"")
}

/// Lay the embedded package out where installation reads it.
#[cfg(windows)]
fn materialise_package() -> std::result::Result<PathBuf, String> {
    let dir = idd_package_dir().ok_or("no location for the driver package")?;
    extract_embedded_package(&dir)
        .map_err(|e| format!("laying out the driver package in {}: {e}", dir.display()))?;
    Ok(dir)
}

// ------------------------------------------------------------------ detection

/// Whether the MttVDD package is in the driver store.
///
/// Staged is not the same as working: a staged package with no device node
/// shows no monitor. [`is_idd_usable`] is the question that decides that.
/// Always `false` off Windows.
pub fn is_idd_installed() -> bool {
    #[cfg(windows)]
    {
        // `pnputil /add-driver` and `UpdateDriverForPlugAndPlayDevices` both
        // stage into `FileRepository\mttvdd.inf_amd64_<hash>`, named after
        // the original INF.
        let Ok(root) = std::env::var("SystemRoot") else {
            return false;
        };
        let repo = Path::new(&root).join("System32/DriverStore/FileRepository");
        std::fs::read_dir(repo)
            .map(|entries| {
                entries.flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .starts_with("mttvdd.inf_")
                })
            })
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether a device node for the driver exists and its driver has started.
///
/// That is the state in which the driver is showing its monitor (or will, the
/// moment the settings it read allow it).
pub fn is_idd_usable() -> bool {
    #[cfg(windows)]
    {
        nodes::find()
            .iter()
            .any(|node| node.present && node.started && node.problem == 0)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Whether this process can stage a driver and create a device node.
///
/// Both require the Administrators group *enabled* in the token: an elevated
/// administrator, or `SYSTEM`. A standard user, and an administrator's
/// filtered (non-elevated) token, both answer `false`.
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SID_IDENTIFIER_AUTHORITY,
    };
    // SAFETY: the SID is allocated, checked and freed here; nothing escapes.
    unsafe {
        let mut sid = PSID::default();
        // SECURITY_NT_AUTHORITY, SECURITY_BUILTIN_DOMAIN_RID (32),
        // DOMAIN_ALIAS_RID_ADMINS (544).
        let authority = SID_IDENTIFIER_AUTHORITY {
            Value: [0, 0, 0, 0, 0, 5],
        };
        if AllocateAndInitializeSid(&authority, 2, 32, 544, 0, 0, 0, 0, 0, 0, &mut sid).is_err() {
            return false;
        }
        let mut member = windows::core::BOOL(0);
        let elevated = CheckTokenMembership(None, sid, &mut member).is_ok() && member.as_bool();
        let _ = FreeSid(sid);
        elevated
    }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// The sentence the UI shows when driver work is refused for want of
/// administrator rights.
pub fn needs_elevation_message() -> &'static str {
    "The virtual display driver needs administrator rights to install. In Pravera, open Settings, then Displays, and choose Add 1920x1080 display: Windows asks for permission once. The display then appears by itself and stays across reboots. Hosting refuses until then rather than streaming a test pattern."
}

/// What the automatic paths should do about this machine's screens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Headless {
    /// A real screen exists, or this is not the kind of session that can
    /// have a virtual one. Leave everything alone.
    No,
    /// Nothing but placeholders, or nothing at all: create the display.
    NeedsDisplay,
    /// The virtual display exists but a placeholder is still primary, so the
    /// taskbar is on a screen nobody can see.
    NeedsPrimary,
}

fn headless_state() -> Headless {
    #[cfg(windows)]
    {
        use crate::win32::monitors::{self, Kind};
        // An RDP session's desktop is the remote client's; the virtual
        // display would appear on the console, where this session is not.
        if monitors::is_remote_session() {
            return Headless::No;
        }
        match monitors::inventory() {
            Err(CaptureError::NoDisplays) => Headless::NeedsDisplay,
            // Could not look. Creating a device on a guess is not an option.
            Err(_) => Headless::No,
            Ok(entries) => {
                if entries.iter().any(|e| e.kind == Kind::Physical) {
                    return Headless::No;
                }
                match entries.iter().find(|e| e.kind == Kind::Virtual) {
                    None => Headless::NeedsDisplay,
                    Some(virt) if !virt.display.primary => Headless::NeedsPrimary,
                    Some(_) => Headless::No,
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        Headless::No
    }
}

/// Whether this machine needs the virtual display: it has no real screen
/// (nothing at all, or only Windows' placeholders), or it has the virtual one
/// but a placeholder is still primary.
///
/// This is the gate every automatic path uses. It never says yes on a desktop
/// with a real monitor, whatever the driver's state — staged-without-a-device
/// is the normal state of a desktop machine, not a fault to repair.
pub fn needs_virtual_display() -> bool {
    headless_state() != Headless::No
}

/// A snapshot for the Settings card, cheap enough to take on a poll and far
/// too expensive to take on every frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtualDisplayStatus {
    /// The package is in the driver store.
    pub driver_staged: bool,
    /// A device node exists and its driver started.
    pub device_ready: bool,
    /// The virtual monitor is on the desktop right now.
    pub display_active: bool,
    /// See [`needs_virtual_display`].
    pub needs_display: bool,
    /// Privileged work can happen from this process.
    pub elevated: bool,
}

/// Take a [`VirtualDisplayStatus`].
pub fn virtual_display_status() -> VirtualDisplayStatus {
    VirtualDisplayStatus {
        driver_staged: is_idd_installed(),
        device_ready: is_idd_usable(),
        display_active: active_virtual_display().is_some(),
        needs_display: needs_virtual_display(),
        elevated: is_elevated(),
    }
}

/// The virtual monitor, as capture numbers it, when it is on the desktop.
fn active_virtual_display() -> Option<Display> {
    #[cfg(windows)]
    {
        crate::win32::monitors::offered()
            .ok()?
            .into_iter()
            .find(|(_, _, kind)| *kind == crate::win32::monitors::Kind::Virtual)
            .map(|(display, _, _)| display)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Everything this module knows, in a few lines for the log.
///
/// Written at host start and after every failed setup, so the next report from
/// a headless machine carries the state instead of a guess about it.
pub fn diagnostics() -> String {
    #[cfg(windows)]
    {
        use std::fmt::Write as _;
        let mut out = String::new();
        let settings = settings::path();
        let settings_state = match std::fs::read(&settings) {
            Ok(bytes) => match settings::check(&String::from_utf8_lossy(&bytes), 1920, 1080, 60) {
                Ok(()) => "ok".to_string(),
                Err(reason) => reason,
            },
            Err(_) => "absent".to_string(),
        };
        let _ = write!(
            out,
            "elevated={} remote_session={} driver_staged={} publisher_trusted={} settings={} ({settings_state})",
            is_elevated(),
            crate::win32::monitors::is_remote_session(),
            is_idd_installed(),
            match trust::catalog_signer(embedded("mttvdd.cat"), false) {
                Ok(trusted) => trusted.to_string(),
                Err(reason) => reason,
            },
            settings.display(),
        );
        let found = nodes::find();
        if found.is_empty() {
            out.push_str("; no device node");
        }
        for node in found {
            let _ = write!(
                out,
                "; node {} present={} started={} problem={}",
                node.instance_id, node.present, node.started, node.problem
            );
        }
        match crate::win32::monitors::inventory() {
            Ok(entries) => {
                for entry in entries {
                    let _ = write!(
                        out,
                        "; monitor {} {:?} active={} {}x{} primary={} \"{}\"",
                        entry.gdi_name,
                        entry.kind,
                        entry.active,
                        entry.display.resolution.width,
                        entry.display.resolution.height,
                        entry.display.primary,
                        entry.display.name
                    );
                }
            }
            Err(error) => {
                let _ = write!(out, "; monitors: {error}");
            }
        }
        out
    }
    #[cfg(not(windows))]
    {
        "virtual displays are a Windows feature".to_string()
    }
}

// ------------------------------------------------------------------- settings

/// The driver's `vdd_settings.xml`: checking it and making it serve.
///
/// Platform-independent text handling, so it is tested everywhere; only
/// [`settings::path`] and [`settings::ensure`] touch the machine.
mod settings {
    /// Why this settings text would not give the driver a usable
    /// `width`x`height`@`refresh_hz` monitor, or `Ok` when it would.
    ///
    /// Mirrors what the driver's `loadSettings` does with it: every `count`,
    /// `width`, `height`, `refresh_rate` and `g_refresh_rate` goes through
    /// `stoi`/`stof` with no error handling, a `height` uses the `width` read
    /// before it, and a mode is offered at its own rate and at every global
    /// rate.
    pub(super) fn check(xml: &str, width: u32, height: u32, refresh_hz: u32) -> Result<(), String> {
        let xml = xml.trim_start_matches('\u{feff}');
        let doc = roxmltree::Document::parse(xml).map_err(|e| format!("not well-formed: {e}"))?;
        let root = doc.root_element();
        if !root.has_tag_name("vdd_settings") {
            return Err(format!("root element is <{}>", root.tag_name().name()));
        }

        let text = |node: roxmltree::Node| node.text().unwrap_or("").trim().to_string();
        let whole = |node: roxmltree::Node, what: &str| -> Result<u32, String> {
            text(node)
                .parse::<u32>()
                .map_err(|_| format!("<{what}> is not a whole number: {:?}", text(node)))
        };

        let mut monitors = None;
        for count in root.descendants().filter(|n| n.has_tag_name("count")) {
            monitors = Some(whole(count, "count")?);
        }
        match monitors {
            None => return Err("no monitor <count>".into()),
            Some(0) => return Err("monitor <count> is 0".into()),
            Some(_) => {}
        }

        let globals = root
            .descendants()
            .filter(|n| n.has_tag_name("g_refresh_rate"))
            .map(|n| whole(n, "g_refresh_rate"))
            .collect::<Result<Vec<_>, _>>()?;

        let mut entries = 0usize;
        let mut offered = false;
        for mode in root.descendants().filter(|n| n.has_tag_name("resolution")) {
            let child = |name: &str| mode.children().find(|n| n.has_tag_name(name));
            let (Some(w), Some(h), Some(r)) =
                (child("width"), child("height"), child("refresh_rate"))
            else {
                return Err("a <resolution> lacks width, height or refresh_rate".into());
            };
            let (w, h) = (whole(w, "width")?, whole(h, "height")?);
            let rate = text(r)
                .parse::<f32>()
                .map_err(|_| format!("<refresh_rate> is not a number: {:?}", text(r)))?;
            entries += 1;
            if w == width
                && h == height
                && ((rate.round() as u32) == refresh_hz || globals.contains(&refresh_hz))
            {
                offered = true;
            }
        }
        if entries == 0 {
            return Err("no <resolution> entries".into());
        }
        if !offered {
            return Err(format!(
                "{width}x{height} at {refresh_hz} Hz is not offered"
            ));
        }
        Ok(())
    }

    /// The settings text to write, or `None` when `existing` already serves.
    ///
    /// Somebody's own well-formed file is patched rather than replaced when
    /// all it lacks is a monitor or the mode; anything malformed is replaced
    /// whole from the embedded default, because the driver cannot be trusted
    /// to survive it.
    pub(super) fn repaired(
        existing: Option<&str>,
        default: &str,
        width: u32,
        height: u32,
        refresh_hz: u32,
    ) -> Option<String> {
        if let Some(xml) = existing {
            if check(xml, width, height, refresh_hz).is_ok() {
                return None;
            }
            let xml = xml.trim_start_matches('\u{feff}');
            if roxmltree::Document::parse(xml).is_ok() {
                let patched = with_mode(&with_monitor(xml), width, height, refresh_hz);
                if check(&patched, width, height, refresh_hz).is_ok() {
                    return Some(patched);
                }
            }
        }
        let patched = with_mode(&with_monitor(default), width, height, refresh_hz);
        if check(&patched, width, height, refresh_hz).is_ok() {
            Some(patched)
        } else {
            Some(default.to_string())
        }
    }

    /// `xml` with its monitor count raised to 1 if it was below.
    fn with_monitor(xml: &str) -> String {
        const OPEN: &str = "<count>";
        const CLOSE: &str = "</count>";
        let mut out = xml.to_string();
        if let Some(start) = out.find(OPEN) {
            let inner = start + OPEN.len();
            if let Some(len) = out[inner..].find(CLOSE) {
                // Only the text between the tags. Replacing from `<count>`
                // up to `</count>` with a whole element is what once wrote
                // `<count>1</count></count>` into someone's file.
                if out[inner..inner + len].trim().parse::<u32>().unwrap_or(0) < 1 {
                    out.replace_range(inner..inner + len, "1");
                }
            }
        }
        out
    }

    /// `xml` with `width`x`height`@`refresh_hz` added to its mode table if
    /// the table does not already offer it.
    fn with_mode(xml: &str, width: u32, height: u32, refresh_hz: u32) -> String {
        match check(xml, width, height, refresh_hz) {
            Err(reason)
                if reason.ends_with("is not offered") || reason.starts_with("no <resolution>") => {}
            _ => return xml.to_string(),
        }
        // Width, height, rate — in that order, which the driver relies on.
        let mode = format!(
            "\n        <resolution>\n            <width>{width}</width>\n            <height>{height}</height>\n            <refresh_rate>{refresh_hz}</refresh_rate>\n        </resolution>"
        );
        let mut out = xml.to_string();
        if let Some(pos) = out.find("<resolutions>") {
            out.insert_str(pos + "<resolutions>".len(), &mode);
        } else if let Some(pos) = out.find("</vdd_settings>") {
            out.insert_str(pos, &format!("<resolutions>{mode}\n</resolutions>\n"));
        }
        out
    }

    /// Where the driver reads its settings: its `VDDPATH` when set, the
    /// hardcoded default otherwise.
    #[cfg(windows)]
    pub(super) fn path() -> std::path::PathBuf {
        dir().join("vdd_settings.xml")
    }

    #[cfg(windows)]
    fn dir() -> std::path::PathBuf {
        use windows::core::w;
        use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
        let mut buffer = [0u16; 260];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        // SAFETY: `size` is the byte length of `buffer`, as the call expects.
        let read = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!(r"SOFTWARE\MikeTheTech\VirtualDisplayDriver"),
                w!("VDDPATH"),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if read.is_ok() {
            let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
            let configured = String::from_utf16_lossy(&buffer[..end]);
            if !configured.trim().is_empty() {
                return std::path::PathBuf::from(configured.trim());
            }
        }
        std::path::PathBuf::from(super::DEFAULT_SETTINGS_DIR)
    }

    /// Make the file on disk serve `width`x`height`@`refresh_hz`. Returns
    /// whether it was rewritten, which means a running driver must restart
    /// to see it.
    #[cfg(windows)]
    pub(super) fn ensure(width: u32, height: u32, refresh_hz: u32) -> std::io::Result<bool> {
        let path = path();
        let existing = std::fs::read(&path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        let default = String::from_utf8_lossy(super::embedded("vdd_settings.xml")).into_owned();
        let Some(repaired) = repaired(existing.as_deref(), &default, width, height, refresh_hz)
        else {
            return Ok(false);
        };
        match existing
            .as_deref()
            .map(|xml| check(xml, width, height, refresh_hz))
        {
            Some(Err(reason)) => tracing::warn!(
                path = %path.display(),
                %reason,
                "the virtual display driver's settings would not serve; rewriting them"
            ),
            _ => {
                tracing::info!(path = %path.display(), "writing the virtual display driver's settings")
            }
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Whole file or nothing: the driver may read it the moment its device
        // restarts, and half a file is the malformed case all over again.
        let temporary = path.with_extension("xml.pravera");
        std::fs::write(&temporary, repaired)?;
        std::fs::rename(&temporary, &path)?;
        Ok(true)
    }
}

// ---------------------------------------------------------------------- trust

/// Making the driver's publisher trusted, so installation never prompts.
#[cfg(windows)]
mod trust {
    use windows::core::w;
    use windows::Win32::Security::Cryptography::{
        CertAddCertificateContextToStore, CertCloseStore, CertFindCertificateInStore,
        CertFreeCertificateContext, CertOpenStore, CryptMsgClose, CryptMsgGetParam,
        CryptQueryObject, CERT_FIND_EXISTING, CERT_FIND_SUBJECT_CERT, CERT_OPEN_STORE_FLAGS,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED, CERT_QUERY_ENCODING_TYPE,
        CERT_QUERY_FORMAT_FLAG_BINARY, CERT_QUERY_OBJECT_BLOB, CERT_STORE_ADD_USE_EXISTING,
        CERT_STORE_OPEN_EXISTING_FLAG, CERT_STORE_PROV_SYSTEM_W, CERT_STORE_READONLY_FLAG,
        CERT_SYSTEM_STORE_LOCAL_MACHINE, CMSG_SIGNER_CERT_INFO_PARAM, CRYPT_INTEGER_BLOB,
        HCERTSTORE, PKCS_7_ASN_ENCODING, X509_ASN_ENCODING,
    };

    /// Whether the catalogue's signer is a trusted publisher on this machine,
    /// after adding it when `add` is set.
    ///
    /// Only the signing certificate is added — not the intermediate and root
    /// the catalogue also carries, which already chain to a trusted root and
    /// have no business in Trusted Publishers.
    pub(super) fn catalog_signer(catalog: &[u8], add: bool) -> Result<bool, String> {
        let encoding = X509_ASN_ENCODING | PKCS_7_ASN_ENCODING;
        let blob = CRYPT_INTEGER_BLOB {
            cbData: catalog.len() as u32,
            pbData: catalog.as_ptr() as *mut u8,
        };
        let mut store = HCERTSTORE::default();
        let mut message: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `blob` points at `catalog`, which outlives the call; the
        // store and message it returns are closed below on every path.
        unsafe {
            CryptQueryObject(
                CERT_QUERY_OBJECT_BLOB,
                std::ptr::from_ref(&blob).cast(),
                // Signed-message only. A catalogue is also a CTL, and asked
                // for "anything" the call answers CTL — which comes back with
                // no message handle at all.
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                None,
                None,
                None,
                Some(&mut store),
                Some(&mut message),
                None,
            )
            .map_err(|e| format!("reading the driver catalogue's signature: {e}"))?;
        }
        if message.is_null() || store.is_invalid() {
            // SAFETY: closing whichever of the two did come back.
            unsafe {
                if !message.is_null() {
                    let _ = CryptMsgClose(Some(message.cast_const()));
                }
                if !store.is_invalid() {
                    let _ = CertCloseStore(Some(store), 0);
                }
            }
            return Err("the driver catalogue is not a signed message".into());
        }

        // SAFETY: `store` and `message` are the live handles just returned.
        let outcome = unsafe {
            (|| -> Result<bool, String> {
                let mut size = 0u32;
                CryptMsgGetParam(message, CMSG_SIGNER_CERT_INFO_PARAM, 0, None, &mut size)
                    .map_err(|e| format!("reading the catalogue's signer: {e}"))?;
                // `u64`s so the `CERT_INFO` written here is pointer-aligned.
                let mut info = vec![0u64; (size as usize).div_ceil(8)];
                CryptMsgGetParam(
                    message,
                    CMSG_SIGNER_CERT_INFO_PARAM,
                    0,
                    Some(info.as_mut_ptr().cast()),
                    &mut size,
                )
                .map_err(|e| format!("reading the catalogue's signer: {e}"))?;
                let signer = CertFindCertificateInStore(
                    store,
                    encoding,
                    0,
                    CERT_FIND_SUBJECT_CERT,
                    Some(info.as_ptr().cast()),
                    None,
                );
                if signer.is_null() {
                    return Err("the catalogue does not carry its signer's certificate".into());
                }

                let mut flags = CERT_OPEN_STORE_FLAGS(CERT_SYSTEM_STORE_LOCAL_MACHINE);
                if !add {
                    flags = flags | CERT_STORE_READONLY_FLAG | CERT_STORE_OPEN_EXISTING_FLAG;
                }
                let publishers = match CertOpenStore(
                    CERT_STORE_PROV_SYSTEM_W,
                    CERT_QUERY_ENCODING_TYPE(0),
                    None,
                    flags,
                    Some(w!("TrustedPublisher").as_ptr().cast()),
                ) {
                    Ok(publishers) => publishers,
                    Err(e) => {
                        let _ = CertFreeCertificateContext(Some(signer));
                        return Err(format!("opening Trusted Publishers: {e}"));
                    }
                };

                let existing = CertFindCertificateInStore(
                    publishers,
                    encoding,
                    0,
                    CERT_FIND_EXISTING,
                    Some(signer.cast_const().cast()),
                    None,
                );
                let result = if !existing.is_null() {
                    let _ = CertFreeCertificateContext(Some(existing));
                    Ok(true)
                } else if add {
                    CertAddCertificateContextToStore(
                        Some(publishers),
                        signer,
                        CERT_STORE_ADD_USE_EXISTING,
                        None,
                    )
                    .map(|()| {
                        tracing::info!(
                            "trusted the virtual display driver's publisher for silent installs"
                        );
                        true
                    })
                    .map_err(|e| {
                        format!("adding the driver's publisher to Trusted Publishers: {e}")
                    })
                } else {
                    Ok(false)
                };
                let _ = CertFreeCertificateContext(Some(signer));
                let _ = CertCloseStore(Some(publishers), 0);
                result
            })()
        };

        // SAFETY: closing the handles opened above, once each.
        unsafe {
            let _ = CryptMsgClose(Some(message.cast_const()));
            let _ = CertCloseStore(Some(store), 0);
        }
        outcome
    }
}

// -------------------------------------------------------------------- staging

/// Install the driver package from a directory containing its `.inf`.
///
/// Runs `pnputil /add-driver <inf> /install`, which stages the package and
/// installs it on any device already waiting for it. Requires elevation. Never
/// waits more than two minutes: a prompt nobody can answer must not park a
/// thread for the life of the process.
pub fn install_from_dir(dir: &Path) -> std::result::Result<String, String> {
    let inf = std::fs::read_dir(dir)
        .map_err(|e| format!("could not read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("inf"))
        })
        .min_by_key(|p| {
            // Prefer the MttVDD package if a directory happens to carry
            // leftovers of the older IddSampleDriver generation.
            (
                p.file_name()
                    .map(|n| !n.to_string_lossy().to_ascii_lowercase().contains("mttvdd"))
                    .unwrap_or(true),
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        })
        .ok_or_else(|| format!("no .inf found in {}", dir.display()))?;

    #[cfg(windows)]
    {
        let mut command = std::process::Command::new("pnputil");
        command.arg("/add-driver").arg(&inf).arg("/install");
        let output = run_hidden(command, std::time::Duration::from_secs(120))?;
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // 0: done. 259 (`ERROR_NO_MORE_ITEMS`): staged, no device needed it.
        // 3010 (`ERROR_SUCCESS_REBOOT_REQUIRED`): done, reboot to finish.
        match output.status.code() {
            Some(0 | 259 | 3010) => Ok(combined),
            _ => {
                let lowered = combined.to_ascii_lowercase();
                if lowered.contains("access is denied") || lowered.contains("administrator") {
                    return Err(format!("{combined}\n{}", needs_elevation_message()));
                }
                Err(combined)
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = inf;
        Err("driver installation is Windows only".into())
    }
}

/// Run a console tool with no window and a deadline.
#[cfg(windows)]
fn run_hidden(
    mut command: std::process::Command,
    deadline: std::time::Duration,
) -> std::result::Result<std::process::Output, String> {
    use std::io::Read as _;
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;

    let program = format!("{:?}", command.get_program());
    // CREATE_NO_WINDOW. Without it every call flashes a console that also
    // takes the foreground.
    command
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not launch {program}: {e}"))?;

    // Drained on threads so a chatty tool cannot fill a pipe and stall.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );

    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{program} did not finish within {}s (a prompt nobody can answer?)",
                    deadline.as_secs()
                ));
            }
            Err(e) => return Err(format!("waiting for {program}: {e}")),
        }
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Set after a staging attempt failed for a reason other than elevation, so
/// the poll does not rerun `pnputil` against a refusal it cannot fix. Cleared
/// by a relaunch. Elevation refusals do not set it: the next elevated launch
/// is the one that succeeds.
#[cfg(windows)]
static INSTALL_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Stage the embedded driver in the driver store, if it is not there yet.
///
/// Returns `Ok(Some(output))` when it staged now, `Ok(None)` when nothing
/// needed doing, and `Err` with the reason otherwise — immediately, without
/// running anything, when this process is not elevated. Staging alone adds no
/// monitor; [`ensure_virtual_display`] does that, and only when needed.
pub fn try_auto_install() -> std::result::Result<Option<String>, String> {
    if is_idd_installed() {
        return Ok(None);
    }
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;
        if INSTALL_FAILED.load(Ordering::Relaxed) {
            return Err(
                "an earlier install attempt was refused; not retrying until relaunch".into(),
            );
        }
        if !is_elevated() {
            return Err(needs_elevation_message().to_string());
        }
        let _one_at_a_time = SETUP.lock();
        if is_idd_installed() {
            return Ok(None);
        }
        let dir = materialise_package()?;
        if let Err(reason) = trust::catalog_signer(embedded("mttvdd.cat"), true) {
            tracing::warn!(%reason, "could not trust the driver's publisher; the install may prompt");
        }
        match install_from_dir(&dir) {
            Ok(out) => {
                tracing::info!(dir = %dir.display(), "staged the virtual display driver");
                Ok(Some(out))
            }
            Err(out) => {
                tracing::warn!(dir = %dir.display(), output = %out, "staging the virtual display driver failed");
                if !out.contains(needs_elevation_message()) {
                    INSTALL_FAILED.store(true, Ordering::Relaxed);
                }
                Err(out)
            }
        }
    }
    #[cfg(not(windows))]
    {
        Err("driver installation is Windows only".into())
    }
}

// ---------------------------------------------------------------- device node

/// The driver's device nodes, through SetupAPI.
#[cfg(windows)]
mod nodes {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{w, BOOL, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_Status, SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList,
        SetupDiCreateDeviceInfoW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
        SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW,
        SetupDiOpenDeviceInfoW, SetupDiSetClassInstallParamsW, SetupDiSetDeviceRegistryPropertyW,
        UpdateDriverForPlugAndPlayDevicesW, CM_DEVNODE_STATUS_FLAGS, CM_PROB, CR_SUCCESS,
        DICD_GENERATE_ID, DICS_ENABLE, DICS_FLAG_CONFIGSPECIFIC, DICS_FLAG_GLOBAL, DICS_PROPCHANGE,
        DIF_PROPERTYCHANGE, DIF_REGISTERDEVICE, DIGCF_ALLCLASSES, DN_HAS_PROBLEM, DN_STARTED,
        HDEVINFO, INSTALLFLAG_FORCE, INSTALLFLAG_NONINTERACTIVE, SETUP_DI_STATE_CHANGE,
        SPDRP_HARDWAREID, SP_CLASSINSTALL_HEADER, SP_DEVINFO_DATA, SP_PROPCHANGE_PARAMS,
    };

    /// The display-class GUID, `{4d36e968-e325-11ce-bfc1-08002be10318}`.
    const DISPLAY_CLASS_GUID: windows::core::GUID =
        windows::core::GUID::from_u128(0x4d36e968_e325_11ce_bfc1_08002be10318);

    /// `CM_PROB_DISABLED`: somebody turned the device off.
    pub(super) const PROBLEM_DISABLED: u32 = 22;

    /// One device node carrying the driver's hardware id.
    #[derive(Debug, Clone)]
    pub(super) struct Node {
        pub instance_id: String,
        /// Known to the PnP manager right now, rather than a leftover
        /// registry entry ("phantom").
        pub present: bool,
        pub started: bool,
        /// `CM_PROB_*`, 0 when the device is fine.
        pub problem: u32,
    }

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// Every root-enumerated node whose hardware id is the driver's, present
    /// or not. Matched on the hardware id rather than the instance id, so a
    /// node the upstream installer made (`ROOT\DISPLAY\0000`) counts too.
    pub(super) fn find() -> Vec<Node> {
        let mut nodes = Vec::new();
        // SAFETY: the list is destroyed below; every buffer passed is sized
        // by the length handed with it.
        unsafe {
            let Ok(list) = SetupDiGetClassDevsW(None, w!("ROOT"), None, DIGCF_ALLCLASSES) else {
                return nodes;
            };
            for index in 0u32.. {
                let mut devinfo = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                if SetupDiEnumDeviceInfo(list, index, &mut devinfo).is_err() {
                    break;
                }
                if !carries_driver_id(list, &devinfo) {
                    continue;
                }
                let mut id = [0u16; 512];
                let instance_id =
                    if SetupDiGetDeviceInstanceIdW(list, &devinfo, Some(&mut id), None).is_ok() {
                        let end = id.iter().position(|&c| c == 0).unwrap_or(id.len());
                        String::from_utf16_lossy(&id[..end])
                    } else {
                        String::new()
                    };
                let mut status = CM_DEVNODE_STATUS_FLAGS(0);
                let mut problem = CM_PROB(0);
                let present = CM_Get_DevNode_Status(&mut status, &mut problem, devinfo.DevInst, 0)
                    == CR_SUCCESS;
                nodes.push(Node {
                    instance_id,
                    present,
                    started: present && status.0 & DN_STARTED.0 != 0,
                    problem: if present && status.0 & DN_HAS_PROBLEM.0 != 0 {
                        problem.0
                    } else {
                        0
                    },
                });
            }
            let _ = SetupDiDestroyDeviceInfoList(list);
        }
        nodes
    }

    /// Whether this node's hardware ids include the driver's.
    ///
    /// # Safety
    /// `list` and `devinfo` must be a live device information set and one of
    /// its elements.
    unsafe fn carries_driver_id(list: HDEVINFO, devinfo: &SP_DEVINFO_DATA) -> bool {
        let mut buffer = [0u8; 1024];
        // SAFETY: per the function contract; the buffer length is passed.
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                list,
                devinfo,
                SPDRP_HARDWAREID,
                None,
                Some(&mut buffer),
                None,
            )
        }
        .is_err()
        {
            return false;
        }
        let wide: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        wide.split(|&c| c == 0)
            .map(String::from_utf16_lossy)
            .any(|id| {
                id.eq_ignore_ascii_case(super::DRIVER_HW_ID) || id.eq_ignore_ascii_case("MttVDD")
            })
    }

    /// Create a root-enumerated node for the driver — `devcon install`'s
    /// first half: create the element, give it the hardware id, register it.
    /// The driver is bound separately by [`bind`].
    pub(super) fn create() -> Result<(), String> {
        // SAFETY: every pointer handed over is to a live local; the list is
        // destroyed on every path.
        unsafe {
            let list = SetupDiCreateDeviceInfoList(Some(&DISPLAY_CLASS_GUID), None)
                .map_err(|e| super::setupapi_error("create device list", &e))?;
            let outcome = (|| -> Result<(), (&'static str, windows::core::Error)> {
                let mut devinfo = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                let device_id = wide(super::DRIVER_DEVICE_ID);
                SetupDiCreateDeviceInfoW(
                    list,
                    PCWSTR(device_id.as_ptr()),
                    &DISPLAY_CLASS_GUID,
                    PCWSTR::null(),
                    None,
                    DICD_GENERATE_ID,
                    Some(&mut devinfo),
                )
                .map_err(|e| ("create device", e))?;
                // REG_MULTI_SZ: the id, its terminator, and the list's.
                let mut ids = wide(super::DRIVER_HW_ID);
                ids.push(0);
                let bytes: Vec<u8> = ids.iter().flat_map(|c| c.to_le_bytes()).collect();
                SetupDiSetDeviceRegistryPropertyW(
                    list,
                    &mut devinfo,
                    SPDRP_HARDWAREID,
                    Some(&bytes),
                )
                .map_err(|e| ("set hardware id", e))?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, list, Some(&devinfo))
                    .map_err(|e| ("register device", e))?;
                Ok(())
            })();
            let _ = SetupDiDestroyDeviceInfoList(list);
            outcome.map_err(|(step, e)| super::setupapi_error(step, &e))
        }
    }

    /// Install the package on every node with the driver's hardware id and
    /// start them — `devcon install`'s second half. Returns whether Windows
    /// wants a reboot to finish.
    ///
    /// Non-interactive: if Windows would need to ask anything (an untrusted
    /// publisher, chiefly) this fails instead of showing a dialog on a desktop
    /// nobody is looking at.
    pub(super) fn bind(package: &std::path::Path) -> Result<bool, String> {
        let hardware_id = wide(super::DRIVER_HW_ID);
        let inf = wide(&package.join("MttVDD.inf").to_string_lossy());
        let mut reboot = BOOL(0);
        // SAFETY: both strings outlive the call; `reboot` is a live local.
        unsafe {
            UpdateDriverForPlugAndPlayDevicesW(
                None,
                PCWSTR(hardware_id.as_ptr()),
                PCWSTR(inf.as_ptr()),
                INSTALLFLAG_FORCE | INSTALLFLAG_NONINTERACTIVE,
                Some(&mut reboot),
            )
        }
        .map_err(|e| super::setupapi_error("install the driver on its device", &e))?;
        Ok(reboot.as_bool())
    }

    /// Restart a node, so its driver reads its settings again.
    pub(super) fn restart(instance_id: &str) -> Result<(), String> {
        change(instance_id, DICS_PROPCHANGE)
    }

    /// Re-enable a node somebody disabled.
    pub(super) fn enable(instance_id: &str) -> Result<(), String> {
        change(instance_id, DICS_ENABLE)
    }

    fn change(instance_id: &str, state: SETUP_DI_STATE_CHANGE) -> Result<(), String> {
        let id = wide(instance_id);
        // SAFETY: every pointer handed over is to a live local; the list is
        // destroyed on every path.
        unsafe {
            let list = SetupDiCreateDeviceInfoList(None, None)
                .map_err(|e| super::setupapi_error("create device list", &e))?;
            let outcome = (|| -> Result<(), (&'static str, windows::core::Error)> {
                let mut devinfo = SP_DEVINFO_DATA {
                    cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
                    ..Default::default()
                };
                SetupDiOpenDeviceInfoW(list, PCWSTR(id.as_ptr()), None, 0, Some(&mut devinfo))
                    .map_err(|e| ("open device", e))?;
                let params = SP_PROPCHANGE_PARAMS {
                    ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                        cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                        InstallFunction: DIF_PROPERTYCHANGE,
                    },
                    StateChange: state,
                    // `devcon`'s scopes: enabling is global, a restart is
                    // for the current hardware profile.
                    Scope: if state == DICS_ENABLE {
                        DICS_FLAG_GLOBAL
                    } else {
                        DICS_FLAG_CONFIGSPECIFIC
                    },
                    HwProfile: 0,
                };
                SetupDiSetClassInstallParamsW(
                    list,
                    Some(&devinfo),
                    Some(&params.ClassInstallHeader),
                    std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32,
                )
                .map_err(|e| ("set change parameters", e))?;
                SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, list, Some(&devinfo))
                    .map_err(|e| ("change device state", e))?;
                Ok(())
            })();
            let _ = SetupDiDestroyDeviceInfoList(list);
            outcome.map_err(|(step, e)| super::setupapi_error(step, &e))
        }
    }
}

/// English text for the SetupAPI errors this flow can produce.
///
/// The system message table has no text for them (they live in setupapi's own
/// table), so without this a failure renders as a bare hex. Accepts both the
/// raw `0xE00002xx` form and the `0x800F02xx` `HRESULT` form. Text from
/// `setupapi.h`.
#[cfg(windows)]
fn spapi_text(code: u32) -> Option<&'static str> {
    let code = if code & 0xE000_0000 == 0xE000_0000 {
        0x800F_0000 | (code & 0xFFFF)
    } else {
        code
    };
    Some(match code {
        0x800F0200 => "no associated install class",
        0x800F0201 => "class mismatch",
        0x800F0202 => "duplicate found: an existing device duplicates this one",
        0x800F0203 => "no driver selected",
        0x800F0204 => "device registry key does not exist",
        0x800F0205 => "invalid device instance name",
        0x800F0206 => "install class not present or invalid",
        0x800F0207 => "device instance already exists",
        0x800F0208 => "device info element not registered",
        0x800F0209 => "invalid device property code",
        0x800F020A => "INF does not exist",
        0x800F020B => "no device carries the driver's hardware id",
        0x800F0228 => "the INF has no driver for this device",
        0x800F023C => "installing would have needed a prompt, and none can be shown here",
        0x800F0242 => "the driver's signature is not trusted on this machine",
        0x800F0243 => "the driver's publisher is not trusted on this machine",
        0x800F0247 => "the driver could not be added to the driver store",
        _ => return None,
    })
}

/// Render a SetupAPI failure with its English text, from the `HRESULT` the
/// `windows` crate captured at the failing call — not `GetLastError`
/// afterwards, which by then holds whatever the last formatting helper left.
#[cfg(windows)]
fn setupapi_error(context: &str, e: &windows::core::Error) -> String {
    let code = e.code().0 as u32;
    match spapi_text(code) {
        Some(text) => format!("{context}: {text} (0x{code:08X})"),
        None => format!("{context}: {e} (0x{code:08X})"),
    }
}

// ---------------------------------------------------------------- orchestration

/// Make sure the virtual display is on the desktop, doing whatever is missing:
/// staging, trusting, settings, the device node, starting it, and making it
/// primary when nothing else is a real screen.
///
/// Idempotent and cheap when the display is already there. Callers gate on
/// [`needs_virtual_display`] — this function does not, because the explicit
/// button uses the same machinery. After a failure, repeat calls within a
/// minute return that failure without retrying; see [`add_virtual_display`]
/// for the call that always tries.
///
/// `width`/`height`/`refresh_hz` is the mode the driver's settings must
/// offer; Windows picks the monitor's preferred mode from what is offered.
pub fn ensure_virtual_display(width: u32, height: u32, refresh_hz: u32) -> Result<Display> {
    run(width, height, refresh_hz, false)
}

/// [`ensure_virtual_display`] for an explicit request: never answers from the
/// recent-failure cache. Behind the Settings button, which is the one place a
/// virtual display may be added beside real monitors.
pub fn add_virtual_display(width: u32, height: u32, refresh_hz: u32) -> Result<Display> {
    run(width, height, refresh_hz, true)
}

fn run(width: u32, height: u32, refresh_hz: u32, explicit: bool) -> Result<Display> {
    #[cfg(windows)]
    {
        let _one_at_a_time = SETUP.lock();

        if let Some(display) = active_virtual_display() {
            promote_if_headless();
            return Ok(active_virtual_display().unwrap_or(display));
        }
        if crate::win32::monitors::is_remote_session() {
            return Err(CaptureError::Unavailable(
                "a Remote Desktop session cannot show the virtual display; it appears on this machine's own console",
            ));
        }
        if !is_elevated() {
            return Err(CaptureError::Unavailable(needs_elevation_message()));
        }
        if !explicit {
            if let Some((at, reason)) = LAST_FAILURE.lock().as_ref() {
                if at.elapsed() < RETRY_AFTER {
                    return Err(CaptureError::backend(format!(
                        "{reason} (retrying automatically in about a minute)"
                    )));
                }
            }
        }

        match setup(width, height, refresh_hz) {
            Ok(shown) => {
                *LAST_FAILURE.lock() = None;
                // Not `display`: inside `tracing`'s macros that name is its
                // field-formatting helper.
                tracing::info!(
                    name = %shown.name,
                    width = shown.resolution.width,
                    height = shown.resolution.height,
                    "the virtual display is on the desktop"
                );
                Ok(shown)
            }
            Err(reason) => {
                tracing::warn!(%reason, state = %diagnostics(), "the virtual display could not be brought up");
                *LAST_FAILURE.lock() = Some((std::time::Instant::now(), reason.clone()));
                Err(CaptureError::backend(reason))
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (width, height, refresh_hz, explicit);
        Err(CaptureError::Unavailable(
            "virtual displays are a Windows display driver feature",
        ))
    }
}

/// The whole sequence, elevated, under the setup lock.
#[cfg(windows)]
fn setup(width: u32, height: u32, refresh_hz: u32) -> std::result::Result<Display, String> {
    // No SetupAPI dialogs from this process, ever: on a machine nobody is
    // watching, a dialog is a thread that never returns.
    // SAFETY: a process-wide flag with no pointers involved.
    unsafe {
        let _ =
            windows::Win32::Devices::DeviceAndDriverInstallation::SetupSetNonInteractiveMode(true);
    }

    let package = materialise_package()?;
    if let Err(reason) = trust::catalog_signer(embedded("mttvdd.cat"), true) {
        // Not fatal: a machine that already trusts it, or whose policy
        // pre-approved it, installs anyway. If not, the bind below fails
        // with "publisher is not trusted" rather than hanging.
        tracing::warn!(%reason, "could not trust the virtual display driver's publisher");
    }
    let rewrote = settings::ensure(width, height, refresh_hz).map_err(|e| {
        format!(
            "writing the driver's settings ({}): {e}",
            settings::path().display()
        )
    })?;

    let found = nodes::find();
    let present: Vec<&nodes::Node> = found.iter().filter(|node| node.present).collect();
    if present.len() > 1 {
        tracing::warn!(
            count = present.len(),
            "more than one virtual display device exists; each adds its own monitor"
        );
    }
    match present.first() {
        None => {
            tracing::info!("creating the virtual display device");
            nodes::create()?;
            if nodes::bind(&package)? {
                tracing::warn!("Windows wants a reboot to finish installing the virtual display");
            }
        }
        Some(node) if node.problem == nodes::PROBLEM_DISABLED => {
            tracing::info!(id = %node.instance_id, "the virtual display device was disabled; enabling it");
            nodes::enable(&node.instance_id)?;
        }
        Some(node) if !node.started || node.problem != 0 => {
            tracing::info!(
                id = %node.instance_id,
                problem = node.problem,
                "the virtual display device is not running; reinstalling its driver"
            );
            if nodes::bind(&package)? {
                tracing::warn!("Windows wants a reboot to finish installing the virtual display");
            }
        }
        Some(node) => {
            // Running with no monitor on the desktop: it started with settings
            // that gave it none (or were just rewritten). A restart makes it
            // read them again.
            tracing::info!(
                id = %node.instance_id,
                settings_rewritten = rewrote,
                "restarting the virtual display device"
            );
            nodes::restart(&node.instance_id)?;
        }
    }

    let display = match wait_for_virtual_display(ARRIVAL) {
        Some(display) => display,
        None => {
            // The monitor is connected but not part of the desktop — a
            // topology Windows remembered from before. Extending is what a
            // person would pick in Display settings.
            extend_desktop();
            wait_for_virtual_display(std::time::Duration::from_secs(10)).ok_or_else(|| {
                "the virtual display device is installed but its monitor did not reach the desktop"
                    .to_string()
            })?
        }
    };
    promote_if_headless();
    Ok(active_virtual_display().unwrap_or(display))
}

#[cfg(windows)]
fn wait_for_virtual_display(within: std::time::Duration) -> Option<Display> {
    let started = std::time::Instant::now();
    loop {
        if let Some(display) = active_virtual_display() {
            return Some(display);
        }
        if started.elapsed() >= within {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// Apply the "extend these displays" topology, which activates every
/// connected monitor.
#[cfg(windows)]
fn extend_desktop() {
    use windows::Win32::Devices::Display::{SetDisplayConfig, SDC_APPLY, SDC_TOPOLOGY_EXTEND};
    // SAFETY: no arrays are passed; the flags select a stored topology.
    let result = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };
    if result != 0 {
        tracing::debug!(result, "extending the desktop did not take");
    }
}

/// Make the virtual monitor primary when every other display is a
/// placeholder. Never on a machine with a real monitor: that is somebody's
/// desktop, arranged the way they want it.
#[cfg(windows)]
fn promote_if_headless() {
    use crate::win32::monitors::{self, Kind};
    let Ok(entries) = monitors::inventory() else {
        return;
    };
    if entries.iter().any(|e| e.kind == Kind::Physical) {
        return;
    }
    let Some(virt) = entries.iter().find(|e| e.kind == Kind::Virtual) else {
        return;
    };
    if virt.display.primary {
        return;
    }
    match make_primary(&virt.gdi_name) {
        Ok(()) => tracing::info!(display = %virt.gdi_name, "made the virtual display primary"),
        Err(reason) => tracing::warn!(%reason, "could not make the virtual display primary"),
    }
}

/// Move `gdi_name` to (0, 0) and make it primary, shifting every other
/// attached display by the same offset so the arrangement is kept.
#[cfg(windows)]
fn make_primary(gdi_name: &str) -> std::result::Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::POINTL;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsW, CDS_NORESET,
        CDS_SET_PRIMARY, CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAY_DEVICEW,
        DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISP_CHANGE_SUCCESSFUL, DM_POSITION,
        ENUM_CURRENT_SETTINGS,
    };

    let mut attached: Vec<([u16; 32], DEVMODEW)> = Vec::new();
    // SAFETY: every struct is sized as the call requires and outlives it.
    unsafe {
        for index in 0u32.. {
            let mut device = DISPLAY_DEVICEW {
                cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            if !EnumDisplayDevicesW(PCWSTR::null(), index, &mut device, 0).as_bool() {
                break;
            }
            if device.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 {
                continue;
            }
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if EnumDisplaySettingsW(
                PCWSTR(device.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            )
            .as_bool()
            {
                attached.push((device.DeviceName, mode));
            }
        }

        let name_of = |raw: &[u16; 32]| {
            let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
            String::from_utf16_lossy(&raw[..end])
        };
        let origin = attached
            .iter()
            .find(|(name, _)| name_of(name).eq_ignore_ascii_case(gdi_name))
            .map(|(_, mode)| mode.Anonymous1.Anonymous2.dmPosition)
            .ok_or_else(|| format!("{gdi_name} is not attached to the desktop"))?;

        for (name, mut mode) in attached.iter().copied() {
            let target = name_of(&name).eq_ignore_ascii_case(gdi_name);
            let at = mode.Anonymous1.Anonymous2.dmPosition;
            mode.Anonymous1.Anonymous2.dmPosition = POINTL {
                x: at.x - origin.x,
                y: at.y - origin.y,
            };
            mode.dmFields = DM_POSITION;
            let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
            if target {
                flags = flags | CDS_SET_PRIMARY;
            }
            let changed =
                ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&mode), None, flags, None);
            if changed != DISP_CHANGE_SUCCESSFUL {
                return Err(format!(
                    "{} refused its new position ({})",
                    name_of(&name),
                    changed.0
                ));
            }
        }
        // Everything above was staged with CDS_NORESET; this applies it.
        let applied = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        if applied != DISP_CHANGE_SUCCESSFUL {
            return Err(format!(
                "applying the new arrangement failed ({})",
                applied.0
            ));
        }
    }
    Ok(())
}

/// Ensure there is at least one display to capture.
///
/// Adds the virtual display when [`needs_virtual_display`] says the machine
/// has no real screen, and returns whether hosting now has something to
/// offer. `false` means hosting must refuse — never a test pattern.
pub fn ensure_at_least_one_display() -> bool {
    if needs_virtual_display() {
        if let Err(err) = ensure_virtual_display(1920, 1080, 60) {
            tracing::debug!(%err, "no virtual display; hosting must refuse");
        }
    }
    crate::platform_source()
        .and_then(|source| source.displays())
        .is_ok_and(|list| !list.is_empty())
}

// ------------------------------------------------------------ fallback source

/// A `CaptureSource` that adds the secure-desktop fallback.
///
/// Wraps any backend and passes its displays through untouched: zero displays
/// stay zero so hosting refuses honestly instead of inventing a test pattern.
///
/// When the inner source refuses with `PermissionDenied` (WGC on the secure
/// desktop), this wrapper tries `DdaSource` before giving up: DDA is the
/// monitor-level duplication that works from session 0 as `SYSTEM` and can
/// see the lock screen where WGC cannot.
pub struct IddFallbackSource {
    inner: Box<dyn CaptureSource>,
    #[cfg(windows)]
    dda: Option<crate::dda::DdaSource>,
}

impl IddFallbackSource {
    /// Wrap `inner`, adding the DDA secure-desktop fallback.
    pub fn new(inner: Box<dyn CaptureSource>, _fallback_resolution: Resolution) -> Self {
        IddFallbackSource {
            inner,
            #[cfg(windows)]
            dda: crate::dda::DdaSource::new().ok(),
        }
    }

    /// Convenience: `inner` wrapped with the secure-desktop fallback.
    pub fn with_default_fallback(inner: Box<dyn CaptureSource>) -> Self {
        Self::new(inner, Resolution::new(1920, 1080))
    }
}

impl CaptureSource for IddFallbackSource {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn displays(&self) -> Result<Vec<Display>> {
        self.inner.displays()
    }

    fn start(&self, id: DisplayId, options: &CaptureOptions) -> Result<FrameStream> {
        match self.inner.start(id, options) {
            Ok(stream) => Ok(stream),
            Err(err) => {
                // PermissionDenied is the WGC signal for the secure desktop
                // (Winlogon / UAC). DDA from the service can still see it.
                #[cfg(windows)]
                if matches!(err, CaptureError::PermissionDenied) {
                    if let Some(dda) = &self.dda {
                        match dda.start(id, options) {
                            Ok(stream) => {
                                tracing::info!(%id, "capturing via Desktop Duplication fallback for secure desktop");
                                return Ok(stream);
                            }
                            Err(dda_err) => {
                                tracing::debug!(%dda_err, "DDA fallback also failed");
                            }
                        }
                    }
                }
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureSource;

    fn default_settings() -> String {
        String::from_utf8_lossy(embedded("vdd_settings.xml")).into_owned()
    }

    #[test]
    fn the_bundled_settings_already_offer_1080p60() {
        // 1920x1080 is listed at 30 Hz, and 60 is a global rate: the driver
        // offers 1920x1080@60 without anything being added.
        assert_eq!(settings::check(&default_settings(), 1920, 1080, 60), Ok(()));
        assert_eq!(
            settings::repaired(
                Some(&default_settings()),
                &default_settings(),
                1920,
                1080,
                60
            ),
            None,
            "a file that serves must be left alone"
        );
    }

    #[test]
    fn a_header_overwritten_in_place_is_replaced_whole() {
        // What was found on a real machine: the first 17 bytes of the file
        // overwritten with `<count>1</count>\n`, leaving the tail of the XML
        // declaration as text outside any element. The driver parses up to
        // the error and ends up with a monitor and no modes.
        let default = default_settings();
        let mut corrupt = default.clone();
        corrupt.replace_range(0..17, "<count>1</count>\n");
        assert!(settings::check(&corrupt, 1920, 1080, 60).is_err());
        let repaired = settings::repaired(Some(&corrupt), &default, 1920, 1080, 60)
            .expect("a corrupt file must be rewritten");
        assert_eq!(settings::check(&repaired, 1920, 1080, 60), Ok(()));
        assert!(repaired.starts_with("<?xml"));
    }

    #[test]
    fn a_zero_count_is_raised_without_breaking_the_element() {
        let default = default_settings();
        let zero = default.replacen("<count>1</count>", "<count>0</count>", 1);
        assert!(settings::check(&zero, 1920, 1080, 60).is_err());
        let repaired = settings::repaired(Some(&zero), &default, 1920, 1080, 60).unwrap();
        assert_eq!(settings::check(&repaired, 1920, 1080, 60), Ok(()));
        // The old patch wrote `<count>1</count></count>`.
        assert!(!repaired.contains("</count></count>"));
        assert!(repaired.contains("<count>1</count>"));
    }

    #[test]
    fn a_missing_mode_is_added_and_the_rest_kept() {
        let default = default_settings();
        // Somebody's own table, without 2560x1600.
        assert!(settings::check(&default, 2560, 1600, 60).is_err());
        let repaired = settings::repaired(Some(&default), &default, 2560, 1600, 60).unwrap();
        assert_eq!(settings::check(&repaired, 2560, 1600, 60), Ok(()));
        // Their existing modes and options survive.
        assert_eq!(settings::check(&repaired, 1920, 1080, 60), Ok(()));
        assert!(repaired.contains("<HardwareCursor>true</HardwareCursor>"));
    }

    #[test]
    fn values_the_driver_would_crash_on_are_refused() {
        // `stoi("wide")` throws inside the driver host.
        let default = default_settings();
        let garbage = default.replacen("<width>800</width>", "<width>wide</width>", 1);
        assert!(settings::check(&garbage, 1920, 1080, 60).is_err());
        let repaired = settings::repaired(Some(&garbage), &default, 1920, 1080, 60).unwrap();
        assert!(!repaired.contains("<width>wide</width>"));
        assert_eq!(settings::check(&repaired, 1920, 1080, 60), Ok(()));
    }

    #[test]
    fn a_missing_file_gets_the_bundled_default() {
        let default = default_settings();
        assert_eq!(
            settings::repaired(None, &default, 1920, 1080, 60).as_deref(),
            Some(default.as_str())
        );
    }

    #[test]
    fn a_byte_order_mark_is_not_a_malformed_file() {
        let with_bom = format!("\u{feff}{}", default_settings());
        assert_eq!(settings::check(&with_bom, 1920, 1080, 60), Ok(()));
    }

    #[cfg(windows)]
    #[test]
    fn setupapi_codes_have_english_text_in_both_forms() {
        // The system message table has no text for these, so the flow carries
        // its own. If this fails, a headless failure renders as a bare hex.
        assert_eq!(spapi_text(0x800F0205), Some("invalid device instance name"));
        assert_eq!(spapi_text(0xE0000205), Some("invalid device instance name"));
        assert_eq!(spapi_text(0xE0000243), spapi_text(0x800F0243));
        assert_eq!(spapi_text(0x80070005), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_catalogue_names_a_signer_it_carries() {
        // Read-only: whether it is trusted here is this machine's business;
        // the point is that the signer can be found at all, because trusting
        // it is what makes the first install silent.
        let result = trust::catalog_signer(embedded("mttvdd.cat"), false);
        assert!(result.is_ok(), "{result:?}");
    }

    #[cfg(windows)]
    #[test]
    fn looking_for_device_nodes_changes_nothing() {
        // Read-only enumeration; must not panic on any machine.
        for node in nodes::find() {
            eprintln!("{node:?}");
        }
        eprintln!("{}", diagnostics());
    }

    struct EmptySource;
    impl CaptureSource for EmptySource {
        fn name(&self) -> &'static str {
            "empty"
        }
        fn displays(&self) -> Result<Vec<Display>> {
            Ok(Vec::new())
        }
        fn start(&self, _id: DisplayId, _opts: &CaptureOptions) -> Result<FrameStream> {
            Err(CaptureError::NoDisplays)
        }
    }

    struct FailingSource;
    impl CaptureSource for FailingSource {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn displays(&self) -> Result<Vec<Display>> {
            Err(CaptureError::Unavailable("no backend"))
        }
        fn start(&self, _id: DisplayId, _opts: &CaptureOptions) -> Result<FrameStream> {
            Err(CaptureError::Unavailable("no backend"))
        }
    }

    struct OneDisplay(Resolution);
    impl CaptureSource for OneDisplay {
        fn name(&self) -> &'static str {
            "stub"
        }
        fn displays(&self) -> Result<Vec<Display>> {
            Ok(vec![Display {
                id: DisplayId::PRIMARY,
                name: "Stub".to_string(),
                resolution: self.0,
                position: (0, 0),
                scale: 1.0,
                primary: true,
                refresh_hz: 60,
            }])
        }
        fn start(&self, id: DisplayId, _opts: &CaptureOptions) -> Result<FrameStream> {
            if id == DisplayId::PRIMARY {
                Err(CaptureError::Unavailable("stub captures nothing"))
            } else {
                Err(CaptureError::NoSuchDisplay(id))
            }
        }
    }

    #[test]
    fn an_empty_inner_source_stays_empty_so_hosting_refuses_honestly() {
        let inner: Box<dyn CaptureSource> = Box::new(EmptySource);
        let wrapped = IddFallbackSource::new(inner, Resolution::new(1920, 1080));
        let displays = wrapped.displays().expect("empty list must pass through");
        assert!(displays.is_empty());
    }

    #[test]
    fn an_unavailable_backend_refuses_instead_of_inventing_a_display() {
        let inner: Box<dyn CaptureSource> = Box::new(FailingSource);
        let wrapped = IddFallbackSource::with_default_fallback(inner);
        assert!(matches!(
            wrapped.displays(),
            Err(CaptureError::Unavailable(_))
        ));
        assert!(matches!(
            wrapped.start(DisplayId::PRIMARY, &CaptureOptions::default()),
            Err(CaptureError::Unavailable(_))
        ));
    }

    #[test]
    fn a_real_display_is_never_replaced_by_the_fallback() {
        let inner: Box<dyn CaptureSource> = Box::new(OneDisplay(Resolution::new(800, 600)));
        let wrapped = IddFallbackSource::new(inner, Resolution::new(1920, 1080));
        let displays = wrapped.displays().unwrap();
        assert_eq!(displays[0].resolution, Resolution::new(800, 600));
    }

    #[test]
    fn install_from_dir_reports_missing_inf_without_panicking() {
        let dir = std::env::temp_dir().join("pravera-idd-test-empty");
        let _ = std::fs::create_dir_all(&dir);
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
        let err = install_from_dir(&dir).unwrap_err();
        assert!(err.contains("no .inf"), "{err}");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn the_embedded_package_is_complete() {
        assert_eq!(EMBEDDED_PACKAGE.len(), 4);
        for (name, bytes) in EMBEDDED_PACKAGE {
            assert!(!bytes.is_empty(), "{name} is empty");
        }
        let inf = embedded("MttVDD.inf");
        // Windows driver infs are UTF-16LE with a BOM.
        let text = String::from_utf16_lossy(
            &inf[2..]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        // The hardware id this module creates nodes with must be one the INF
        // matches, or the bind finds nothing to install on.
        assert!(
            text.contains(r"Root\MttVDD"),
            "inf does not match Root\\MttVDD"
        );
    }

    #[test]
    fn extracting_the_embedded_package_reproduces_it() {
        let dir = std::env::temp_dir().join(format!("pravera-idd-extract-{}", std::process::id()));
        extract_embedded_package(&dir).expect("extraction must succeed");
        for (name, bytes) in EMBEDDED_PACKAGE {
            let written = std::fs::read(dir.join(name)).expect("file must exist");
            assert_eq!(written, *bytes, "{name} differs");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

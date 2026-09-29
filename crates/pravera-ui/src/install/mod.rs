//! Installing, updating and removing Pravera.
//!
//! # The installer is the program
//!
//! There is no separate setup binary and no payload glued onto one. Each
//! release ships one file per platform, and that file *is* `pravera`: named
//! `Pravera-Setup-…`, it opens as the installer; copied into place under its
//! own name, it is the app. That keeps the promise the README makes — one
//! executable — and it means an update is nothing more than the same file
//! arriving again:
//!
//! | platform | what ships | where it goes |
//! |---|---|---|
//! | Windows | `Pravera-Setup-Windows-x64.exe` | `%LOCALAPPDATA%\Programs\Pravera\pravera.exe` |
//! | macOS | `Pravera-Setup-macOS-arm64.zip` (`Pravera Setup.app`) | `/Applications/Pravera.app` |
//! | Linux | `Pravera-Setup-Linux-x86_64.AppImage` | `~/.local/share/pravera/pravera` |
//!
//! Per user, never elevated: nothing here needs an administrator, so nothing
//! here asks for one. The Windows service is the one piece that does, and it
//! still registers itself the way it always has, the first time Pravera runs
//! elevated.
//!
//! # Replacing a running program
//!
//! Windows will not overwrite an executable that is running, but it will
//! rename one. So a new build is staged beside the old, the old is renamed out
//! of the way, and the new one takes its name — see [`replace_file`]. Whatever
//! was running keeps running the old image until it exits, and the leftovers
//! are swept up by the next start ([`sweep_retired`]).

pub mod release;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod win;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where releases come from.
pub const REPO: &str = "n1ssyyy/Pravera";
/// Shown by the operating system's uninstall list and the Setup window.
pub const PUBLISHER: &str = "n1ssyyy";
pub const HOMEPAGE: &str = "https://github.com/n1ssyyy/Pravera";

/// This build's version, from the workspace manifest. A release tag must match
/// it; CI refuses to publish one that does not.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The one file each platform's release carries, which is also the file the
/// updater downloads. Changing a name here without changing CI breaks every
/// installed copy's updates, which is why a test pins them.
pub const ASSET: &str = if cfg!(windows) {
    "Pravera-Setup-Windows-x64.exe"
} else if cfg!(target_os = "macos") {
    "Pravera-Setup-macOS-arm64.zip"
} else {
    "Pravera-Setup-Linux-x86_64.AppImage"
};

/// This build's version, parsed.
pub fn current_version() -> semver::Version {
    semver::Version::parse(VERSION).expect("the crate version is semver")
}

// ------------------------------------------------------------------ the CLI

/// What the command line asked for. Only the installer's own flags; the
/// service and agent flags are read where they always were.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cli {
    /// `--version`: print `pravera X.Y.Z` and exit. Also how CI and the
    /// updater check that a downloaded build actually runs.
    pub version: bool,
    /// `--setup`: open the installer whatever this file is called.
    pub setup: bool,
    /// `--install`: install, from the installer or silently with `--quiet`.
    pub install: bool,
    /// `--uninstall`: what the operating system's uninstall entry runs.
    pub uninstall: bool,
    /// `--quiet`: no window. For scripts, CI and the uninstall list.
    pub quiet: bool,
    /// `--no-launch`: do not open Pravera once installed.
    pub no_launch: bool,
    /// `--desktop-shortcut`: add one (quiet installs only; the window asks).
    pub desktop: bool,
    /// `--purge`: with `--uninstall`, also delete this user's Pravera data.
    pub purge: bool,
    /// `--dir PATH`: install somewhere other than the default.
    pub dir: Option<PathBuf>,
    /// `--export-icon PATH SIZE`: write the app icon as a PNG. The release
    /// build uses it to make the macOS and Linux icons from the same vector
    /// the Windows one comes from.
    pub export_icon: Option<(PathBuf, u32)>,
    /// `--updated`: this start follows an update, so wait for the build it
    /// replaced to let go before claiming to be the only Pravera.
    pub updated: bool,
}

impl Cli {
    pub fn from_env() -> Cli {
        Cli::parse(std::env::args_os().skip(1))
    }

    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Cli {
        let mut cli = Cli::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str().unwrap_or_default() {
                "--version" | "-V" => cli.version = true,
                "--setup" => cli.setup = true,
                "--install" => cli.install = true,
                "--uninstall" => cli.uninstall = true,
                "--quiet" | "--silent" => cli.quiet = true,
                "--no-launch" => cli.no_launch = true,
                "--desktop-shortcut" => cli.desktop = true,
                "--purge" => cli.purge = true,
                "--updated" => cli.updated = true,
                "--dir" => cli.dir = args.next().map(PathBuf::from),
                "--export-icon" => {
                    let path = args.next().map(PathBuf::from);
                    let size = args
                        .next()
                        .and_then(|size| size.to_str().and_then(|s| s.parse().ok()));
                    if let (Some(path), Some(size)) = (path, size) {
                        cli.export_icon = Some((path, size));
                    }
                }
                _ => {}
            }
        }
        cli
    }
}

/// What this start of the executable is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Version,
    ExportIcon(PathBuf, u32),
    /// Install or uninstall with no window, and exit with the outcome.
    Quiet,
    /// The installer window.
    Setup,
    App,
}

pub fn launch(cli: &Cli) -> Launch {
    if cli.version {
        return Launch::Version;
    }
    if let Some((path, size)) = &cli.export_icon {
        return Launch::ExportIcon(path.clone(), *size);
    }
    if cli.quiet && (cli.install || cli.uninstall) {
        return Launch::Quiet;
    }
    if cli.setup || cli.install || cli.uninstall || named_setup() {
        return Launch::Setup;
    }
    Launch::App
}

/// Whether this file was shipped as the installer. Decided by name, because
/// the name is the one thing a downloaded file and its installed copy differ
/// in: `Pravera-Setup-Windows-x64.exe` installs, `pravera.exe` runs.
fn named_setup() -> bool {
    // An AppImage runs from a mount point; the file somebody double-clicked
    // is in `$APPIMAGE`.
    if let Some(image) = std::env::var_os("APPIMAGE") {
        return is_setup_name(Path::new(&image));
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    // On macOS the binary is always `pravera`; the bundle carries the name.
    if cfg!(target_os = "macos") {
        return exe.ancestors().any(|dir| {
            dir.extension().is_some_and(|ext| ext == "app") && is_setup_name(dir)
        });
    }
    is_setup_name(&exe)
}

fn is_setup_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().contains("setup"))
}

/// Print the version for `--version`. The format is what CI and the updater
/// read back: `pravera 0.1.0`.
pub fn print_version() {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "pravera {VERSION}");
    let _ = out.flush();
}

// --------------------------------------------------------------- the layout

/// Where an installed Pravera lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// The folder the installer owns. On macOS, the folder the bundle is in.
    pub dir: PathBuf,
    /// The executable the shortcuts point at.
    pub exe: PathBuf,
    /// macOS: `Pravera.app`.
    pub bundle: Option<PathBuf>,
}

impl Layout {
    /// The default place for this user.
    pub fn default_for_user() -> Option<Layout> {
        #[cfg(windows)]
        {
            let local = std::env::var_os("LOCALAPPDATA")?;
            Some(Layout::in_dir(PathBuf::from(local).join("Programs").join("Pravera")))
        }
        #[cfg(target_os = "linux")]
        {
            let data = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .or_else(|| home().map(|home| home.join(".local").join("share")))?;
            Some(Layout::in_dir(data.join("pravera")))
        }
        #[cfg(target_os = "macos")]
        {
            Some(Layout::in_dir(macos::applications_dir()))
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    pub fn in_dir(dir: PathBuf) -> Layout {
        // `--dir` comes from a person or a script: make it absolute, and on
        // Windows give it one kind of slash, before it lands in the registry.
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        if cfg!(target_os = "macos") {
            let bundle = dir.join("Pravera.app");
            Layout {
                exe: bundle.join("Contents").join("MacOS").join("pravera"),
                bundle: Some(bundle),
                dir,
            }
        } else {
            let name = if cfg!(windows) { "pravera.exe" } else { "pravera" };
            Layout {
                exe: dir.join(name),
                bundle: None,
                dir,
            }
        }
    }

    /// Where an existing install is, if there is one: the operating system's
    /// record first, where it keeps one, then the default place.
    pub fn find() -> Option<Layout> {
        #[cfg(windows)]
        if let Some(dir) = win::registered_dir() {
            let layout = Layout::in_dir(dir);
            if layout.exe.is_file() {
                return Some(layout);
            }
        }
        Layout::default_for_user()
    }

    fn manifest(&self) -> PathBuf {
        match &self.bundle {
            Some(bundle) => bundle.join("Contents").join("Resources").join("install.json"),
            None => self.dir.join("install.json"),
        }
    }

    /// What is installed here, if anything.
    pub fn installed(&self) -> Option<Installed> {
        if !self.exe.is_file() {
            return None;
        }
        let manifest = std::fs::read_to_string(self.manifest())
            .ok()
            .and_then(|text| serde_json::from_str::<Manifest>(&text).ok());
        Some(Installed {
            version: manifest
                .as_ref()
                .and_then(|m| semver::Version::parse(&m.version).ok()),
            desktop: manifest.is_some_and(|m| m.desktop_shortcut),
        })
    }
}

/// What the installer wrote down about an install.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: String,
    desktop_shortcut: bool,
}

/// An install found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// `None` when the file is there but the record of it is not.
    pub version: Option<semver::Version>,
    pub desktop: bool,
}

// ------------------------------------------------------------- the actions

/// The choices the installer window offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub desktop: bool,
    pub launch: bool,
    pub purge: bool,
}

/// What an action did, one line per step, for the window's summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub steps: Vec<String>,
    /// Things that did not work but did not stop the rest.
    pub warnings: Vec<String>,
}

/// Install this build at `layout`, over whatever is there.
pub fn install(layout: &Layout, options: Options) -> Result<Report, String> {
    let mut report = Report::default();

    if !crate::single_instance::ask_first_to_quit(Duration::from_secs(15)) {
        report
            .warnings
            .push("A running Pravera did not close; it switches to this version when it restarts.".into());
    } else {
        report.steps.push("Closed any running Pravera".into());
    }

    std::fs::create_dir_all(&layout.dir)
        .map_err(|error| format!("Could not create {}: {error}", layout.dir.display()))?;

    #[cfg(target_os = "macos")]
    macos::place_bundle(layout).map_err(|error| format!("Could not copy Pravera into place: {error}"))?;
    #[cfg(not(target_os = "macos"))]
    {
        // The executable itself — on Linux, the one inside the AppImage
        // rather than the AppImage. An installed AppImage would need FUSE to
        // start every time, which a stock Ubuntu 24.04 does not have; the
        // program inside needs nothing but the libraries every desktop has.
        let source = std::env::current_exe().map_err(|error| format!("Could not find this installer: {error}"))?;
        replace_file(&source, &layout.exe)
            .map_err(|error| format!("Could not copy Pravera to {}: {error}", layout.exe.display()))?;
    }
    report.steps.push(format!("Copied Pravera {VERSION} to {}", layout.dir.display()));

    let manifest = Manifest {
        version: VERSION.to_string(),
        desktop_shortcut: options.desktop,
    };
    if let Some(parent) = layout.manifest().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(
        layout.manifest(),
        serde_json::to_string_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|error| format!("Could not write the install record: {error}"))?;

    integrate(layout, options, &mut report);
    sweep_retired(&layout.exe);

    if options.launch {
        if let Err(error) = open(layout) {
            report.warnings.push(format!("Could not open Pravera: {error}"));
        }
    }
    Ok(report)
}

/// Remove the install at `layout`.
pub fn uninstall(layout: &Layout, options: Options) -> Result<Report, String> {
    let mut report = Report::default();

    if crate::single_instance::ask_first_to_quit(Duration::from_secs(15)) {
        report.steps.push("Closed any running Pravera".into());
    }

    // Before the files: a start-at-sign-in entry or a service left pointing at
    // a deleted file fails quietly every time it fires, forever.
    //
    // Only the ones that start *this* copy: removing a test install, or an old
    // portable copy, must leave the Pravera that is really in use alone.
    if crate::autostart::registered_for(&layout.exe) {
        match crate::autostart::set(false) {
            Ok(()) => report.steps.push("Removed the start-at-sign-in entry".into()),
            Err(error) => report.warnings.push(error),
        }
    }
    if pravera_service::registered_for(&layout.exe) {
        match pravera_service::uninstall() {
            Ok(done) => report.steps.push(done),
            Err(_) => report.warnings.push(
                "The Pravera service is still registered, and removing it needs an administrator. \
                 From an administrator prompt: sc delete Pravera"
                    .into(),
            ),
        }
    }

    disintegrate(layout, &mut report);

    let target = layout.bundle.as_ref().unwrap_or(&layout.dir);
    match remove_install(layout) {
        Ok(()) => report.steps.push(format!("Deleted {}", target.display())),
        Err(error) => report
            .warnings
            .push(format!("Some files in {} could not be deleted: {error}", target.display())),
    }

    if options.purge {
        for dir in [pravera_core::paths::data_dir().ok(), pravera_core::paths::config_dir().ok()]
            .into_iter()
            .flatten()
        {
            if dir.exists() {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => report.steps.push(format!("Deleted {}", dir.display())),
                    Err(error) => report
                        .warnings
                        .push(format!("Could not delete {}: {error}", dir.display())),
                }
            }
        }
        let shared = pravera_core::paths::service_data_dir();
        if shared.exists() && std::fs::remove_dir_all(&shared).is_err() {
            report.warnings.push(format!(
                "{} is shared by every account on this machine and needs an administrator to delete.",
                shared.display()
            ));
        }
    }
    Ok(report)
}

/// Open the installed Pravera, detached from this process.
pub fn open(layout: &Layout) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if let Some(bundle) = &layout.bundle {
        return std::process::Command::new("/usr/bin/open")
            .arg(bundle)
            .spawn()
            .map(|_| ());
    }
    spawn_detached(&layout.exe, &[])
}

fn integrate(layout: &Layout, options: Options, report: &mut Report) {
    #[cfg(windows)]
    win::integrate(layout, options.desktop, report);
    #[cfg(target_os = "linux")]
    linux::integrate(layout, options.desktop, report);
    #[cfg(target_os = "macos")]
    macos::integrate(layout, report);
    let _ = (layout, options, report);
}

fn disintegrate(layout: &Layout, report: &mut Report) {
    #[cfg(windows)]
    win::disintegrate(report);
    #[cfg(target_os = "linux")]
    linux::disintegrate(report);
    let _ = (layout, report);
}

fn remove_install(layout: &Layout) -> std::io::Result<()> {
    if let Some(bundle) = &layout.bundle {
        return std::fs::remove_dir_all(bundle);
    }
    // Only what the installer put there. `--dir` may have pointed somewhere
    // that holds other things too, and an uninstaller that emptied a folder
    // it did not create would be a disaster with a progress bar.
    sweep_retired(&layout.exe);
    let mut failed = None;
    for file in [layout.exe.clone(), layout.manifest()] {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                // Still running (the service restarted it, say): move it out of
                // the way so the folder goes, and let it be deleted later.
                #[cfg(windows)]
                if win::delete_later(&file).is_ok() {
                    continue;
                }
                failed = Some(error);
            }
        }
    }
    let _ = std::fs::remove_dir(&layout.dir);
    match failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

// ------------------------------------------------------------ file helpers

/// The file an update replaces: the AppImage somebody ran, where it is one,
/// not the mount it runs from.
pub fn self_image() -> std::io::Result<PathBuf> {
    if let Some(image) = std::env::var_os("APPIMAGE") {
        return Ok(PathBuf::from(image));
    }
    std::env::current_exe()
}

fn sibling(target: &Path, suffix: &str) -> PathBuf {
    let mut name = target.file_name().map(OsString::from).unwrap_or_default();
    name.push(".");
    name.push(suffix);
    target.with_file_name(name)
}

/// Put `source` at `target`, even while `target` is running.
///
/// Staged first, so a copy that fails half-way leaves the old program intact.
/// On Windows the running file is renamed aside, which Windows allows, and the
/// new one takes its name; elsewhere a rename over a running file is atomic
/// and the running process keeps its open inode.
pub fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    let staged = sibling(target, "new");
    let _ = std::fs::remove_file(&staged);
    std::fs::copy(source, &staged)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }

    #[cfg(windows)]
    if target.exists() {
        let retired = retire(target)?;
        if let Err(error) = std::fs::rename(&staged, target) {
            // Put the old one back rather than leave nothing to start.
            let _ = std::fs::rename(&retired, target);
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        return Ok(());
    }

    std::fs::rename(&staged, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&staged);
    })
}

/// Rename a (possibly running) file aside. A previous retiree that is itself
/// still running cannot be deleted, so the name is varied until one is free.
#[cfg(windows)]
fn retire(target: &Path) -> std::io::Result<PathBuf> {
    for n in 0..32 {
        let old = sibling(target, &if n == 0 { "old".to_string() } else { format!("old{n}") });
        let _ = std::fs::remove_file(&old);
        if !old.exists() {
            std::fs::rename(target, &old)?;
            return Ok(old);
        }
    }
    let old = sibling(target, &format!("old-{}", std::process::id()));
    std::fs::rename(target, &old)?;
    Ok(old)
}

/// Delete whatever [`replace_file`] renamed aside, where nothing is still
/// running it. Called at every start, so leftovers last one restart at most.
pub fn sweep_retired(target: &Path) {
    let Some(dir) = target.parent() else { return };
    let Some(stem) = target.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let prefix = format!("{stem}.old");
    let staged = format!("{stem}.new");
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&prefix) || name == staged {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Start `exe` with `args`, owned by nobody: no console, no inherited handles
/// the caller is about to close.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<()> {
    let mut command = std::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_| ())
}

#[cfg(unix)]
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|path| path.is_absolute())
}

// --------------------------------------------------------------- the icon

pub(crate) const LOGO_SVG: &str = include_str!("../../assets/logo.svg");

/// The app icon at `size` pixels square, as a PNG. The same 64×64 vector the
/// Windows `.ico` is rendered from in `build.rs`.
pub fn icon_png(size: u32) -> Result<Vec<u8>, String> {
    let tree = resvg::usvg::Tree::from_str(LOGO_SVG, &resvg::usvg::Options::default())
        .map_err(|error| error.to_string())?;
    let mut pixmap = tiny_skia::Pixmap::new(size, size).ok_or("that size is not drawable")?;
    let scale = size as f32 / 64.0;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|error| error.to_string())
}

pub fn export_icon(path: &Path, size: u32) -> i32 {
    match icon_png(size).and_then(|png| std::fs::write(path, png).map_err(|e| e.to_string())) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("pravera: could not write the icon: {error}");
            1
        }
    }
}

// ----------------------------------------------------------- quiet actions

/// `--install --quiet` and `--uninstall --quiet`: the same work as the
/// window, reported on stdout, with the exit code saying how it went.
pub fn run_quiet(cli: &Cli) -> i32 {
    let layout = match &cli.dir {
        Some(dir) => Some(Layout::in_dir(dir.clone())),
        None if cli.uninstall => Layout::find(),
        None => Layout::default_for_user(),
    };
    let Some(layout) = layout else {
        eprintln!("pravera: there is no per-user install location on this system; pass --dir");
        return 1;
    };
    let options = Options {
        desktop: cli.desktop,
        launch: cli.install && !cli.no_launch,
        purge: cli.purge,
    };
    let outcome = if cli.uninstall {
        #[cfg(windows)]
        if win::runs_from(&layout) {
            // Cannot delete the file this process is running from; hand the
            // job to a copy in the temp folder and let it finish.
            return match win::hand_off_uninstall() {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("pravera: {error}");
                    1
                }
            };
        }
        uninstall(&layout, options)
    } else {
        install(&layout, options)
    };
    #[cfg(windows)]
    win::delete_self_if_temporary();
    match outcome {
        Ok(report) => {
            for step in &report.steps {
                println!("{step}");
            }
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            0
        }
        Err(error) => {
            eprintln!("pravera: {error}");
            1
        }
    }
}

#[cfg(windows)]
pub use win::{delete_self_if_temporary, hand_off_uninstall, runs_from};

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Cli {
        Cli::parse(list.iter().map(OsString::from))
    }

    #[test]
    fn the_release_names_are_the_ones_ci_publishes() {
        // `.github/workflows/ci.yml` checks the release for exactly these
        // names, and every installed copy looks for its own among them.
        let all = [
            "Pravera-Setup-Windows-x64.exe",
            "Pravera-Setup-macOS-arm64.zip",
            "Pravera-Setup-Linux-x86_64.AppImage",
        ];
        assert!(all.contains(&ASSET));
        assert!(is_setup_name(Path::new(ASSET)));
    }

    #[test]
    fn only_the_setup_name_opens_the_installer() {
        assert!(is_setup_name(Path::new(r"C:\Downloads\Pravera-Setup-Windows-x64.exe")));
        assert!(is_setup_name(Path::new("/Volumes/x/Pravera Setup.app")));
        assert!(!is_setup_name(Path::new(r"C:\Users\a\AppData\Local\Programs\Pravera\pravera.exe")));
        assert!(!is_setup_name(Path::new("/home/a/.local/share/pravera/pravera")));
    }

    #[test]
    fn flags_parse_in_any_order() {
        let cli = args(&["--quiet", "--dir", "/tmp/x", "--install", "--no-launch"]);
        assert!(cli.install && cli.quiet && cli.no_launch);
        assert_eq!(cli.dir, Some(PathBuf::from("/tmp/x")));
        assert_eq!(launch(&cli), Launch::Quiet);

        let cli = args(&["--export-icon", "icon.png", "256"]);
        assert_eq!(launch(&cli), Launch::ExportIcon(PathBuf::from("icon.png"), 256));

        // The service's and agent's own flags are not the installer's business.
        let cli = args(&["--agent", "--hidden"]);
        assert_eq!(cli, Cli::default());
    }

    #[test]
    fn version_wins_over_everything() {
        assert_eq!(launch(&args(&["--install", "--version"])), Launch::Version);
    }

    #[test]
    fn this_build_has_a_version_the_updater_can_compare() {
        assert_eq!(current_version().to_string(), VERSION);
    }

    #[test]
    fn a_replacement_lands_and_the_leftovers_go() {
        let dir = std::env::temp_dir().join(format!("pravera-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("new-build");
        let target = dir.join("pravera-test.bin");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&target, b"old").unwrap();

        replace_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        sweep_retired(&target);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.iter().all(|name| !name.contains(".old") && !name.ends_with(".new")), "{left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_record_is_read_back() {
        let dir = std::env::temp_dir().join(format!("pravera-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout::in_dir(dir.clone());
        assert_eq!(layout.installed(), None);

        std::fs::create_dir_all(layout.exe.parent().unwrap()).unwrap();
        std::fs::write(&layout.exe, b"x").unwrap();
        assert_eq!(layout.installed().unwrap().version, None);

        std::fs::create_dir_all(layout.manifest().parent().unwrap()).unwrap();
        std::fs::write(layout.manifest(), r#"{"version":"1.2.3","desktop_shortcut":true}"#).unwrap();
        let found = layout.installed().unwrap();
        assert_eq!(found.version, Some(semver::Version::new(1, 2, 3)));
        assert!(found.desktop);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_icon_renders() {
        let png = icon_png(64).unwrap();
        assert_eq!(&png[1..4], b"PNG");
    }
}

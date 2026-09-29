//! macOS: `Pravera Setup.app` builds `Pravera.app` in the Applications
//! folder from its own executable and icon, with its own name in the
//! Info.plist, and signs the result ad hoc so the bundle's seal matches what
//! is in it.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{home, Layout, Report, VERSION};

/// `/Applications` when this user may write there (an administrator account,
/// the usual case), their own `~/Applications` when not.
pub fn applications_dir() -> PathBuf {
    let shared = PathBuf::from("/Applications");
    let probe = shared.join(format!(".pravera-probe-{}", std::process::id()));
    if std::fs::write(&probe, b"").is_ok() {
        let _ = std::fs::remove_file(probe);
        return shared;
    }
    home()
        .map(|home| home.join("Applications"))
        .unwrap_or(shared)
}

fn info_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Pravera</string>
  <key>CFBundleDisplayName</key><string>Pravera</string>
  <key>CFBundleIdentifier</key><string>com.n1ssyyy.pravera</string>
  <key>CFBundleExecutable</key><string>pravera</string>
  <key>CFBundleIconFile</key><string>pravera</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{VERSION}</string>
  <key>CFBundleVersion</key><string>{VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>Pravera sends this Mac's sound to the machine viewing it.</string>
</dict>
</plist>
"#
    )
}

/// The bundle this process runs from: `…/X.app/Contents/MacOS/pravera`.
fn own_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
        .map(Path::to_path_buf)
}

pub fn place_bundle(layout: &Layout) -> std::io::Result<()> {
    let bundle = layout.bundle.as_ref().expect("a macOS layout has a bundle");
    let contents = bundle.join("Contents");
    let binaries = contents.join("MacOS");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&binaries)?;
    std::fs::create_dir_all(&resources)?;

    let exe = std::env::current_exe()?;
    super::replace_file(&exe, &layout.exe)?;
    std::fs::write(contents.join("Info.plist"), info_plist())?;
    if let Some(icon) = own_bundle().map(|b| b.join("Contents").join("Resources").join("pravera.icns")) {
        if icon.is_file() {
            let _ = std::fs::copy(icon, resources.join("pravera.icns"));
        }
    }
    Ok(())
}

pub fn integrate(layout: &Layout, report: &mut Report) {
    let Some(bundle) = &layout.bundle else { return };
    reseal(bundle);
    report.steps.push("Signed the app for this Mac".into());
}

/// Sign the bundle ad hoc and clear the download quarantine, so Gatekeeper
/// judges the app somebody already approved as the same app. Called after
/// every change to the bundle: a binary swapped in by an update no longer
/// matches the old seal.
pub fn reseal(bundle: &Path) {
    let _ = Command::new("/usr/bin/codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(bundle)
        .output();
    let _ = Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(bundle)
        .output();
}

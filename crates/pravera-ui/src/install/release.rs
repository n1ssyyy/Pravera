//! Updates: what the newest release is, fetching it, checking it, and
//! putting it in place of the running program.
//!
//! The feed is GitHub's own release API for [`REPO`](super::REPO), read with
//! no token — sixty requests an hour per address, against one check every few
//! hours. What is downloaded is exactly the installer a person would download
//! by hand ([`ASSET`](super::ASSET)), and before it replaces anything it has
//! to pass three checks:
//!
//! 1. its size is the size the release says;
//! 2. its SHA-256 is the digest GitHub computed when it was uploaded;
//! 3. it runs, and says it is the version the release says it is.
//!
//! The third is the one that matters most in practice: a truncated or
//! mislabelled file never gets to replace a working program.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{ASSET, REPO, VERSION};

/// A published release this build could update to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    /// The release page, for "what's new".
    pub page: String,
    pub asset: Asset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex, when GitHub reports one.
    pub sha256: Option<String>,
}

/// The version to compare releases against: this build's, except that a debug
/// build may pretend to be older (`PRAVERA_UPDATE_FROM=0.0.1`) so the whole
/// path can be exercised against a real release without cutting a new one.
pub fn running_version() -> semver::Version {
    if cfg!(debug_assertions) {
        if let Some(pretend) = std::env::var("PRAVERA_UPDATE_FROM")
            .ok()
            .and_then(|v| semver::Version::parse(v.trim().trim_start_matches('v')).ok())
        {
            return pretend;
        }
    }
    super::current_version()
}

/// Whether updating is something this process should do at all. Release
/// builds always; a debug build only when told to pretend, because an update
/// would replace `target/debug/pravera.exe` with a release build.
pub fn enabled() -> bool {
    if cfg!(debug_assertions) {
        return std::env::var_os("PRAVERA_UPDATE_FROM").is_some();
    }
    // A release build still sitting in Cargo's output folder belongs to
    // whoever is building it, not to the release feed.
    let in_cargo_target = super::self_image().is_ok_and(|exe| {
        exe.components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case("target"))
            && exe
                .parent()
                .and_then(|dir| dir.file_name())
                .is_some_and(|name| name == "release" || name == "debug" || name == "deps")
    });
    !in_cargo_target
}

fn client() -> Result<reqwest::Client, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(format!("Pravera/{VERSION} (+https://github.com/{REPO})"))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| error.to_string())
}

/// The newest published release, whatever its version.
pub async fn latest() -> Result<Release, String> {
    let response = client()?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("Could not reach GitHub: {error}"))?;
    match response.status().as_u16() {
        200 => {}
        404 => return Err("No release has been published yet.".into()),
        403 | 429 => return Err("GitHub is rate-limiting this address; trying again later.".into()),
        code => return Err(format!("GitHub answered {code}.")),
    }
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("The release feed was cut off: {error}"))?;
    parse_latest(&body)
}

/// Read the API's answer. Separate from [`latest`] so it can be tested
/// against a saved response.
pub fn parse_latest(body: &[u8]) -> Result<Release, String> {
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| format!("The release feed is not JSON: {error}"))?;
    let tag = json["tag_name"].as_str().ok_or("The release has no tag.")?;
    let version = semver::Version::parse(tag.trim_start_matches('v'))
        .map_err(|_| format!("The release tag {tag} is not a version."))?;
    let asset = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| asset["name"].as_str() == Some(ASSET))
        .ok_or_else(|| format!("Release {tag} has no {ASSET}."))?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    Ok(Release {
        version,
        page: json["html_url"].as_str().unwrap_or_default().to_string(),
        asset: Asset {
            name: ASSET.to_string(),
            url: asset["browser_download_url"]
                .as_str()
                .ok_or("The release asset has no download address.")?
                .to_string(),
            size: asset["size"].as_u64().unwrap_or(0),
            sha256,
        },
    })
}

/// Where downloads wait before they are applied. One folder per version, so a
/// half-finished download of one never passes for another.
fn staging(version: &semver::Version) -> PathBuf {
    std::env::temp_dir().join(format!("pravera-update-{version}"))
}

/// Download `release`, calling `progress(done, total)` as it arrives, and
/// check it. Returns the executable to swap in.
pub async fn download(
    release: &Release,
    mut progress: impl FnMut(u64, u64),
) -> Result<PathBuf, String> {
    let dir = staging(&release.version);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|error| format!("Could not stage the update: {error}"))?;
    let file_path = dir.join(&release.asset.name);

    let mut response = client()?
        .get(&release.asset.url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("The download did not start: {error}"))?;
    let total = response.content_length().unwrap_or(release.asset.size);

    let mut file = std::fs::File::create(&file_path).map_err(|error| error.to_string())?;
    let mut hash = Sha256::new();
    let mut done = 0u64;
    let mut reported = Instant::now();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("The download was cut off: {error}"))?
    {
        file.write_all(&chunk).map_err(|error| error.to_string())?;
        hash.update(&chunk);
        done += chunk.len() as u64;
        if reported.elapsed() >= Duration::from_millis(100) {
            reported = Instant::now();
            progress(done, total);
        }
    }
    file.flush().map_err(|error| error.to_string())?;
    drop(file);
    progress(done, total);

    if release.asset.size != 0 && done != release.asset.size {
        return Err(format!(
            "The download is {done} bytes; the release says {}.",
            release.asset.size
        ));
    }
    if let Some(expected) = &release.asset.sha256 {
        let actual: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if &actual != expected {
            return Err("The download does not match the release's checksum.".into());
        }
    }

    let executable = tokio::task::spawn_blocking({
        let file_path = file_path.clone();
        let dir = dir.clone();
        move || unpack(&file_path, &dir)
    })
    .await
    .map_err(|error| error.to_string())??;

    let version = release.version.clone();
    let checked = executable.clone();
    tokio::task::spawn_blocking(move || runs_as(&checked, &version))
        .await
        .map_err(|error| error.to_string())??;
    Ok(executable)
}

/// The executable inside what was downloaded.
fn unpack(file: &Path, dir: &Path) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let out = dir.join("unpacked");
        let status = std::process::Command::new("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(file)
            .arg(&out)
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err("The downloaded archive could not be opened.".into());
        }
        let exe = out
            .join("Pravera Setup.app")
            .join("Contents")
            .join("MacOS")
            .join("pravera");
        return if exe.is_file() {
            Ok(exe)
        } else {
            Err("The downloaded archive has no Pravera in it.".into())
        };
    }
    #[cfg(target_os = "linux")]
    {
        // What an install holds is the program inside the AppImage (see
        // `install::install`), so that is what an update swaps in. Unpacking
        // one file needs no FUSE.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
        let status = std::process::Command::new(file)
            .args(["--appimage-extract", "usr/bin/pravera"])
            .current_dir(dir)
            .env_remove("APPIMAGE_EXTRACT_AND_RUN")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|error| error.to_string())?;
        let exe = dir.join("squashfs-root").join("usr").join("bin").join("pravera");
        return if status.success() && exe.is_file() {
            Ok(exe)
        } else {
            Err("The downloaded AppImage could not be unpacked.".into())
        };
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = dir;
        Ok(file.to_path_buf())
    }
}

/// Run `exe --version` and insist it names `version`.
pub fn runs_as(exe: &Path, version: &semver::Version) -> Result<(), String> {
    let mut child = std::process::Command::new(exe)
        .arg("--version")
        // An AppImage needs FUSE to mount itself; unpacking instead works on
        // every machine, including the ones without it.
        .env("APPIMAGE_EXTRACT_AND_RUN", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("The new version would not start: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                return Err("The new version did not answer.".into());
            }
        }
    }
    let mut said = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut said);
    }
    let expected = format!("pravera {version}");
    if said.trim() == expected {
        Ok(())
    } else {
        Err(format!(
            "The download says it is \"{}\", not \"{expected}\".",
            said.trim()
        ))
    }
}

/// Put the checked executable in place of the running one. Returns what to
/// start afterwards. `version` is what the new build is, for the install
/// record.
pub fn apply(executable: &Path, version: &semver::Version) -> Result<PathBuf, String> {
    let target = super::self_image().map_err(|error| error.to_string())?;
    // A Pravera run straight from its AppImage, never installed, is replaced
    // by the new AppImage rather than by the program inside it.
    let source = if cfg!(target_os = "linux") && std::env::var_os("APPIMAGE").is_some() {
        staging(version).join(ASSET)
    } else {
        executable.to_path_buf()
    };
    super::replace_file(&source, &target)
        .map_err(|error| format!("Could not replace {}: {error}", target.display()))?;
    #[cfg(target_os = "macos")]
    if let Some(bundle) = target
        .ancestors()
        .find(|dir| dir.extension().is_some_and(|ext| ext == "app"))
    {
        // The icon may have changed with the build.
        if let Some(icon) = executable
            .ancestors()
            .nth(2)
            .map(|contents| contents.join("Resources").join("pravera.icns"))
            .filter(|icon| icon.is_file())
        {
            let _ = std::fs::copy(icon, bundle.join("Contents").join("Resources").join("pravera.icns"));
        }
        super::macos::reseal(bundle);
    }
    // Keep the install record honest, where there is one.
    if let Some(layout) = super::Layout::find().filter(|layout| {
        layout.exe.canonicalize().ok() == target.canonicalize().ok()
    }) {
        if let Some(installed) = layout.installed() {
            let manifest = super::Manifest {
                version: version.to_string(),
                desktop_shortcut: installed.desktop,
            };
            let _ = std::fs::write(
                layout.manifest(),
                serde_json::to_string_pretty(&manifest).unwrap_or_default(),
            );
        }
    }
    Ok(target)
}

/// Start the new build. `hidden` carries over a Pravera that was in the
/// notification area, so an update does not pop a window up on its own.
pub fn relaunch(target: &Path, hidden: bool) -> std::io::Result<()> {
    let mut args = vec!["--updated"];
    if hidden {
        args.push(crate::autostart::HIDDEN_FLAG);
    }
    super::spawn_detached(target, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(tag: &str, asset: &str) -> Vec<u8> {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/{REPO}/releases/tag/{tag}",
                "assets": [
                    {{ "name": "something-else.txt", "browser_download_url": "https://x/y", "size": 1 }},
                    {{ "name": "{asset}", "browser_download_url": "https://example.test/{asset}",
                       "size": 123, "digest": "sha256:ABCDEF" }}
                ]
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn the_feed_finds_this_platforms_installer() {
        let release = parse_latest(&feed("v9.8.7", ASSET)).unwrap();
        assert_eq!(release.version, semver::Version::new(9, 8, 7));
        assert_eq!(release.asset.size, 123);
        assert_eq!(release.asset.sha256.as_deref(), Some("abcdef"));
        assert!(release.asset.url.ends_with(ASSET));
    }

    #[test]
    fn a_release_without_this_platform_is_not_an_update() {
        assert!(parse_latest(&feed("v9.8.7", "Pravera-Setup-Amiga.adf")).is_err());
    }

    #[test]
    fn a_tag_that_is_not_a_version_is_refused() {
        assert!(parse_latest(&feed("latest", ASSET)).is_err());
    }

    /// The whole update path against the real release: find it, download it,
    /// check its size, digest and `--version`. Needs the network and a
    /// published release, so it only runs when asked:
    /// `cargo test -p pravera-ui --lib -- --ignored published_release`.
    #[test]
    #[ignore]
    fn the_published_release_downloads_and_verifies() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let release = runtime.block_on(latest()).expect("a published release");
        assert_eq!(release.asset.name, ASSET);
        assert!(release.asset.sha256.is_some(), "GitHub reports a digest");
        let mut last = (0, 0);
        let executable = runtime
            .block_on(download(&release, |done, total| last = (done, total)))
            .expect("the download verifies");
        assert!(executable.is_file());
        assert_eq!(last.0, release.asset.size);
    }

    #[test]
    fn the_same_version_is_not_newer() {
        let release = parse_latest(&feed(&format!("v{VERSION}"), ASSET)).unwrap();
        assert!(release.version <= super::super::current_version());
    }
}

//! Linux: a desktop entry and an icon under `~/.local/share`, and a `pravera`
//! on the `PATH` in `~/.local/bin`. The freedesktop per-user locations, so no
//! root and nothing for a package manager to trip over.

use std::path::PathBuf;

use super::{home, icon_png, Layout, Report};

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home().map(|home| home.join(".local").join("share")))
}

fn entry_path() -> Option<PathBuf> {
    data_home().map(|data| data.join("applications").join("pravera.desktop"))
}

fn icon_path() -> Option<PathBuf> {
    data_home().map(|data| {
        data.join("icons")
            .join("hicolor")
            .join("256x256")
            .join("apps")
            .join("pravera.png")
    })
}

fn link_path() -> Option<PathBuf> {
    home().map(|home| home.join(".local").join("bin").join("pravera"))
}

fn desktop_dir() -> Option<PathBuf> {
    // `xdg-user-dir DESKTOP` would be exact, but it is not always installed,
    // and every desktop that has one defaults it to this.
    home().map(|home| home.join("Desktop"))
}

fn entry(layout: &Layout) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Pravera\n\
         GenericName=Remote Desktop\n\
         Comment=Peer-to-peer remote desktop\n\
         Exec=\"{}\"\n\
         Icon=pravera\n\
         Terminal=false\n\
         Categories=Network;RemoteAccess;\n\
         Keywords=remote;desktop;vnc;rdp;\n\
         StartupWMClass=pravera\n",
        layout.exe.display()
    )
}

pub fn integrate(layout: &Layout, desktop: bool, report: &mut Report) {
    if let Some(path) = icon_path() {
        let written = icon_png(256).and_then(|png| {
            std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string())?;
            std::fs::write(&path, png).map_err(|e| e.to_string())
        });
        if let Err(error) = written {
            report.warnings.push(format!("Could not install the icon: {error}"));
        }
    }

    match entry_path() {
        Some(path) => {
            let written = std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .and_then(|()| std::fs::write(&path, entry(layout)));
            match written {
                Ok(()) => report.steps.push("Added Pravera to the applications menu".into()),
                Err(error) => report
                    .warnings
                    .push(format!("Could not add the menu entry: {error}")),
            }
        }
        None => report.warnings.push("There is no home folder to add a menu entry to".into()),
    }

    if let Some(link) = link_path() {
        let _ = std::fs::create_dir_all(link.parent().unwrap_or(&link));
        let _ = std::fs::remove_file(&link);
        match std::os::unix::fs::symlink(&layout.exe, &link) {
            Ok(()) => report.steps.push(format!("Linked {}", link.display())),
            Err(error) => report
                .warnings
                .push(format!("Could not link {}: {error}", link.display())),
        }
    }

    if let Some(dir) = desktop_dir() {
        let shortcut = dir.join("pravera.desktop");
        if desktop && dir.is_dir() {
            use std::os::unix::fs::PermissionsExt;
            let written = std::fs::write(&shortcut, entry(layout)).and_then(|()| {
                std::fs::set_permissions(&shortcut, std::fs::Permissions::from_mode(0o755))
            });
            match written {
                Ok(()) => report.steps.push("Added a desktop shortcut".into()),
                Err(error) => report
                    .warnings
                    .push(format!("Could not add the desktop shortcut: {error}")),
            }
        } else if !desktop {
            let _ = std::fs::remove_file(shortcut);
        }
    }
}

pub fn disintegrate(report: &mut Report) {
    let mut removed = false;
    for path in [
        entry_path(),
        icon_path(),
        link_path(),
        desktop_dir().map(|dir| dir.join("pravera.desktop")),
    ]
    .into_iter()
    .flatten()
    {
        removed |= std::fs::remove_file(path).is_ok();
    }
    if removed {
        report.steps.push("Removed the menu entry, icon and shortcuts".into());
    }
}

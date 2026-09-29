//! Enumerating Windows displays.
//!
//! `windows-capture` knows how to *capture* a monitor but says nothing about
//! where it sits on the desktop, whether it is the primary, or how the user
//! has scaled it. All three matter — position and primary decide the display
//! ids the protocol uses, and scale is what lets the client size the pointer
//! correctly — so this reaches past it to the Win32 calls that do know.
//!
//! ## What is behind a display
//!
//! `EnumDisplayMonitors` lists more than screens. A machine with nothing
//! plugged in still gets a desktop, and with it a monitor: either one with no
//! active DisplayConfig path at all (nothing composes to it), or Windows'
//! `Generic Non-PnP Monitor` — the `Default_Monitor` it invents for an output
//! that returned no EDID. Neither is a screen anybody is looking at, and
//! capturing one produces nothing or a dead framebuffer. The bundled virtual
//! display, on the other hand, is a real composited monitor that announces
//! itself with the driver's EDID (`MTT1337`, "VDD by MTT").
//!
//! [`Kind`] names those three cases from the DisplayConfig *target device
//! path* — `\\?\DISPLAY#<EDID id>#...` — which is the one identifier Windows
//! derives from the monitor rather than from the adapter or the user's naming.

use std::collections::HashMap;

use pravera_core::Resolution;
use tracing::debug;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, RECT};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows_capture::monitor::Monitor;

use crate::{CaptureError, Display, DisplayId, Result};

/// Dots per inch Windows calls 100% scaling.
const BASELINE_DPI: f32 = 96.0;

/// The EDID id the bundled MttVDD driver gives its monitors (`MTT`, `0x1337`).
const VIRTUAL_EDID_ID: &str = "#mtt1337#";

/// The id Windows gives a monitor that returned no EDID at all.
const NO_EDID_ID: &str = "#default_monitor#";

/// What sits behind a display, as far as capture is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A monitor with an EDID: a real screen, a TV, or a dummy plug.
    Physical,
    /// The bundled virtual display driver's monitor.
    Virtual,
    /// Windows' stand-in when nothing is attached. See the module docs.
    Placeholder,
}

/// One enumerated monitor, before the list is filtered and numbered.
pub(crate) struct Entry {
    pub display: Display,
    pub monitor: Monitor,
    pub kind: Kind,
    /// `\\.\DISPLAYn`, the name `ChangeDisplaySettingsExW` takes.
    pub gdi_name: String,
    /// Whether an active DisplayConfig path drives this monitor. `false` for
    /// the headless pseudo-desktop, which nothing composes to.
    pub active: bool,
}

/// Every monitor Windows lists, classified, in no particular order.
///
/// Nothing is dropped here; [`enumerate`] decides what is worth offering.
/// `Err(NoDisplays)` only when Windows lists nothing at all.
pub(crate) fn inventory() -> Result<Vec<Entry>> {
    let monitors = Monitor::enumerate().map_err(CaptureError::backend)?;
    if monitors.is_empty() {
        return Err(CaptureError::NoDisplays);
    }

    // An RDP session's displays belong to the remote client and DisplayConfig
    // describes them inconsistently; classifying there could hide the only
    // screen the session has. Nothing is inferred from paths in that case.
    let targets = if is_remote_session() {
        None
    } else {
        active_targets()
    };
    // A lookup that failed part-way cannot prove a monitor has no active
    // path, so only a complete answer counts as evidence of absence.
    let reliable = targets
        .as_ref()
        .filter(|targets| targets.complete)
        .map(|targets| targets.paths.len());
    let mut entries = Vec::with_capacity(monitors.len());
    for monitor in monitors {
        let display = match describe(&monitor) {
            Ok(display) => display,
            // One unreadable monitor should not cost the operator the others.
            // This happens in practice while a display is being hot-plugged.
            Err(error) => {
                debug!(%error, "skipping a monitor that could not be described");
                continue;
            }
        };
        let gdi_name = monitor.device_name().unwrap_or_default();
        let path = targets
            .as_ref()
            .and_then(|targets| targets.paths.get(&gdi_name));
        let kind = classify(reliable, path.map(String::as_str), &display.name);
        entries.push(Entry {
            active: path.is_some(),
            display,
            monitor,
            kind,
            gdi_name,
        });
    }

    if entries.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    Ok(entries)
}

/// Every display worth offering, in protocol order, each paired with the
/// handle needed to capture it.
///
/// Two kinds of placeholder are left out. A monitor with no active path is
/// never composited, so it can only ever produce silence. And once the virtual
/// display exists, the no-EDID leftovers of a headless machine are dropped
/// too: the virtual display is that machine's screen now, and keeping a dead
/// framebuffer in the list — possibly as display 0 — is exactly how a session
/// ended up black. A `Default_Monitor` that *is* being composited stays when
/// there is nothing better, because it may be the only thing there is.
pub(crate) fn enumerate() -> Result<Vec<(Display, Monitor)>> {
    Ok(offered()?
        .into_iter()
        .map(|(display, monitor, _)| (display, monitor))
        .collect())
}

/// [`enumerate`], keeping each display's [`Kind`] — so the virtual display
/// can be found under the id capture will actually use for it.
pub(crate) fn offered() -> Result<Vec<(Display, Monitor, Kind)>> {
    let entries = inventory()?;
    let has_virtual = entries.iter().any(|e| e.kind == Kind::Virtual);
    let any_active = entries.iter().any(|e| e.active);

    let kept: Vec<(Display, (Monitor, Kind))> = entries
        .into_iter()
        .filter(|entry| {
            if entry.kind != Kind::Placeholder {
                return true;
            }
            if has_virtual {
                return false;
            }
            // No active path while others have one: nothing draws there.
            !(any_active && !entry.active)
        })
        .map(|entry| (entry.display, (entry.monitor, entry.kind)))
        .collect();

    if kept.is_empty() {
        return Err(CaptureError::NoDisplays);
    }
    Ok(Display::normalise_with(kept)
        .into_iter()
        .map(|(display, (monitor, kind))| (display, monitor, kind))
        .collect())
}

/// Look up one display by the id the protocol uses.
pub(crate) fn find(id: DisplayId) -> Result<(Display, Monitor)> {
    enumerate()?
        .into_iter()
        .find(|(display, _)| display.id == id)
        .ok_or(CaptureError::NoSuchDisplay(id))
}

/// Classify one monitor.
///
/// `active_paths` is how many active DisplayConfig paths this session has, or
/// `None` when DisplayConfig could not be asked (some sessions refuse it).
/// `path` is this monitor's target device path, when one of those paths
/// drives it.
pub(crate) fn classify(active_paths: Option<usize>, path: Option<&str>, name: &str) -> Kind {
    if let Some(path) = path {
        let path = path.to_ascii_lowercase();
        if path.contains(VIRTUAL_EDID_ID) {
            return Kind::Virtual;
        }
        if path.contains(NO_EDID_ID) {
            return Kind::Placeholder;
        }
        return Kind::Physical;
    }
    match active_paths {
        // DisplayConfig works here and nothing drives this monitor — including
        // the headless case where there are no active paths at all.
        Some(_) => Kind::Placeholder,
        // No way to ask. The name is the only evidence left, and "Non-PnP" is
        // what Windows calls the monitor it invents for an EDID-less output.
        None if name.contains("Non-PnP") => Kind::Placeholder,
        None => Kind::Physical,
    }
}

/// Whether this process is in a Remote Desktop session.
pub(crate) fn is_remote_session() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_REMOTESESSION};
    // SAFETY: a pure query with no arguments to get wrong.
    unsafe { GetSystemMetrics(SM_REMOTESESSION) != 0 }
}

/// The active DisplayConfig paths, by source.
struct Targets {
    /// GDI device name (`\\.\DISPLAY1`) to monitor device path.
    paths: HashMap<String, String>,
    /// Whether every active path could be described. When one could not, a
    /// monitor missing from `paths` may still be driven by it.
    complete: bool,
}

/// Every active DisplayConfig path, keyed by GDI device name. `None` when
/// DisplayConfig cannot be queried here.
fn active_targets() -> Option<Targets> {
    // The topology can change between sizing and querying; that is what
    // `ERROR_INSUFFICIENT_BUFFER` means here, and asking again is the fix.
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0u32, 0u32);
        // SAFETY: both out-pointers are live locals.
        let sized = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        };
        if sized != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the arrays are exactly as long as the counts passed with them.
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if queried == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if queried != ERROR_SUCCESS {
            return None;
        }
        paths.truncate(path_count as usize);

        let mut map = HashMap::with_capacity(paths.len());
        let mut complete = true;
        for path in &paths {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            // SAFETY: the header's size and type match the struct it heads.
            if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
                complete = false;
                continue;
            }
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            // SAFETY: as above.
            if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
                complete = false;
                continue;
            }
            map.insert(
                wide_to_string(&source.viewGdiDeviceName),
                wide_to_string(&target.monitorDevicePath),
            );
        }
        return Some(Targets {
            paths: map,
            complete,
        });
    }
    None
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

fn describe(monitor: &Monitor) -> Result<Display> {
    let info = monitor_info(monitor)?;

    let resolution = Resolution::new(
        monitor.width().map_err(CaptureError::backend)?,
        monitor.height().map_err(CaptureError::backend)?,
    );
    if resolution.width == 0 || resolution.height == 0 {
        return Err(CaptureError::backend("monitor reported a zero-sized mode"));
    }

    Ok(Display {
        // Overwritten by `normalise_with`; a value that is obviously not a
        // real id makes it clear that nothing here decides the ordering.
        id: DisplayId(u8::MAX),
        name: name_of(monitor),
        resolution,
        position: (info.rcMonitor.left, info.rcMonitor.top),
        scale: scale_of(monitor),
        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        refresh_hz: monitor.refresh_rate().unwrap_or(0),
    })
}

fn monitor_info(monitor: &Monitor) -> Result<MONITORINFO> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        },
        szDevice: [0; 32],
    };

    // SAFETY: `info` is a correctly sized `MONITORINFOEXW` with `cbSize` set,
    // which is how the API distinguishes it from the shorter `MONITORINFO`,
    // and the handle comes straight from an enumeration that just produced it.
    let ok = unsafe {
        GetMonitorInfoW(
            HMONITOR(monitor.as_raw_hmonitor()),
            std::ptr::addr_of_mut!(info).cast(),
        )
    };
    if !ok.as_bool() {
        return Err(CaptureError::backend("GetMonitorInfoW failed"));
    }
    Ok(info.monitorInfo)
}

/// The name to show a person.
///
/// Three sources, best first: the monitor's own EDID name, the adapter's
/// description, then the device path. Never fails — a display with no name is
/// still a display the operator may want to share, and `\\.\DISPLAY2` is a
/// worse label than "DELL U2723QE" but a much better one than nothing.
fn name_of(monitor: &Monitor) -> String {
    monitor
        .name()
        .ok()
        // An EDID without a name descriptor comes back as an empty string,
        // not an error; that is a reason to keep looking, not a name.
        .filter(|name| !name.trim().is_empty())
        .or_else(|| monitor.device_string().ok())
        .or_else(|| monitor.device_name().ok())
        .unwrap_or_else(|| "Display".to_string())
}

/// The user's scaling factor, or 1.0 when Windows will not say.
///
/// Only the horizontal DPI is used. Windows reports both axes, but every
/// shipping display scales uniformly, and a client cannot do anything sensible
/// with two different numbers anyway.
fn scale_of(monitor: &Monitor) -> f32 {
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);

    // SAFETY: both out-pointers are to live locals, and the handle came from
    // an enumeration in this process.
    let queried = unsafe {
        GetDpiForMonitor(
            HMONITOR(monitor.as_raw_hmonitor()),
            MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        )
    };

    match queried {
        Ok(()) if dpi_x > 0 => dpi_x as f32 / BASELINE_DPI,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELL: &str =
        r"\\?\DISPLAY#DEL42B9#5&11933232&0&UID20738#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const VDD: &str =
        r"\\?\DISPLAY#MTT1337#5&2a45e7d&0&UID256#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
    const NO_EDID: &str =
        r"\\?\DISPLAY#Default_Monitor#4&1b2c3d&0&UID0#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";

    #[test]
    fn the_virtual_display_is_recognised_by_its_edid() {
        assert_eq!(classify(Some(2), Some(VDD), "VDD by MTT"), Kind::Virtual);
    }

    #[test]
    fn a_monitor_with_an_edid_is_physical_whatever_it_is_called() {
        assert_eq!(
            classify(Some(1), Some(DELL), "DELL AW2523HF"),
            Kind::Physical
        );
        // An EDID with no name descriptor still means something is plugged in.
        assert_eq!(
            classify(Some(1), Some(DELL), "Generic PnP Monitor"),
            Kind::Physical
        );
    }

    #[test]
    fn the_monitor_windows_invents_for_a_silent_output_is_a_placeholder() {
        assert_eq!(
            classify(Some(1), Some(NO_EDID), "Generic Non-PnP Monitor"),
            Kind::Placeholder
        );
    }

    #[test]
    fn a_monitor_no_active_path_drives_is_a_placeholder() {
        // The headless pseudo-desktop: listed, never composited.
        assert_eq!(classify(Some(0), None, r"\\.\DISPLAY1"), Kind::Placeholder);
        assert_eq!(
            classify(Some(2), None, "Generic PnP Monitor"),
            Kind::Placeholder
        );
    }

    #[test]
    fn without_displayconfig_only_the_non_pnp_name_is_evidence() {
        assert_eq!(
            classify(None, None, "Generic Non-PnP Monitor"),
            Kind::Placeholder
        );
        assert_eq!(classify(None, None, "DELL AW2523HF"), Kind::Physical);
    }

    #[test]
    fn this_machines_monitors_classify_without_panicking() {
        // Read-only: prints what this machine has, for eyeballing with
        // `--nocapture`. Asserts only that enumeration and classification
        // agree with each other.
        let Ok(entries) = inventory() else {
            eprintln!("no monitors here");
            return;
        };
        for entry in &entries {
            eprintln!(
                "{} {:?} active={} {}x{} primary={}",
                entry.gdi_name,
                entry.kind,
                entry.active,
                entry.display.resolution.width,
                entry.display.resolution.height,
                entry.display.primary,
            );
        }
        let offered = enumerate().map(|list| list.len()).unwrap_or(0);
        assert!(offered <= entries.len());
    }
}

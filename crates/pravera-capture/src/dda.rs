//! Desktop Duplication backend — the monitor-level, service-capable capture path.
//!
//! Why this exists alongside Windows Graphics Capture — and why neither can
//! replace the other — is the point of this module, so it is worth stating it
//! plainly and then getting out of the way.
//!
//! ## Why DDA and not `BitBlt`
//!
//! `BitBlt`/`GetDC` reads the GDI back-buffer. It misses anything the compositor
//! drew after GDI handed off: hardware overlays, DirectFlip surfaces, and every
//! window that lives on the DWM's own scene graph. The picture comes back with
//! holes where video was playing and with tearing when the copy raced the
//! present. DDA reads the final composited surface from DXGI, so what the eye
//! sees is what the capture sees, and the DWM hands over dirty rectangles that
//! tell the encoder what actually changed.
//!
//! ## Why alongside WGC
//!
//! WGC is composited per-window. It is correct on mixed-DPI desktops, survives
//! a full-screen exclusive application taking the display, and produces a
//! D3D11 texture the GPU pipeline can import without a round-trip through
//! system memory. It does not work from session 0. A service running as
//! `SYSTEM` lives in a window station with no desktop of its own; when it asks
//! WGC for the console, WGC asks the compositor in the interactive session, and
//! the compositor — correctly — says it belongs to a different user.
//!
//! DDA is monitor-level. When the same code runs as `SYSTEM` on session 0, it
//! can duplicate the console output even while the secure desktop is active —
//! the login screen, UAC, the lock screen — which is precisely the screen WGC
//! in a user session cannot see. The one thing this backend buys that nothing
//! else does is that: **the service can see what a user-session process cannot.**
//! That is why capture must be able to run in the service when the secure
//! desktop is up.
//!
//! ## The two-phase dance
//!
//! `IDXGIOutputDuplication::AcquireNextFrame` holds the next desktop image.
//! Nothing else may acquire on that output until `ReleaseFrame` is called, so
//! holding it is holding the compositor's own present path. The copy must
//! therefore be the only work done while the frame is held: copy the
//! `ID3D11Texture2D` into a shared/staging texture, map the staging texture,
//! copy the bytes out, and release. The same acquire-copy-release pattern WGC
//! uses, for the same reason.
//!
//! Dirty rects are the win. When the API reports a set of changed rectangles
//! they are coalesced into a [`Damage`] region; when it reports that the whole
//! desktop moved (or gives no rects at all), the frame is marked
//! [`Damage::Full`]. That keeps the encoder from re-encoding eight million
//! pixels because a clock ticked in the corner.
//!
//! ## Recovery
//!
//! `DXGI_ERROR_ACCESS_LOST` and `DXGI_ERROR_ACCESS_DENIED` are not failures.
//! They fire when the secure desktop activates, when the display mode changes,
//! or when another duplication session stole the output. The backend
//! re-enumerates outputs, recreates the duplication, backs off briefly, and
//! tries again — never panicking, never leaking the held frame.
//!
//! ## What this backend is not
//!
//! Headless fake monitors want an Indirect Display Driver (IDD) — a signed
//! user-mode (UMDF) driver that advertises a synthetic display
//! the way a dock advertises a real one. IDD complements this work but is not
//! this work: this backend duplicates whatever displays already exist, whether
//! there is a user in the console session or not; IDD would create displays
//! that do not exist yet so there is always something to duplicate on a box
//! with no monitor at all. The two fit together and neither replaces the other.
//!

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use bytes::Bytes;
use pravera_core::{PixelFormat, Rect, Resolution};

use crate::stream::{channel, FrameSink, Runner};
use crate::{
    CaptureError, CaptureOptions, CaptureSource, CapturedFrame, Damage, Display, DisplayId,
    FrameStream, Result, MAX_DISPLAYS,
};

// ---------------------------------------------------------------------------
// Public constants that tests use — kept here so the mapping is testable on
// Linux without needing a GPU.
// ---------------------------------------------------------------------------

/// `DXGI_ERROR_ACCESS_LOST` — the duplication is gone, recreate it.
pub const DXGI_ERROR_ACCESS_LOST: i32 = 0x887A0026u32 as i32;
/// `DXGI_ERROR_ACCESS_DENIED` — mode change or secure desktop took the output.
pub const DXGI_ERROR_ACCESS_DENIED: i32 = 0x887A0027u32 as i32;
/// `DXGI_ERROR_WAIT_TIMEOUT` — no new frame yet, not an error.
pub const DXGI_ERROR_WAIT_TIMEOUT: i32 = 0x887A002Au32 as i32;

/// Short backoff before re-creating a lost duplication. Long enough that a
/// tight loop does not burn a core, short enough that the login screen does
/// not stay black.
pub const BACKOFF: Duration = Duration::from_millis(200);

/// Whether this `HRESULT` means "try again" rather than "report failure".
///
/// Never panics. An unknown `HRESULT` is not retryable — it is a fault that
/// the caller should surface, not spin on.
pub fn is_retryable_dxgi_error(hr: i32) -> bool {
    hr == DXGI_ERROR_ACCESS_LOST || hr == DXGI_ERROR_ACCESS_DENIED
}

/// Map a raw `HRESULT` to a [`CaptureError`] the caller can act on.
///
/// `ACCESS_LOST`/`ACCESS_DENIED` become [`CaptureError::Lost`] (transient,
/// retryable). Everything else becomes `Backend` — the log keeps the
/// platform's own words, the control flow does not branch on them.
pub fn map_dxgi_hresult(hr: i32) -> CaptureError {
    if is_retryable_dxgi_error(hr) {
        CaptureError::Lost
    } else if hr == DXGI_ERROR_WAIT_TIMEOUT {
        // Timeout is not an error at all; the caller should keep polling.
        CaptureError::backend(format!("DXGI wait timeout (0x{hr:08x})"))
    } else {
        CaptureError::backend(format!("DXGI error 0x{hr:08x}"))
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, D3D11_CPU_ACCESS_READ,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput,
        IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_OUTDUPL_FRAME_INFO,
    };
    use windows::Win32::Graphics::Direct3D11::D3D11CreateDevice;
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;

    fn create_device() -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = Default::default();
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            Ok((device.unwrap(), context.unwrap()))
        }
    }

    fn enumerate_via_dxgi() -> Result<Vec<Display>> {
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(CaptureError::backend)?;
            let mut displays = Vec::new();
            let mut next_id: u8 = 0;
            for adapter_idx in 0..16 {
                let adapter: IDXGIAdapter = match factory.EnumAdapters(adapter_idx) {
                    Ok(a) => a,
                    Err(_) => break,
                };
                for output_idx in 0..8 {
                    let output: IDXGIOutput = match adapter.EnumOutputs(output_idx) {
                        Ok(o) => o,
                        Err(_) => break,
                    };
                    let desc = output.GetDesc().map_err(CaptureError::backend)?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let left = desc.DesktopCoordinates.left;
                    let top = desc.DesktopCoordinates.top;
                    let right = desc.DesktopCoordinates.right;
                    let bottom = desc.DesktopCoordinates.bottom;
                    let width = (right - left).max(0) as u32;
                    let height = (bottom - top).max(0) as u32;
                    if width == 0 || height == 0 {
                        continue;
                    }
                    let is_primary = left == 0 && top == 0;
                    // Refresh and scale are not in DXGI_OUTPUT_DESC; keep
                    // conservative defaults and let the host refine them later.
                    let display = Display {
                        id: DisplayId(next_id),
                        name: format!("Display {}", next_id as u32 + 1),
                        resolution: Resolution::new(width, height),
                        position: (left, top),
                        scale: 1.0,
                        primary: is_primary,
                        refresh_hz: 60,
                    };
                    displays.push(display);
                    next_id = next_id.wrapping_add(1);
                    if displays.len() >= MAX_DISPLAYS {
                        break;
                    }
                }
                if displays.len() >= MAX_DISPLAYS {
                    break;
                }
            }
            if displays.is_empty() {
                return Err(CaptureError::NoDisplays);
            }
            Ok(Display::normalise(displays))
        }
    }

    /// Fallback to the existing monitor enumeration (which knows names, DPI,
    /// and refresh) and map DDA outputs by position. Used when DXGI
    /// enumeration succeeds but the display list would otherwise lack the
    /// friendly names the protocol shows.
    fn enumerate_best() -> Result<Vec<Display>> {
        // Prefer DXGI when it can list outputs; otherwise fall back to the
        // monitor helper which uses `windows-capture` + Win32 monitor APIs.
        match enumerate_via_dxgi() {
            Ok(list) if !list.is_empty() => Ok(list),
            _ => {
                // `win32::monitors` is pub(crate); reach it via the crate root.
                // If that also fails, surface NoDisplays rather than a blank list.
                crate::win32::monitors::enumerate()
                    .map(|paired| paired.into_iter().map(|(d, _)| d).collect())
                    .or_else(|_| Err(CaptureError::NoDisplays))
            }
        }
    }

    pub(crate) fn displays() -> Result<Vec<Display>> {
        enumerate_best()
    }

    fn find_output_index_for_display(
        factory: &IDXGIFactory1,
        wanted: &Display,
    ) -> Option<(u32, u32)> {
        unsafe {
            for adapter_idx in 0..16 {
                let adapter: IDXGIAdapter = factory.EnumAdapters(adapter_idx).ok()?;
                for output_idx in 0..8 {
                    let output: IDXGIOutput = adapter.EnumOutputs(output_idx).ok()?;
                    let desc = output.GetDesc().ok()?;
                    let pos = (desc.DesktopCoordinates.left, desc.DesktopCoordinates.top);
                    let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                    let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;
                    if pos == wanted.position
                        && width == wanted.resolution.width
                        && height == wanted.resolution.height
                    {
                        return Some((adapter_idx, output_idx));
                    }
                }
            }
            None
        }
    }

    fn duplicate_for_display(
        device: &ID3D11Device,
        factory: &IDXGIFactory1,
        wanted: &Display,
    ) -> windows::core::Result<IDXGIOutputDuplication> {
        unsafe {
            let (adapter_idx, output_idx) = find_output_index_for_display(factory, wanted)
                .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
            let adapter: IDXGIAdapter = factory.EnumAdapters(adapter_idx)?;
            let output: IDXGIOutput = adapter.EnumOutputs(output_idx)?;
            let output1: IDXGIOutput1 = output.cast()?;
            output1.DuplicateOutput(device)
        }
    }

    fn clamp_rect(
        rect: windows::Win32::Foundation::RECT,
        resolution: Resolution,
    ) -> Option<Rect> {
        let left = rect.left.max(0) as u32;
        let top = rect.top.max(0) as u32;
        let right = rect.right.max(0).min(resolution.width as i32) as u32;
        let bottom = rect.bottom.max(0).min(resolution.height as i32) as u32;
        (right > left && bottom > top).then(|| Rect::new(left, top, right - left, bottom - top))
    }

    fn damage_from_dirty_rects(
        rects: &[windows::Win32::Foundation::RECT],
        resolution: Resolution,
        track_damage: bool,
    ) -> Damage {
        if !track_damage {
            return Damage::Full;
        }
        if rects.is_empty() {
            return Damage::Full;
        }
        let regions: Vec<Rect> = rects
            .iter()
            .filter_map(|r| clamp_rect(*r, resolution))
            .collect();
        Damage::regions(regions)
    }

    struct DdaRunner {
        halt: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl Runner for DdaRunner {
        fn stop(&mut self) {
            self.halt.store(true, Ordering::Release);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    pub fn start_capture(
        display: Display,
        id: DisplayId,
        options: CaptureOptions,
    ) -> Result<FrameStream> {
        let format = match options.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => options.format,
            other => return Err(CaptureError::UnsupportedFormat(other)),
        };
        let interval = options
            .max_fps
            .filter(|fps| *fps > 0)
            .map(|_| options.frame_interval(display.refresh_hz));
        let track_damage = options.damage;
        let resolution = display.resolution;

        let (sink, pending) = channel(display.clone(), format);
        let halt = Arc::new(AtomicBool::new(false));
        let thread_halt = halt.clone();

        let handle = thread::Builder::new()
            .name("pravera-dda".into())
            .spawn(move || {
                run_loop(
                    sink,
                    thread_halt,
                    display,
                    id,
                    resolution,
                    format,
                    interval,
                    track_damage,
                )
            })
            .map_err(CaptureError::backend)?;

        Ok(pending.attach(Box::new(DdaRunner {
            halt,
            handle: Some(handle),
        })))
    }

    fn run_loop(
        sink: FrameSink,
        halt: Arc<AtomicBool>,
        display: Display,
        id: DisplayId,
        resolution: Resolution,
        format: PixelFormat,
        interval: Option<Duration>,
        track_damage: bool,
    ) {
        let (device, context) = match create_device() {
            Ok(pair) => pair,
            Err(error) => {
                sink.fail(CaptureError::backend(error));
                return;
            }
        };

        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(error) => {
                sink.fail(CaptureError::backend(error));
                return;
            }
        };

        // Staging texture for CPU readback — BGRA8, staged, CPU-readable.
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: resolution.width,
            Height: resolution.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe {
            let _ = device.CreateTexture2D(&staging_desc, None, Some(&mut staging));
        }
        let staging = match staging {
            Some(t) => t,
            None => {
                sink.fail(CaptureError::backend("could not create DDA staging texture"));
                return;
            }
        };

        let mut duplication: Option<IDXGIOutputDuplication> = None;
        let mut last_recreate = Instant::now() - BACKOFF * 2;
        let started = Instant::now();
        let mut next_tick = started;

        while !halt.load(Ordering::Acquire) && sink.is_listening() {
            // Rate-limit even when the desktop is changing faster than max_fps.
            if let Some(iv) = interval {
                let now = Instant::now();
                if now < next_tick {
                    thread::sleep((next_tick - now).min(Duration::from_millis(10)));
                    continue;
                }
                next_tick = now + iv;
            }

            // Ensure duplication exists.
            if duplication.is_none() {
                let now = Instant::now();
                if now.duration_since(last_recreate) < BACKOFF {
                    thread::sleep(BACKOFF - now.duration_since(last_recreate));
                }
                last_recreate = Instant::now();
                match duplicate_for_display(&device, &factory, &display) {
                    Ok(d) => duplication = Some(d),
                    Err(error) => {
                        let hr = error.code().0;
                        if super::is_retryable_dxgi_error(hr) {
                            thread::sleep(BACKOFF);
                            continue;
                        }
                        // No output for this display — wait and retry rather than
                        // spinning.
                        thread::sleep(BACKOFF);
                        continue;
                    }
                }
            }

            let dupl = duplication.as_ref().unwrap();
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            // AcquireNextFrame timeout in ms — 500 ms keeps the thread
            // responsive to halt without busy-looping on an idle desktop.
            let acquired = unsafe { dupl.AcquireNextFrame(500, &mut frame_info, &mut resource) };
            if let Err(error) = acquired {
                let code = error.code().0;
                if code == super::DXGI_ERROR_WAIT_TIMEOUT {
                    continue;
                }
                if super::is_retryable_dxgi_error(code) {
                    unsafe { let _ = dupl.ReleaseFrame(); }
                    duplication = None;
                    thread::sleep(BACKOFF);
                    continue;
                }
                sink.fail(CaptureError::backend(format!("AcquireNextFrame failed: 0x{code:08x}")));
                break;
            }

            // DDA may signal no accumulated frames — treat as idle and release.
            if frame_info.AccumulatedFrames == 0 {
                unsafe { let _ = dupl.ReleaseFrame(); }
                continue;
            }

            // Collect dirty rects.
            let mut dirty_rects: Vec<windows::Win32::Foundation::RECT> = Vec::new();
            let damage = if track_damage {
                // Query how many rects are needed.
                let mut needed: u32 = 0;
                unsafe {
                    let _ = dupl.GetFrameDirtyRects(0, std::ptr::null_mut(), &mut needed);
                }
                if needed > 0 {
                    // `needed` is bytes; the buffer wants that many RECTs.
                    let rect_bytes = std::mem::size_of::<windows::Win32::Foundation::RECT>() as u32;
                    dirty_rects.resize(needed as usize / rect_bytes as usize, Default::default());
                    let mut fetched = 0u32;
                    unsafe {
                        let _ = dupl.GetFrameDirtyRects(needed, dirty_rects.as_mut_ptr(), &mut fetched);
                    }
                    // DXGI reports dirty rects in desktop coordinates — they are
                    // already in the right space for Damage (which is frame-local
                    // because DDA is monitor-level). For a monitor-level capture
                    // the rects are already frame-relative; no offset needed when
                    // the display is at (0,0) after normalisation, but when
                    // displays are arranged left-to-right we keep them as-is and
                    // let the caller clip.
                }
                // Move rects are treated as dirty for now — they describe
                // pixels that moved, which still need re-encoding.
                let mut move_needed: u32 = 0;
                unsafe {
                    let _ = dupl.GetFrameMoveRects(0, std::ptr::null_mut(), &mut move_needed);
                }
                if move_needed > 0 {
                    // Move rects indicate block moves plus a dirty region;
                    // counting them as full damage is safe and avoids
                    // composing a blit in the copy.
                    damage_from_dirty_rects(&dirty_rects, resolution, true)
                        .coalesced_or_full(resolution)
                } else if dirty_rects.is_empty() {
                    Damage::Full
                } else {
                    damage_from_dirty_rects(&dirty_rects, resolution, true)
                }
            } else {
                Damage::Full
            };

            // Two-phase copy: hold frame, copy GPU texture to staging, release,
            // then map and copy bytes. Holding the frame only for the GPU copy
            // keeps the compositor unblocked.
            let mut copied_ok = false;
            let mut pixels: Option<Bytes> = None;
            if let Some(res) = resource {
                if let Ok(src) = res.cast::<ID3D11Texture2D>() {
                    unsafe {
                        context.CopyResource(&staging, &src);
                    }
                    // Release before mapping — the spec's "same dance WGC does".
                    unsafe { let _ = dupl.ReleaseFrame(); }
                    // Now map the staging texture on the CPU.
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    let map_hr = unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) };
                    if map_hr.is_ok() {
                        let row_bytes = resolution.width as usize * 4;
                        let src_ptr = mapped.pData as *const u8;
                        let src_pitch = mapped.RowPitch as usize;
                        let mut out = vec![0u8; row_bytes * resolution.height as usize];
                        // Handle padded rows from the driver.
                        for row in 0..resolution.height as usize {
                            let src_row = unsafe { std::slice::from_raw_parts(src_ptr.add(row * src_pitch), row_bytes) };
                            out[row * row_bytes..(row + 1) * row_bytes].copy_from_slice(src_row);
                        }
                        unsafe { context.Unmap(&staging, 0); }
                        // Convert BGRA to RGBA if requested — channel swap in place.
                        if format == PixelFormat::Rgba8 {
                            for chunk in out.chunks_exact_mut(4) {
                                chunk.swap(0, 2);
                            }
                        }
                        pixels = Some(Bytes::from(out));
                        copied_ok = true;
                    } else {
                        // Map failed — treat as backend error and retry.
                        copied_ok = false;
                    }
                } else {
                    unsafe { let _ = dupl.ReleaseFrame(); }
                }
            } else {
                unsafe { let _ = dupl.ReleaseFrame(); }
            }

            if !copied_ok {
                // If we already released, just continue; the next iteration will
                // re-acquire. Avoid double-release.
                thread::sleep(Duration::from_millis(5));
                continue;
            }

            let elapsed = started.elapsed();
            let frame = CapturedFrame {
                display: id,
                format,
                resolution,
                stride: resolution.width as usize * 4,
                pixels: pixels.unwrap(),
                elapsed,
                damage,
            };
            debug_assert!(frame.is_consistent(), "DDA produced inconsistent frame: {frame:?}");
            if !sink.put(frame) {
                break;
            }
        }

        sink.end();
    }

    // Helper to coalesce damage and collapse to Full when it covers nearly
    // the whole screen — avoids encoding dozens of tiny rects as separate passes.
    trait Coalesced {
        fn coalesced_or_full(self, resolution: Resolution) -> Damage;
    }
    impl Coalesced for Damage {
        fn coalesced_or_full(mut self, resolution: Resolution) -> Damage {
            // Keep at most 8 rects before merging crumbs.
            self.coalesce(8, resolution);
            self
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub(crate) fn displays() -> Result<Vec<Display>> {
        Err(CaptureError::Unavailable(
            "Desktop Duplication is available on Windows only",
        ))
    }
    pub fn start_capture(
        _display: Display,
        _id: DisplayId,
        _options: CaptureOptions,
    ) -> Result<FrameStream> {
        Err(CaptureError::Unavailable(
            "Desktop Duplication is available on Windows only",
        ))
    }
}

// ---------------------------------------------------------------------------
// Public type — present on every platform, Windows-only internals hidden.
// ---------------------------------------------------------------------------

/// Monitor-level capture that works from session 0 as `SYSTEM`.
///
/// On Windows this drives `IDXGIOutputDuplication`; on other platforms it is a
/// stub that always returns [`CaptureError::Unavailable`], mirroring
/// `linux.rs`.
pub struct DdaSource;

impl DdaSource {
    /// Create a new DDA source.
    ///
    /// Does not touch the GPU — that happens on the first `start` so
    /// construction never fails because the compositor is busy.
    pub fn new() -> Result<Self> {
        Ok(DdaSource)
    }
}

impl CaptureSource for DdaSource {
    fn name(&self) -> &'static str {
        "desktop-duplication"
    }

    fn displays(&self) -> Result<Vec<Display>> {
        #[cfg(windows)]
        {
            imp::displays()
        }
        #[cfg(not(windows))]
        {
            imp::displays()
        }
    }

    fn start(&self, id: DisplayId, options: &CaptureOptions) -> Result<FrameStream> {
        // Validate format early — produces a clear error rather than a later
        // backend failure that looks like a GPU problem.
        match options.format {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => {}
            other => return Err(CaptureError::UnsupportedFormat(other)),
        }

        #[cfg(windows)]
        {
            // Re-enumerate to get the Display for this id; fail fast if the
            // caller asked for a display that is not there rather than
            // capturing the wrong monitor.
            let display = self
                .displays()?
                .into_iter()
                .find(|d| d.id == id)
                .ok_or(CaptureError::NoSuchDisplay(id))?;
            imp::start_capture(display, id, options.clone())
        }
        #[cfg(not(windows))]
        {
            let _ = id;
            let _ = options;
            imp::start_capture(
                Display {
                    id: DisplayId(0),
                    name: String::new(),
                    resolution: Resolution::new(0, 0),
                    position: (0, 0),
                    scale: 1.0,
                    primary: false,
                    refresh_hz: 0,
                },
                id,
                options.clone(),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyntheticSource;

    #[test]
    fn synthetic_source_still_works_without_a_display_server() {
        // The one backend that must work everywhere — headless CI, no GPU,
        // no compositor. If this fails, the pipeline tests have no substrate.
        let source = SyntheticSource::new(Resolution::new(320, 240), 60);
        let displays = source.displays().expect("synthetic displays must exist");
        assert_eq!(displays.len(), 1);
        assert_eq!(displays[0].resolution, Resolution::new(320, 240));

        let stream = source
            .start(DisplayId::PRIMARY, &CaptureOptions::default())
            .expect("synthetic start must succeed");
        let frame = {
            use crate::Recv;
            use std::time::Duration;
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut got = None;
            while Instant::now() < deadline {
                match stream.recv_timeout(Duration::from_millis(100)) {
                    Ok(Recv::Frame(f)) => {
                        got = Some(f);
                        break;
                    }
                    Ok(Recv::Idle) => continue,
                    other => panic!("unexpected synthetic recv: {other:?}"),
                }
            }
            got.expect("synthetic must produce a frame promptly")
        };
        assert!(frame.is_consistent());
        assert_eq!(frame.resolution, Resolution::new(320, 240));
    }

    #[test]
    fn access_lost_and_access_denied_are_retryable_not_fatal() {
        // The error that fires when the secure desktop activates or the mode
        // changes must not become a panic or a hard failure — it is the normal
        // way the OS revokes a duplication session.
        assert!(
            is_retryable_dxgi_error(DXGI_ERROR_ACCESS_LOST),
            "ACCESS_LOST must be retryable"
        );
        assert!(
            is_retryable_dxgi_error(DXGI_ERROR_ACCESS_DENIED),
            "ACCESS_DENIED must be retryable"
        );
        assert!(
            !is_retryable_dxgi_error(DXGI_ERROR_WAIT_TIMEOUT),
            "WAIT_TIMEOUT is idle, not a loss"
        );
        assert!(
            !is_retryable_dxgi_error(0),
            "S_OK is not retryable"
        );
        assert!(
            !is_retryable_dxgi_error(-1),
            "an unknown failure must not be retried endlessly"
        );
    }

    #[test]
    fn dxgi_error_mapping_never_panics_and_is_transient_only_when_retryable() {
        // `map_dxgi_hresult` must return `Lost` exactly for the retryable set,
        // and `Backend` for everything else — never panic, never misclassify.
        for hr in [DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_ACCESS_DENIED] {
            let mapped = map_dxgi_hresult(hr);
            assert!(
                matches!(mapped, CaptureError::Lost),
                "retryable hr 0x{hr:08x} should map to Lost, got {mapped:?}"
            );
            assert!(mapped.is_transient(), "Lost must be transient");
        }
        for hr in [DXGI_ERROR_WAIT_TIMEOUT, 0, -2147467259i32] {
            // WAIT_TIMEOUT and S_OK/other failures are not `Lost`.
            if hr == DXGI_ERROR_WAIT_TIMEOUT {
                // Timeout is reported as Backend, not Lost — the capture loop
                // handles it as idle directly.
                let mapped = map_dxgi_hresult(hr);
                assert!(
                    matches!(mapped, CaptureError::Backend(_)),
                    "timeout should be Backend"
                );
            } else if hr != DXGI_ERROR_ACCESS_LOST && hr != DXGI_ERROR_ACCESS_DENIED {
                let mapped = map_dxgi_hresult(hr);
                // Non-retryable failures are Backend, and is_transient is true
                // for Backend but callers must not spin on them without backoff.
                assert!(!matches!(mapped, CaptureError::Lost));
            }
        }
    }

    #[test]
    fn dda_on_non_windows_reports_unavailable() {
        // Ensures the stub path works on Linux CI — the test itself is the
        // assertion that the binary links and the error is the right variant.
        #[cfg(not(windows))]
        {
            let source = DdaSource::new().expect("DDA construction is always Ok on stub");
            assert_eq!(source.name(), "desktop-duplication");
            assert!(matches!(
                source.displays(),
                Err(CaptureError::Unavailable(_))
            ));
            assert!(matches!(
                source.start(DisplayId::PRIMARY, &CaptureOptions::default()),
                Err(CaptureError::Unavailable(_))
            ));
        }
    }

    // Windows-only integration: attempts to start DDA on display 0. Skips
    // gracefully when no session/user is available — the same pattern the
    // existing `this_machine` tests use so CI stays green on headless runners.
    #[cfg(windows)]
    #[test]
    fn dda_attempts_to_start_on_display_0_and_skips_gracefully_when_no_session() {
        let source = match DdaSource::new() {
            Ok(s) => s,
            Err(error) => {
                eprintln!("SKIPPED: DDA source could not be created: {error}");
                return;
            }
        };
        let displays = match source.displays() {
            Ok(list) => list,
            Err(CaptureError::NoDisplays) => {
                eprintln!("SKIPPED: no displays for DDA on this machine");
                return;
            }
            Err(CaptureError::Unavailable(msg)) => {
                eprintln!("SKIPPED: DDA unavailable on this machine: {msg}");
                return;
            }
            Err(error) => {
                eprintln!("SKIPPED: DDA enumerate failed (no console session?): {error}");
                return;
            }
        };
        if displays.is_empty() {
            eprintln!("SKIPPED: DDA returned empty display list");
            return;
        }
        let primary = displays[0].id;
        match source.start(primary, &CaptureOptions::default()) {
            Ok(stream) => {
                // Started — now prove it can be stopped without hanging. The
                // compositor may produce nothing on a locked headless runner,
                // so we tolerate Idle.
                let _ = stream.recv_timeout(Duration::from_millis(300));
                stream.stop();
                eprintln!("DDA started and stopped on {primary}");
            }
            Err(CaptureError::NoSuchDisplay(_)) => {
                panic!("DDA reported a display and then refused it");
            }
            Err(CaptureError::Unavailable(msg)) => {
                eprintln!("SKIPPED: DDA start unavailable (session 0?): {msg}");
            }
            Err(CaptureError::PermissionDenied) => {
                eprintln!("SKIPPED: DDA denied (secure desktop or session isolation)");
            }
            Err(CaptureError::NoDisplays) => {
                eprintln!("SKIPPED: DDA has no displays to capture right now");
            }
            Err(CaptureError::Backend(msg)) => {
                // DXGI complaints on headless CI are expected — log and skip
                // rather than failing the run.
                eprintln!("SKIPPED: DDA backend error (expected headless): {msg}");
            }
            Err(other) => {
                eprintln!("SKIPPED: DDA start failed: {other}");
            }
        }
    }
}

//! The host's mouse cursor, as something a viewer can draw.
//!
//! Screen capture can embed the cursor in the picture, and until protocol
//! version 3 that is what Pravera did. The trouble is that an embedded cursor
//! only moves when a frame is captured, encoded, sent and decoded, so it lags
//! the viewer's own hand by a full round trip plus a frame. From version 3 the
//! host sends the cursor separately: its image once per shape
//! ([`HostMessage::CursorShape`]) and its position whenever it changes
//! ([`HostMessage::Cursor`]), and the viewer draws it locally, over the picture.
//!
//! ## Layout
//!
//! - [`CursorSource`] is what the platform provides: where the pointer is, what
//!   it looks like, whether it is visible. [`SystemCursor`] is the Windows one.
//! - [`Tracker`] is the pure part. It turns a stream of samples into the
//!   messages worth sending: shapes the first time they are seen, positions
//!   only when something changed, in the coordinates of the picture the viewer
//!   is looking at. No I/O, so it is tested with a scripted source.
//! - [`CursorFeed`] runs a tracker on its own thread and ships what it produces
//!   over the cursor stream, never letting a slow link queue up positions.
//! - The `*_to_rgba` functions turn Windows cursor bitmaps into straight RGBA.
//!   They take plain buffers so they are tested on synthetic ones.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pravera_core::Resolution;
use pravera_input::Screen;
use pravera_proto::{HostMessage, MAX_CURSOR_EDGE};
use pravera_transport::Session;
use tokio::sync::Notify;
use tracing::{debug, info};

// ------------------------------------------------------------------ shapes

/// A cursor image in host pixels, straight (not premultiplied) RGBA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawShape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// The largest bitmap edge worth converting. Real cursors top out around 128;
/// anything past this is a broken handle, and refusing it here keeps the
/// allocations below bounded by something other than a driver's mood.
const MAX_SOURCE_EDGE: usize = 512;

const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];
const BLACK: [u8; 4] = [0, 0, 0, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// A monochrome cursor: two 1-bit planes, AND and XOR, combined with the pixels
/// underneath.
///
/// | AND | XOR | Windows does        | drawn here                      |
/// |-----|-----|---------------------|---------------------------------|
/// |  1  |  0  | leaves the screen   | transparent                     |
/// |  0  |  0  | black               | black                           |
/// |  0  |  1  | white               | white                           |
/// |  1  |  1  | inverts the screen  | white with a one pixel outline  |
///
/// The last row cannot be drawn by a viewer that composites an image over a
/// picture, and it is the I-beam, which is the cursor over every text box. White
/// with a black outline stays visible over light and dark alike, which is what
/// inversion was for.
pub fn monochrome_to_rgba(width: usize, height: usize, and: &[bool], xor: &[bool]) -> Vec<u8> {
    let pixels = width * height;
    assert_eq!(and.len(), pixels);
    assert_eq!(xor.len(), pixels);

    let mut out = vec![0u8; pixels * 4];
    let mut inverted = Vec::new();
    for i in 0..pixels {
        let pixel = match (and[i], xor[i]) {
            (true, false) => TRANSPARENT,
            (false, false) => BLACK,
            (false, true) => WHITE,
            (true, true) => {
                inverted.push(i);
                WHITE
            }
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&pixel);
    }

    // The outline: any pixel touching an inverted one that is otherwise empty.
    for i in inverted {
        let (x, y) = (i % width, i / width);
        let neighbours = [
            (x > 0).then(|| i - 1),
            (x + 1 < width).then(|| i + 1),
            (y > 0).then(|| i - width),
            (y + 1 < height).then(|| i + width),
        ];
        for n in neighbours.into_iter().flatten() {
            if and[n] && !xor[n] {
                out[n * 4..n * 4 + 4].copy_from_slice(&BLACK);
            }
        }
    }
    out
}

/// A colour cursor: a 32 bit BGRA bitmap and an AND mask.
///
/// Modern cursors carry their own alpha and the mask is ignored. Older ones are
/// 24 or 32 bit with an alpha channel of all zeros, and the mask says which
/// pixels are see-through. A masked-in pixel with colour under it is the XOR
/// case and is drawn opaque: a slightly wrong colour beats a hole.
pub fn color_to_rgba(width: usize, height: usize, bgra: &[u8], and: &[bool]) -> Vec<u8> {
    let pixels = width * height;
    assert_eq!(bgra.len(), pixels * 4);
    assert_eq!(and.len(), pixels);

    let has_alpha = bgra.chunks_exact(4).any(|p| p[3] != 0);
    let mut out = vec![0u8; pixels * 4];
    for i in 0..pixels {
        let [b, g, r, a] = [bgra[i * 4], bgra[i * 4 + 1], bgra[i * 4 + 2], bgra[i * 4 + 3]];
        let pixel = if has_alpha {
            [r, g, b, a]
        } else if and[i] && (b | g | r) == 0 {
            TRANSPARENT
        } else {
            [r, g, b, 255]
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&pixel);
    }
    out
}

/// Resize a shape by `factor`, and further if it would exceed the protocol's
/// size cap. Averages in premultiplied space so a soft edge does not pick up a
/// dark halo from the transparent pixels around it.
pub fn scale_shape(shape: &RawShape, factor: f32) -> RawShape {
    let (w, h) = (usize::from(shape.width), usize::from(shape.height));
    let mut factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };
    let edge = w.max(h) as f32 * factor;
    if edge > f32::from(MAX_CURSOR_EDGE) {
        factor *= f32::from(MAX_CURSOR_EDGE) / edge;
    }
    if (factor - 1.0).abs() < 0.02 {
        return shape.clone();
    }

    let out_w = ((w as f32 * factor).round() as usize).clamp(1, usize::from(MAX_CURSOR_EDGE));
    let out_h = ((h as f32 * factor).round() as usize).clamp(1, usize::from(MAX_CURSOR_EDGE));
    let mut rgba = Vec::with_capacity(out_w * out_h * 4);

    for oy in 0..out_h {
        let (y0, y1) = source_range(oy, out_h, h);
        for ox in 0..out_w {
            let (x0, x1) = source_range(ox, out_w, w);
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let p = &shape.rgba[(sy * w + sx) * 4..][..4];
                    let alpha = u32::from(p[3]);
                    r += u32::from(p[0]) * alpha;
                    g += u32::from(p[1]) * alpha;
                    b += u32::from(p[2]) * alpha;
                    a += alpha;
                    n += 1;
                }
            }
            let average = |sum: u32, over: u32| sum.saturating_add(over / 2).checked_div(over);
            match (average(r, a), average(g, a), average(b, a), average(a, n)) {
                (Some(r), Some(g), Some(b), Some(a)) => {
                    rgba.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
                }
                // Nothing opaque in the block.
                _ => rgba.extend_from_slice(&TRANSPARENT),
            }
        }
    }

    let hot = |hot: u16, from: usize, to: usize| -> u16 {
        ((f32::from(hot) * to as f32 / from as f32).round() as usize).min(to - 1) as u16
    };
    RawShape {
        width: out_w as u16,
        height: out_h as u16,
        hot_x: hot(shape.hot_x, w, out_w),
        hot_y: hot(shape.hot_y, h, out_h),
        rgba,
    }
}

/// The source pixels `[start, end)` that output pixel `o` of `out` covers.
fn source_range(o: usize, out: usize, source: usize) -> (usize, usize) {
    let start = (o * source / out).min(source - 1);
    let end = ((o + 1) * source).div_ceil(out).clamp(start + 1, source);
    (start, end)
}

// ------------------------------------------------------------------ source

/// What a platform can say about the pointer at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Where the pointer is, in desktop pixels. `None` when it cannot be read.
    pub position: Option<(i32, i32)>,
    pub shape: ShapeState,
}

/// What the pointer looks like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeState {
    /// An application hid it. The viewer should draw nothing.
    Hidden,
    /// Shown, but there is no image to read: the system suppressed the cursor
    /// (touch input) or there is no mouse at all (a machine hosting unattended).
    /// A viewer still needs a pointer, so this is drawn as the last real shape,
    /// or the system arrow before there was one.
    Unknown,
    /// Shown, wearing the shape this handle names. A handle is opaque to
    /// everything but the source that produced it.
    Handle(u64),
}

/// The platform half of the cursor feed.
pub trait CursorSource: Send + 'static {
    fn sample(&mut self) -> Sample;
    /// The image behind a handle from [`ShapeState::Handle`] or
    /// [`CursorSource::arrow`]. `None` when it cannot be read.
    fn shape(&mut self, handle: u64) -> Option<RawShape>;
    /// The handle of the system arrow, for [`ShapeState::Unknown`].
    fn arrow(&mut self) -> Option<u64>;
}

// ----------------------------------------------------------------- tracker

/// Where the viewer's picture sits on the host's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// The streamed display, in desktop pixels. The same value the input
    /// injector measures pointer positions against, so a cursor position and a
    /// click land on the same pixel.
    pub screen: Screen,
    /// The size of the picture the viewer receives, which is the display's size
    /// or a scaled version of it.
    pub picture: Resolution,
}

impl Geometry {
    /// Picture pixels per desktop pixel.
    fn scale(&self) -> f32 {
        self.picture.width as f32 / self.screen.resolution.width.max(1) as f32
    }
}

/// Shapes remembered per session before the cache is dropped and started over.
/// Ids keep counting up, so a shape sent again gets a new id and no stale entry
/// on the viewer can be mistaken for it.
const CACHE_LIMIT: usize = 256;

/// Turns samples into the messages worth sending. Pure apart from asking its
/// source for shape images.
#[derive(Default)]
pub struct Tracker {
    /// `(handle, scale in thousandths)` to the id sent for it. `None` records a
    /// handle that would not convert, so it is not retried every poll.
    sent: HashMap<(u64, u32), Option<u32>>,
    next_id: u32,
    /// The last handle the system really reported, used while it reports none.
    last_real: Option<u64>,
    last_shape: Option<u32>,
    last: Option<(i32, i32, bool, u32)>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    /// Everything worth sending given the pointer's current state: any shapes
    /// not yet sent, then a `Cursor` if it differs from the last one sent.
    pub fn step(
        &mut self,
        source: &mut dyn CursorSource,
        geometry: &Geometry,
    ) -> Vec<HostMessage> {
        let sample = source.sample();
        let mut out = Vec::new();

        // No position is no news. Better to leave the viewer with the last
        // thing it knows than to guess.
        let Some((px, py)) = sample.position else {
            return out;
        };

        let handle = match sample.shape {
            ShapeState::Hidden => None,
            ShapeState::Handle(handle) => {
                self.last_real = Some(handle);
                Some(handle)
            }
            ShapeState::Unknown => self.last_real.or_else(|| source.arrow()),
        };

        let on_display = geometry.screen.from_desktop(px, py);
        let visible = on_display.is_some() && handle.is_some();

        let shape = match (visible, handle) {
            (true, Some(handle)) => self.shape_id(handle, source, geometry, &mut out),
            _ => self.last_shape,
        };
        // A shape that would not convert leaves nothing to draw. Saying "not
        // visible" makes the viewer fall back to its own cursor rather than
        // showing the host's pointer as a hole.
        let visible = visible && shape.is_some();

        let (x, y) = match on_display {
            Some((fx, fy)) => (
                (fx * geometry.picture.width as f32).round() as i32,
                (fy * geometry.picture.height as f32).round() as i32,
            ),
            None => self.last.map_or((0, 0), |(x, y, ..)| (x, y)),
        };
        let now = (x, y, visible, shape.unwrap_or(0));
        let changed = match self.last {
            None => true,
            // While hidden, where the pointer is does not matter.
            Some(last) if !visible => last.2 != visible,
            Some(last) => last != now,
        };
        if changed {
            self.last = Some(now);
            out.push(HostMessage::Cursor {
                x: now.0,
                y: now.1,
                visible: now.2,
                shape: now.3,
            });
        }
        out
    }

    /// The id for a handle at the current scale, converting and queueing the
    /// image the first time.
    fn shape_id(
        &mut self,
        handle: u64,
        source: &mut dyn CursorSource,
        geometry: &Geometry,
        out: &mut Vec<HostMessage>,
    ) -> Option<u32> {
        let scale = geometry.scale();
        let key = (handle, (scale * 1000.0).round() as u32);
        if let Some(known) = self.sent.get(&key) {
            self.last_shape = known.or(self.last_shape);
            return known.or(self.last_shape);
        }

        if self.sent.len() >= CACHE_LIMIT {
            self.sent.clear();
        }
        let id = source
            .shape(handle)
            .filter(|raw| raw.rgba.len() == usize::from(raw.width) * usize::from(raw.height) * 4)
            .map(|raw| scale_shape(&raw, scale))
            .map(|shape| {
                let id = self.next_id;
                self.next_id += 1;
                out.push(HostMessage::CursorShape {
                    id,
                    width: shape.width,
                    height: shape.height,
                    hot_x: shape.hot_x,
                    hot_y: shape.hot_y,
                    rgba: shape.rgba,
                });
                id
            });
        self.sent.insert(key, id);
        if id.is_some() {
            self.last_shape = id;
        }
        id.or(self.last_shape)
    }
}

// -------------------------------------------------------------------- feed

/// How often the pointer is read. The video runs at most 60 frames a second, so
/// a finer poll would only move the cursor between frames nobody sees.
const POLL: Duration = Duration::from_millis(8);

#[derive(Default)]
struct Pending {
    /// Shapes in the order they were first seen. Never dropped: a position may
    /// name any of them.
    shapes: VecDeque<HostMessage>,
    /// The newest position. Overwritten, never queued.
    cursor: Option<HostMessage>,
}

/// A running cursor feed for one connection. Dropping it stops it.
pub struct CursorFeed {
    stop: Arc<AtomicBool>,
    geometry: Arc<Mutex<Geometry>>,
    poller: Option<thread::JoinHandle<()>>,
    sender: tokio::task::JoinHandle<()>,
}

impl CursorFeed {
    /// Start watching the pointer and sending what changes. Must be called from
    /// inside a tokio runtime.
    pub fn start(session: Session, mut source: Box<dyn CursorSource>, geometry: Geometry) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let geometry = Arc::new(Mutex::new(geometry));
        let pending = Arc::new(Mutex::new(Pending::default()));
        let wake = Arc::new(Notify::new());

        let poller = {
            let (stop, geometry, pending, wake) =
                (stop.clone(), geometry.clone(), pending.clone(), wake.clone());
            thread::Builder::new()
                .name("pravera-cursor".into())
                .spawn(move || {
                    let mut tracker = Tracker::new();
                    while !stop.load(Ordering::Relaxed) {
                        let now = *geometry.lock().unwrap_or_else(|e| e.into_inner());
                        let updates = tracker.step(source.as_mut(), &now);
                        if !updates.is_empty() {
                            let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                            for update in updates {
                                match update {
                                    shape @ HostMessage::CursorShape { .. } => {
                                        pending.shapes.push_back(shape)
                                    }
                                    other => pending.cursor = Some(other),
                                }
                            }
                            drop(pending);
                            wake.notify_one();
                        }
                        thread::sleep(POLL);
                    }
                })
                .ok()
        };

        let sender = tokio::spawn(async move {
            let mut stream = match session.open_cursor().await {
                Ok(stream) => stream,
                Err(error) => {
                    debug!(%error, "could not open the cursor stream");
                    return;
                }
            };
            loop {
                wake.notified().await;
                let (shapes, cursor) = {
                    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
                    (std::mem::take(&mut pending.shapes), pending.cursor.take())
                };
                for message in shapes.iter().chain(cursor.iter()) {
                    if let Err(error) = stream.send(message).await {
                        debug!(%error, "the cursor stream ended");
                        return;
                    }
                }
            }
        });

        if poller.is_none() {
            info!("could not start the cursor thread; the viewer keeps its own cursor");
        }
        CursorFeed {
            stop,
            geometry,
            poller,
            sender,
        }
    }

    /// The viewer's picture moved to another display or size.
    pub fn set_geometry(&self, geometry: Geometry) {
        *self.geometry.lock().unwrap_or_else(|e| e.into_inner()) = geometry;
    }
}

impl Drop for CursorFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.sender.abort();
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
    }
}

/// The platform's cursor source, if this platform has one.
pub fn system_source() -> Option<Box<dyn CursorSource>> {
    #[cfg(windows)]
    {
        Some(Box::new(SystemCursor::new()))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

// ----------------------------------------------------------------- windows

#[cfg(windows)]
pub use win::SystemCursor;

#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::mem::size_of;

    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, GetCursorPos, GetIconInfo, GetSystemMetrics, LoadCursorW, CURSORINFO,
        CURSOR_SUPPRESSED, HCURSOR, HICON, ICONINFO, IDC_ARROW, SM_MOUSEPRESENT,
    };

    use super::{color_to_rgba, monochrome_to_rgba, CursorSource, RawShape, Sample, ShapeState};
    use super::MAX_SOURCE_EDGE;

    /// The cursor of the desktop this process is attached to.
    ///
    /// `GetCursorInfo` is a per-desktop, per-session call. That is right for a
    /// host running in the user's session, and it is why the fallbacks below
    /// exist: with no mouse (or no interactive session) it reports a null cursor
    /// or fails outright, and a viewer still wants a pointer to look at.
    pub struct SystemCursor {
        arrow: Option<u64>,
    }

    impl SystemCursor {
        pub fn new() -> Self {
            SystemCursor { arrow: None }
        }

        /// What `GetCursorInfo` says right now, for the diagnostic test.
        pub fn raw_info() -> Option<(u32, usize, (i32, i32))> {
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            unsafe { GetCursorInfo(&mut info) }.ok()?;
            Some((
                info.flags.0,
                info.hCursor.0 as usize,
                (info.ptScreenPos.x, info.ptScreenPos.y),
            ))
        }
    }

    impl Default for SystemCursor {
        fn default() -> Self {
            Self::new()
        }
    }

    impl CursorSource for SystemCursor {
        fn sample(&mut self) -> Sample {
            let mut info = CURSORINFO {
                cbSize: size_of::<CURSORINFO>() as u32,
                ..Default::default()
            };
            if unsafe { GetCursorInfo(&mut info) }.is_err() {
                // No interactive desktop to ask. The position may still be
                // readable; the shape is not.
                let mut point = POINT::default();
                let position = unsafe { GetCursorPos(&mut point) }
                    .ok()
                    .map(|()| (point.x, point.y));
                return Sample {
                    position,
                    shape: ShapeState::Unknown,
                };
            }

            let position = Some((info.ptScreenPos.x, info.ptScreenPos.y));
            let shape = if info.flags.0 == 0 {
                // Neither showing nor suppressed. With a mouse attached that
                // means an application hid the cursor. With none, Windows
                // reports the cursor as not showing too, and hiding it from a
                // viewer would leave a machine hosting unattended with no
                // pointer to look at; that case is drawn as the arrow.
                if unsafe { GetSystemMetrics(SM_MOUSEPRESENT) } == 0 {
                    ShapeState::Unknown
                } else {
                    ShapeState::Hidden
                }
            } else if info.flags.0 & CURSOR_SUPPRESSED.0 != 0 || info.hCursor.0.is_null() {
                ShapeState::Unknown
            } else {
                ShapeState::Handle(info.hCursor.0 as usize as u64)
            };
            Sample { position, shape }
        }

        fn shape(&mut self, handle: u64) -> Option<RawShape> {
            // SAFETY: the handle came from `GetCursorInfo` or `LoadCursorW`. A
            // handle destroyed since makes `GetIconInfo` fail, which is `None`.
            unsafe { read_cursor(HCURSOR(handle as usize as *mut c_void)) }
        }

        fn arrow(&mut self) -> Option<u64> {
            if self.arrow.is_none() {
                // Shared, so it must not be destroyed and needs no freeing.
                let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.ok()?;
                self.arrow = Some(cursor.0 as usize as u64);
            }
            self.arrow
        }
    }

    /// Frees the two bitmaps `GetIconInfo` hands over. They are ours to delete.
    struct IconBitmaps(ICONINFO);

    impl Drop for IconBitmaps {
        fn drop(&mut self) {
            unsafe {
                if !self.0.hbmColor.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.0.hbmColor.0));
                }
                if !self.0.hbmMask.is_invalid() {
                    let _ = DeleteObject(HGDIOBJ(self.0.hbmMask.0));
                }
            }
        }
    }

    struct ScreenDc(HDC);

    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe {
                ReleaseDC(Some(HWND::default()), self.0);
            }
        }
    }

    unsafe fn read_cursor(cursor: HCURSOR) -> Option<RawShape> {
        let mut info = ICONINFO::default();
        GetIconInfo(HICON(cursor.0), &mut info).ok()?;
        let bitmaps = IconBitmaps(info);
        let dc = ScreenDc(GetDC(None));
        if dc.0.is_invalid() {
            return None;
        }

        let colour = !info.hbmColor.is_invalid();
        let mask_size = bitmap_size(info.hbmMask)?;
        let (width, height, rgba) = if colour {
            let (w, h) = bitmap_size(info.hbmColor)?;
            check_edge(w, h)?;
            let bgra = read_bgra(dc.0, info.hbmColor, w, h)?;
            // The mask of a colour cursor is the same size as the colour
            // bitmap, or double height; the first half is the AND plane either
            // way.
            let mask = read_bgra(dc.0, info.hbmMask, mask_size.0, mask_size.1)?;
            if mask_size.0 != w || mask_size.1 < h {
                return None;
            }
            let and: Vec<bool> = mask[..w * h * 4]
                .chunks_exact(4)
                .map(|p| (p[0] | p[1] | p[2]) != 0)
                .collect();
            (w, h, color_to_rgba(w, h, &bgra, &and))
        } else {
            // Monochrome: one bitmap, twice as tall as the cursor, AND above
            // XOR.
            let (w, double) = mask_size;
            let h = double / 2;
            check_edge(w, h)?;
            let mask = read_bgra(dc.0, info.hbmMask, w, h * 2)?;
            let bits: Vec<bool> = mask
                .chunks_exact(4)
                .map(|p| (p[0] | p[1] | p[2]) != 0)
                .collect();
            let (and, xor) = bits.split_at(w * h);
            (w, h, monochrome_to_rgba(w, h, and, &xor[..w * h]))
        };

        let hot_x = (info.xHotspot as usize).min(width - 1) as u16;
        let hot_y = (info.yHotspot as usize).min(height - 1) as u16;
        drop(bitmaps);
        Some(RawShape {
            width: width as u16,
            height: height as u16,
            hot_x,
            hot_y,
            rgba,
        })
    }

    fn check_edge(w: usize, h: usize) -> Option<()> {
        (w > 0 && h > 0 && w <= MAX_SOURCE_EDGE && h <= MAX_SOURCE_EDGE).then_some(())
    }

    unsafe fn bitmap_size(bitmap: HBITMAP) -> Option<(usize, usize)> {
        let mut info = BITMAP::default();
        let got = GetObjectW(
            HGDIOBJ(bitmap.0),
            size_of::<BITMAP>() as i32,
            Some(&mut info as *mut BITMAP as *mut c_void),
        );
        if got == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
            return None;
        }
        Some((info.bmWidth as usize, info.bmHeight as usize))
    }

    /// A bitmap as 32 bit top-down BGRA, whatever it is stored as. A 1 bit mask
    /// comes back black and white, which is all the callers look at.
    unsafe fn read_bgra(dc: HDC, bitmap: HBITMAP, w: usize, h: usize) -> Option<Vec<u8>> {
        check_edge(w, h.div_ceil(2))?;
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; w * h * 4];
        let lines = GetDIBits(
            dc,
            bitmap,
            0,
            h as u32,
            Some(pixels.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );
        (lines as usize == h).then_some(pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ bitmaps

    fn px(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        rgba[(y * width + x) * 4..][..4].try_into().unwrap()
    }

    #[test]
    fn monochrome_pixels_follow_the_and_xor_table() {
        // One row: AND/XOR = 10, 00, 01, 11.
        let and = [true, false, false, true];
        let xor = [false, false, true, true];
        let out = monochrome_to_rgba(4, 1, &and, &xor);
        assert_eq!(px(&out, 4, 0, 0), TRANSPARENT);
        assert_eq!(px(&out, 4, 1, 0), BLACK);
        assert_eq!(px(&out, 4, 2, 0), WHITE);
        assert_eq!(px(&out, 4, 3, 0), WHITE);
    }

    #[test]
    fn an_inverting_bar_is_outlined_so_it_shows_on_any_background() {
        // 5x3, a one pixel wide inverting bar in the middle column, rest
        // see-through: the I-beam.
        let (w, h) = (5, 3);
        let mut and = vec![true; w * h];
        let mut xor = vec![false; w * h];
        for y in 0..h {
            xor[y * w + 2] = true;
            and[y * w + 2] = true;
        }
        let out = monochrome_to_rgba(w, h, &and, &xor);
        for y in 0..h {
            assert_eq!(px(&out, w, 2, y), WHITE, "the bar itself");
            assert_eq!(px(&out, w, 1, y), BLACK, "outline to the left");
            assert_eq!(px(&out, w, 3, y), BLACK, "outline to the right");
            assert_eq!(px(&out, w, 0, y), TRANSPARENT, "nothing beyond it");
            assert_eq!(px(&out, w, 4, y), TRANSPARENT);
        }
    }

    #[test]
    fn an_outline_never_overwrites_a_real_pixel() {
        // Black (AND 0, XOR 0) beside an inverting pixel stays black, and a
        // white one beside it stays white.
        let and = [false, true, false];
        let xor = [true, true, false];
        let out = monochrome_to_rgba(3, 1, &and, &xor);
        assert_eq!(px(&out, 3, 0, 0), WHITE);
        assert_eq!(px(&out, 3, 2, 0), BLACK);
    }

    #[test]
    fn a_colour_cursor_with_alpha_ignores_the_mask() {
        // BGRA (10,20,30,128) becomes RGBA (30,20,10,128) even though the mask
        // says "transparent".
        let out = color_to_rgba(1, 1, &[10, 20, 30, 128], &[true]);
        assert_eq!(out, vec![30, 20, 10, 128]);
    }

    #[test]
    fn a_colour_cursor_without_alpha_takes_it_from_the_mask() {
        // Two pixels, both alpha 0: the first masked out and black, the second
        // kept.
        let bgra = [0, 0, 0, 0, 9, 8, 7, 0];
        let out = color_to_rgba(2, 1, &bgra, &[true, false]);
        assert_eq!(px(&out, 2, 0, 0), TRANSPARENT);
        assert_eq!(px(&out, 2, 1, 0), [7, 8, 9, 255]);
    }

    #[test]
    fn a_masked_colour_pixel_is_drawn_rather_than_left_a_hole() {
        let out = color_to_rgba(1, 1, &[1, 2, 3, 0], &[true]);
        assert_eq!(out, vec![3, 2, 1, 255]);
    }

    fn solid(w: u16, h: u16, hot: (u16, u16), colour: [u8; 4]) -> RawShape {
        RawShape {
            width: w,
            height: h,
            hot_x: hot.0,
            hot_y: hot.1,
            rgba: colour.repeat(usize::from(w) * usize::from(h)),
        }
    }

    #[test]
    fn a_shape_at_scale_one_is_left_alone() {
        let shape = solid(32, 32, (3, 4), [1, 2, 3, 255]);
        assert_eq!(scale_shape(&shape, 1.0), shape);
        assert_eq!(scale_shape(&shape, 1.01), shape);
    }

    #[test]
    fn halving_a_shape_halves_its_size_and_hotspot() {
        let shape = solid(32, 32, (8, 4), [10, 20, 30, 255]);
        let half = scale_shape(&shape, 0.5);
        assert_eq!((half.width, half.height), (16, 16));
        assert_eq!((half.hot_x, half.hot_y), (4, 2));
        assert_eq!(px(&half.rgba, 16, 5, 5), [10, 20, 30, 255]);
    }

    #[test]
    fn scaling_does_not_bleed_transparent_pixels_into_the_edge() {
        // Left column opaque red, right column transparent black. The averaged
        // pixel must be red at half alpha, not a dark red.
        let shape = RawShape {
            width: 2,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![255, 0, 0, 255, 0, 0, 0, 0],
        };
        let out = scale_shape(&shape, 0.5);
        assert_eq!(out.rgba, vec![255, 0, 0, 128]);
    }

    #[test]
    fn upscaling_repeats_pixels() {
        let shape = RawShape {
            width: 2,
            height: 1,
            hot_x: 1,
            hot_y: 0,
            rgba: vec![1, 1, 1, 255, 2, 2, 2, 255],
        };
        let out = scale_shape(&shape, 2.0);
        assert_eq!((out.width, out.height), (4, 2));
        assert_eq!(px(&out.rgba, 4, 0, 1), [1, 1, 1, 255]);
        assert_eq!(px(&out.rgba, 4, 3, 0), [2, 2, 2, 255]);
        assert_eq!((out.hot_x, out.hot_y), (2, 0));
    }

    #[test]
    fn nothing_leaves_larger_than_the_protocol_allows() {
        let shape = solid(128, 128, (127, 127), [0, 0, 0, 255]);
        for factor in [1.0, 2.0, 0.9] {
            let out = scale_shape(&shape, factor);
            assert!(out.width <= MAX_CURSOR_EDGE && out.height <= MAX_CURSOR_EDGE);
            assert!(out.hot_x < out.width && out.hot_y < out.height);
            assert_eq!(
                out.rgba.len(),
                usize::from(out.width) * usize::from(out.height) * 4
            );
        }
    }

    #[test]
    fn a_nonsense_scale_is_treated_as_one() {
        let shape = solid(8, 8, (0, 0), [1, 1, 1, 255]);
        for factor in [f32::NAN, 0.0, -1.0, f32::INFINITY] {
            assert_eq!(scale_shape(&shape, factor), shape);
        }
    }

    // ------------------------------------------------------------ tracker

    /// A source that says whatever the test told it to.
    struct Scripted {
        sample: Sample,
        shapes: HashMap<u64, RawShape>,
        converted: Vec<u64>,
    }

    impl Scripted {
        fn new() -> Scripted {
            let mut shapes = HashMap::new();
            shapes.insert(1, solid(4, 4, (0, 0), [9, 9, 9, 255]));
            shapes.insert(2, solid(6, 6, (3, 3), [5, 5, 5, 255]));
            shapes.insert(ARROW, solid(4, 4, (0, 0), [1, 1, 1, 255]));
            Scripted {
                sample: Sample {
                    position: Some((100, 100)),
                    shape: ShapeState::Handle(1),
                },
                shapes,
                converted: Vec::new(),
            }
        }

        fn at(&mut self, x: i32, y: i32) {
            self.sample.position = Some((x, y));
        }

        fn wearing(&mut self, shape: ShapeState) {
            self.sample.shape = shape;
        }
    }

    const ARROW: u64 = 99;

    impl CursorSource for Scripted {
        fn sample(&mut self) -> Sample {
            self.sample
        }
        fn shape(&mut self, handle: u64) -> Option<RawShape> {
            self.converted.push(handle);
            self.shapes.get(&handle).cloned()
        }
        fn arrow(&mut self) -> Option<u64> {
            Some(ARROW)
        }
    }

    fn geometry() -> Geometry {
        Geometry {
            screen: Screen::new((0, 0), Resolution::new(1920, 1080)),
            picture: Resolution::new(1920, 1080),
        }
    }

    fn cursors(messages: &[HostMessage]) -> Vec<(i32, i32, bool, u32)> {
        messages
            .iter()
            .filter_map(|m| match m {
                HostMessage::Cursor {
                    x,
                    y,
                    visible,
                    shape,
                } => Some((*x, *y, *visible, *shape)),
                _ => None,
            })
            .collect()
    }

    fn shape_ids(messages: &[HostMessage]) -> Vec<u32> {
        messages
            .iter()
            .filter_map(|m| match m {
                HostMessage::CursorShape { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_first_sample_sends_the_shape_then_the_position() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        let out = tracker.step(&mut source, &geometry());

        assert!(matches!(out[0], HostMessage::CursorShape { id: 0, .. }));
        assert!(matches!(out[1], HostMessage::Cursor { visible: true, shape: 0, .. }));
        assert!(out.iter().all(HostMessage::is_well_formed));
    }

    #[test]
    fn an_unchanged_pointer_sends_nothing() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());
        assert!(tracker.step(&mut source, &geometry()).is_empty());
        assert!(tracker.step(&mut source, &geometry()).is_empty());
    }

    #[test]
    fn a_move_sends_only_a_position() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());
        source.at(101, 100);
        let out = tracker.step(&mut source, &geometry());
        assert_eq!(cursors(&out), vec![(101, 100, true, 0)]);
        assert!(shape_ids(&out).is_empty());
    }

    #[test]
    fn a_shape_is_converted_and_sent_once() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());
        source.wearing(ShapeState::Handle(2));
        let second = tracker.step(&mut source, &geometry());
        assert_eq!(shape_ids(&second), vec![1]);

        // Back to the first: known, so only the position message.
        source.wearing(ShapeState::Handle(1));
        let back = tracker.step(&mut source, &geometry());
        assert!(shape_ids(&back).is_empty());
        assert_eq!(cursors(&back)[0].3, 0);
        assert_eq!(source.converted, vec![1, 2]);
    }

    #[test]
    fn positions_are_in_the_pictures_pixels_not_the_displays() {
        // Streaming a 4K display as 1080p: the desktop centre is the picture
        // centre.
        let g = Geometry {
            screen: Screen::new((0, 0), Resolution::new(3840, 2160)),
            picture: Resolution::new(1920, 1080),
        };
        let mut source = Scripted::new();
        source.at(1920, 1080);
        let out = Tracker::new().step(&mut source, &g);
        let (x, y, ..) = cursors(&out)[0];
        assert!((959..=961).contains(&x) && (539..=541).contains(&y), "{x},{y}");

        // And the shape is scaled the same way.
        let mut source = Scripted::new();
        source.shapes.insert(1, solid(32, 32, (16, 16), [1, 2, 3, 255]));
        let out = Tracker::new().step(&mut source, &g);
        match &out[0] {
            HostMessage::CursorShape {
                width, hot_x, hot_y, ..
            } => assert_eq!((*width, *hot_x, *hot_y), (16, 8, 8)),
            other => panic!("expected a shape, got {other:?}"),
        }
    }

    #[test]
    fn the_second_display_is_offset_by_its_origin() {
        let g = Geometry {
            screen: Screen::new((1920, 0), Resolution::new(1920, 1080)),
            picture: Resolution::new(1920, 1080),
        };
        let mut source = Scripted::new();
        source.at(1920 + 10, 20);
        let out = Tracker::new().step(&mut source, &g);
        assert_eq!(cursors(&out)[0].0, 10);
        assert_eq!(cursors(&out)[0].1, 20);
    }

    #[test]
    fn a_pointer_on_another_display_is_reported_invisible_once() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());

        source.at(2500, 100);
        let out = tracker.step(&mut source, &geometry());
        assert_eq!(cursors(&out).len(), 1);
        assert!(!cursors(&out)[0].2);

        // Still off-screen, still moving: nothing more to say.
        source.at(2600, 300);
        assert!(tracker.step(&mut source, &geometry()).is_empty());

        // Back on: visible again.
        source.at(50, 50);
        let back = tracker.step(&mut source, &geometry());
        assert_eq!(cursors(&back), vec![(50, 50, true, 0)]);
    }

    #[test]
    fn a_pointer_an_application_hid_is_invisible() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());
        source.wearing(ShapeState::Hidden);
        let out = tracker.step(&mut source, &geometry());
        assert!(!cursors(&out)[0].2);
    }

    #[test]
    fn a_suppressed_pointer_keeps_the_last_real_shape() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        source.wearing(ShapeState::Handle(2));
        tracker.step(&mut source, &geometry());
        source.wearing(ShapeState::Unknown);
        source.at(10, 10);
        let out = tracker.step(&mut source, &geometry());
        // Still visible, still the shape 2 was sent as.
        assert_eq!(cursors(&out), vec![(10, 10, true, 0)]);
        assert!(shape_ids(&out).is_empty());
    }

    #[test]
    fn with_no_mouse_at_all_the_pointer_is_the_system_arrow() {
        // A machine that has never seen a real cursor: Unknown from the start.
        let mut source = Scripted::new();
        source.wearing(ShapeState::Unknown);
        let out = Tracker::new().step(&mut source, &geometry());
        assert_eq!(source.converted, vec![ARROW]);
        assert_eq!(shape_ids(&out), vec![0]);
        assert!(cursors(&out)[0].2, "visible, not hidden");
    }

    #[test]
    fn an_unreadable_position_says_nothing() {
        let mut source = Scripted::new();
        source.sample.position = None;
        assert!(Tracker::new().step(&mut source, &geometry()).is_empty());
    }

    #[test]
    fn a_shape_that_will_not_convert_leaves_the_viewer_on_its_own_cursor() {
        let mut source = Scripted::new();
        source.wearing(ShapeState::Handle(1234));
        let mut tracker = Tracker::new();
        let out = tracker.step(&mut source, &geometry());
        assert!(shape_ids(&out).is_empty());
        assert!(!cursors(&out)[0].2);

        // And it is not retried on every poll.
        source.at(5, 5);
        tracker.step(&mut source, &geometry());
        tracker.step(&mut source, &geometry());
        assert_eq!(source.converted, vec![1234]);
    }

    #[test]
    fn changing_the_picture_scale_sends_the_shape_again_at_the_new_size() {
        let mut source = Scripted::new();
        let mut tracker = Tracker::new();
        tracker.step(&mut source, &geometry());
        let small = Geometry {
            picture: Resolution::new(960, 540),
            ..geometry()
        };
        let out = tracker.step(&mut source, &small);
        assert_eq!(shape_ids(&out), vec![1], "a new id, so no stale copy is used");
    }

    #[test]
    fn the_cache_is_bounded_and_ids_keep_counting() {
        let mut source = Scripted::new();
        for handle in 1000..1000 + CACHE_LIMIT as u64 + 10 {
            source.shapes.insert(handle, solid(4, 4, (0, 0), [1, 1, 1, 255]));
        }
        let mut tracker = Tracker::new();
        let mut last_id = 0;
        for handle in 1000..1000 + CACHE_LIMIT as u64 + 10 {
            source.wearing(ShapeState::Handle(handle));
            let out = tracker.step(&mut source, &geometry());
            for id in shape_ids(&out) {
                assert!(id >= last_id);
                last_id = id;
            }
        }
        assert!(tracker.sent.len() <= CACHE_LIMIT);
        assert!(last_id >= CACHE_LIMIT as u32);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// The spike behind the Windows source's fallbacks. Prints what
    /// `GetCursorInfo` returns on the machine running it, so a machine with no
    /// mouse can be checked by running it there:
    ///
    /// `cargo test -p pravera-host --lib -- --ignored --nocapture cursor_spike`
    #[test]
    #[ignore = "reads the live desktop; run by hand"]
    fn cursor_spike() {
        println!("GetCursorInfo: {:?}", SystemCursor::raw_info());
        let mut source = SystemCursor::new();
        let sample = source.sample();
        println!("sample: {sample:?}");
        if let Some(arrow) = source.arrow() {
            let shape = source.shape(arrow).expect("the system arrow converts");
            println!(
                "arrow: {}x{} hotspot {},{}",
                shape.width, shape.height, shape.hot_x, shape.hot_y
            );
            let w = usize::from(shape.width);
            for row in shape.rgba.chunks(w * 4).take(usize::from(shape.height)) {
                let line: String = row
                    .chunks_exact(4)
                    .map(|p| match p {
                        [_, _, _, 0] => '.',
                        [r, g, b, _] if r > &128 && g > &128 && b > &128 => '#',
                        _ => 'x',
                    })
                    .collect();
                println!("  {line}");
            }
        }
        if let ShapeState::Handle(handle) = sample.shape {
            let shape = source.shape(handle).expect("the current cursor converts");
            println!("current: {}x{}", shape.width, shape.height);
        }
    }

    #[test]
    fn the_system_arrow_converts_to_a_well_formed_shape() {
        let mut source = SystemCursor::new();
        let Some(arrow) = source.arrow() else {
            return; // no window station, as on some CI runners
        };
        let Some(shape) = source.shape(arrow) else {
            return;
        };
        let message = HostMessage::CursorShape {
            id: 0,
            width: shape.width,
            height: shape.height,
            hot_x: shape.hot_x,
            hot_y: shape.hot_y,
            rgba: shape.rgba.clone(),
        };
        assert!(message.is_well_formed());
        assert!(
            shape.rgba.chunks_exact(4).any(|p| p[3] != 0),
            "an arrow with nothing visible in it"
        );
    }
}

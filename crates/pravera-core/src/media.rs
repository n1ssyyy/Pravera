use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
    pub const fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }
    pub fn aspect_ratio(self) -> f32 {
        if self.height == 0 {
            0.0
        } else {
            self.width as f32 / self.height as f32
        }
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A dirty region of the framebuffer. The capture layer reports these so the
/// encoder can skip untouched macroblocks — the single biggest win for desktop
/// content, where most frames change almost nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub const fn covering(res: Resolution) -> Self {
        Self {
            x: 0,
            y: 0,
            width: res.width,
            height: res.height,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub const fn area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Smallest rectangle containing both. Used to coalesce scattered damage
    /// into fewer encoder regions when the count gets high.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Rect {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

/// Layout of raw pixels as they leave capture and enter the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// Windows Desktop Duplication native output.
    Bgra8,
    Rgba8,
    /// Hardware encoder input on both platforms; 4:2:0, two planes.
    Nv12,
    /// 4:4:4 planar — the Quality profile path, preserves text edges.
    Yuv444,
}

impl PixelFormat {
    /// Bytes for one frame at this resolution.
    pub const fn frame_size(self, res: Resolution) -> usize {
        let px = (res.width * res.height) as usize;
        match self {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => px * 4,
            PixelFormat::Nv12 => px * 3 / 2,
            PixelFormat::Yuv444 => px * 3,
        }
    }

    pub const fn is_chroma_subsampled(self) -> bool {
        matches!(self, PixelFormat::Nv12)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    /// Universal floor — every device with any hardware encoder speaks it.
    H264,
    /// Better compression at the same quality; widely available since Skylake.
    H265,
    /// Royalty-free and the best quality per bit, but needs RTX 40+ / RDNA3 /
    /// Arc to encode in hardware.
    Av1,
    /// Software fallback so a session is always possible.
    OpenH264,
}

impl Codec {
    pub const fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265",
            Codec::Av1 => "AV1",
            Codec::OpenH264 => "H.264 (software)",
        }
    }

    /// Ranking used when both peers advertise several codecs. Higher wins.
    pub const fn preference(self) -> u8 {
        match self {
            Codec::Av1 => 3,
            Codec::H265 => 2,
            Codec::H264 => 1,
            Codec::OpenH264 => 0,
        }
    }

    pub const fn is_hardware(self) -> bool {
        !matches!(self, Codec::OpenH264)
    }
}

/// Metadata travelling alongside every encoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameFormat {
    pub resolution: Resolution,
    pub pixel_format: PixelFormat,
    pub codec: Codec,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_is_half_the_size_of_bgra() {
        let res = Resolution::new(1920, 1080);
        assert_eq!(PixelFormat::Bgra8.frame_size(res), 1920 * 1080 * 4);
        assert_eq!(PixelFormat::Nv12.frame_size(res), 1920 * 1080 * 3 / 2);
    }

    #[test]
    fn union_of_scattered_damage_covers_both() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(90, 90, 10, 10);
        assert_eq!(a.union(b), Rect::new(0, 0, 100, 100));
    }

    #[test]
    fn union_ignores_empty_rects() {
        let a = Rect::new(5, 5, 20, 20);
        let empty = Rect::new(0, 0, 0, 0);
        assert_eq!(a.union(empty), a);
        assert_eq!(empty.union(a), a);
    }

    #[test]
    fn av1_outranks_h264_which_outranks_software() {
        assert!(Codec::Av1.preference() > Codec::H265.preference());
        assert!(Codec::H264.preference() > Codec::OpenH264.preference());
        assert!(!Codec::OpenH264.is_hardware());
    }
}

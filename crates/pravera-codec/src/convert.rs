//! Turning captured pixels into what a hardware encoder wants.
//!
//! Capture hands over BGRA — that is what both Desktop Duplication and Windows
//! Graphics Capture produce — and every hardware H.264 encoder takes NV12. So
//! something has to walk two million pixels per frame and rewrite them.
//!
//! ## Which numbers
//!
//! BT.601, limited range. Not because it is the best choice for HD content —
//! BT.709 is what 1080p is actually specified in — but because the far end
//! decodes with openh264, whose YUV-to-RGB conversion assumes 601 and reads no
//! VUI. Encoding in 709 and decoding as 601 does not fail; it shifts every
//! colour slightly, greens most, and the result is a picture that looks subtly
//! wrong in a way nobody can point at.
//!
//! Limited range for the same reason: 16–235 for luma is what a decoder assumes
//! when the stream does not say otherwise, and a full-range stream read as
//! limited comes out with crushed blacks and clipped highlights.
//!
//! ## Chroma
//!
//! Each 2×2 block of pixels becomes one chroma pair, and the four source
//! pixels are averaged in RGB before conversion rather than one of them being
//! sampled. Averaging costs three adds per component and removes the crawling
//! edges that point-sampling puts on any diagonal — which on a desktop full of
//! text and window borders is most of the picture.

use pravera_core::{PixelFormat, Resolution};

use crate::{CodecError, RawFrame, Result};

/// Fixed-point BT.601 coefficients, scaled by 256.
///
/// Integer arithmetic throughout: this runs on every pixel of every frame, and
/// the rounding differences against floating point are below one code value.
mod bt601 {
    pub const Y_R: i32 = 66;
    pub const Y_G: i32 = 129;
    pub const Y_B: i32 = 25;
    pub const Y_OFFSET: i32 = 16;

    pub const U_R: i32 = -38;
    pub const U_G: i32 = -74;
    pub const U_B: i32 = 112;

    pub const V_R: i32 = 112;
    pub const V_G: i32 = -94;
    pub const V_B: i32 = -18;

    pub const C_OFFSET: i32 = 128;
    /// Half of the scale, added before shifting so the shift rounds to nearest.
    pub const HALF: i32 = 128;
    pub const SHIFT: u32 = 8;
}

#[inline]
fn luma(r: i32, g: i32, b: i32) -> u8 {
    let y = (bt601::Y_R * r + bt601::Y_G * g + bt601::Y_B * b + bt601::HALF) >> bt601::SHIFT;
    (y + bt601::Y_OFFSET) as u8
}

#[inline]
fn chroma(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = (bt601::U_R * r + bt601::U_G * g + bt601::U_B * b + bt601::HALF) >> bt601::SHIFT;
    let v = (bt601::V_R * r + bt601::V_G * g + bt601::V_B * b + bt601::HALF) >> bt601::SHIFT;
    (
        (u + bt601::C_OFFSET).clamp(0, 255) as u8,
        (v + bt601::C_OFFSET).clamp(0, 255) as u8,
    )
}

/// Where red and blue sit in a four-byte pixel.
///
/// The only difference between the two formats capture produces, so it is the
/// only thing the conversion has to branch on — and it branches once per frame
/// rather than once per pixel.
#[derive(Clone, Copy)]
struct Order {
    red: usize,
    blue: usize,
}

/// Convert a captured frame into NV12, in place in `out`.
///
/// `out` must already be [`PixelFormat::Nv12`]-sized for `target`. The frame
/// may be larger than `target` — a display with an odd width is cropped by a
/// pixel rather than resampled — and may be padded, since a GPU buffer's stride
/// is its own business.
pub(crate) fn to_nv12(frame: &RawFrame<'_>, target: Resolution, out: &mut [u8]) -> Result<()> {
    let order = match frame.format {
        PixelFormat::Bgra8 => Order { red: 2, blue: 0 },
        PixelFormat::Rgba8 => Order { red: 0, blue: 2 },
        other => return Err(CodecError::UnsupportedInput(other)),
    };

    let (width, height) = (target.width as usize, target.height as usize);
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(CodecError::BadDimensions {
            resolution: target,
            reason: "NV12 needs an even width and height",
        });
    }
    if out.len() < PixelFormat::Nv12.frame_size(target) {
        return Err(CodecError::BadDimensions {
            resolution: target,
            reason: "the NV12 buffer is too small for this frame",
        });
    }
    if frame.stride < width * 4 || frame.pixels.len() < frame.stride * height {
        return Err(CodecError::BadDimensions {
            resolution: frame.resolution,
            reason: "the captured buffer is smaller than its own description",
        });
    }

    let (luma_plane, chroma_plane) = out.split_at_mut(width * height);

    for y in 0..height {
        let row = &frame.pixels[y * frame.stride..y * frame.stride + width * 4];
        let target_row = &mut luma_plane[y * width..(y + 1) * width];
        for (x, out) in target_row.iter_mut().enumerate() {
            let px = &row[x * 4..x * 4 + 4];
            *out = luma(px[order.red] as i32, px[1] as i32, px[order.blue] as i32);
        }
    }

    // One chroma pair per 2×2 block, from the average of the four pixels.
    for block_y in 0..height / 2 {
        let top = &frame.pixels[(block_y * 2) * frame.stride..][..width * 4];
        let bottom = &frame.pixels[(block_y * 2 + 1) * frame.stride..][..width * 4];
        let target_row = &mut chroma_plane[block_y * width..(block_y + 1) * width];

        for block_x in 0..width / 2 {
            let at = block_x * 8;
            let mut r = 0i32;
            let mut g = 0i32;
            let mut b = 0i32;
            for plane in [top, bottom] {
                for half in [0usize, 4] {
                    let px = &plane[at + half..at + half + 4];
                    r += px[order.red] as i32;
                    g += px[1] as i32;
                    b += px[order.blue] as i32;
                }
            }

            let (u, v) = chroma(r / 4, g / 4, b / 4);
            target_row[block_x * 2] = u;
            target_row[block_x * 2 + 1] = v;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Resolution = Resolution::new(4, 4);

    fn flat(colour: [u8; 4], resolution: Resolution, stride: Option<usize>) -> (Vec<u8>, usize) {
        let stride = stride.unwrap_or(resolution.width as usize * 4);
        let mut pixels = vec![0u8; stride * resolution.height as usize];
        for row in 0..resolution.height as usize {
            for x in 0..resolution.width as usize {
                pixels[row * stride + x * 4..row * stride + x * 4 + 4].copy_from_slice(&colour);
            }
        }
        (pixels, stride)
    }

    fn convert(pixels: &[u8], stride: usize, format: PixelFormat, res: Resolution) -> Vec<u8> {
        let frame = RawFrame {
            resolution: res,
            format,
            stride,
            pixels,
            capture_micros: 0,
        };
        let mut out = vec![0u8; PixelFormat::Nv12.frame_size(res)];
        to_nv12(&frame, res, &mut out).expect("convert");
        out
    }

    #[test]
    fn black_lands_on_the_bottom_of_the_limited_range() {
        // 16, not 0. A decoder expecting limited range and given 0 produces
        // something blacker than black, which clips.
        let (pixels, stride) = flat([0, 0, 0, 255], SMALL, None);
        let nv12 = convert(&pixels, stride, PixelFormat::Bgra8, SMALL);
        assert_eq!(nv12[0], 16);
        assert_eq!(nv12[15], 16);
    }

    #[test]
    fn white_lands_on_the_top_of_the_limited_range() {
        let (pixels, stride) = flat([255, 255, 255, 255], SMALL, None);
        let nv12 = convert(&pixels, stride, PixelFormat::Bgra8, SMALL);
        assert_eq!(nv12[0], 235);
    }

    #[test]
    fn a_grey_frame_has_no_colour_in_it() {
        // Both chroma planes sit at their neutral point, or the picture has a
        // tint that was not in the original.
        let (pixels, stride) = flat([128, 128, 128, 255], SMALL, None);
        let nv12 = convert(&pixels, stride, PixelFormat::Bgra8, SMALL);
        for byte in &nv12[SMALL.width as usize * SMALL.height as usize..] {
            assert_eq!(*byte, 128, "grey picked up a colour cast");
        }
    }

    #[test]
    fn blue_and_red_are_not_swapped() {
        // The whole difference between the two input formats. Getting it
        // backwards produces a picture that looks fine until something is red.
        let (bgra, stride) = flat([255, 0, 0, 255], SMALL, None); // blue in BGRA
        let blue = convert(&bgra, stride, PixelFormat::Bgra8, SMALL);

        let (rgba, stride) = flat([255, 0, 0, 255], SMALL, None); // red in RGBA
        let red = convert(&rgba, stride, PixelFormat::Rgba8, SMALL);

        let plane = SMALL.width as usize * SMALL.height as usize;
        // Blue is the darkest primary and red is in the middle; if the two
        // orders agreed, these would be equal.
        assert!(blue[0] < red[0], "blue {} red {}", blue[0], red[0]);
        // Blue pushes U up and V down; red does the opposite.
        assert!(blue[plane] > 128 && blue[plane + 1] < 128, "{blue:?}");
        assert!(red[plane] < 128 && red[plane + 1] > 128, "{red:?}");
    }

    #[test]
    fn a_padded_capture_buffer_is_read_by_its_stride() {
        // A GPU buffer's rows are padded to its own alignment. Reading them as
        // though they were tight produces a picture that shears diagonally.
        let padded = SMALL.width as usize * 4 + 64;
        let (pixels, stride) = flat([255, 255, 255, 255], SMALL, Some(padded));
        let nv12 = convert(&pixels, stride, PixelFormat::Bgra8, SMALL);
        assert!(nv12[..16].iter().all(|&y| y == 235), "{:?}", &nv12[..16]);
    }

    #[test]
    fn a_chroma_pair_is_the_average_of_its_block_rather_than_one_corner() {
        // Half the block white, half black. Point-sampling would give the
        // chroma of whichever corner was sampled; averaging gives neutral.
        let mut pixels = vec![0u8; 4 * 4 * 4];
        for y in 0..4 {
            for x in 0..4 {
                let at = (y * 4 + x) * 4;
                let value = if x % 2 == 0 { 255 } else { 0 };
                pixels[at..at + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let nv12 = convert(&pixels, 16, PixelFormat::Bgra8, SMALL);
        let plane = 16;
        assert_eq!(nv12[plane], 128, "averaging a grey block produced a tint");
        assert_eq!(nv12[plane + 1], 128);
    }

    #[test]
    fn an_odd_size_is_refused_rather_than_read_off_the_end() {
        let odd = Resolution::new(5, 4);
        let (pixels, stride) = flat([0, 0, 0, 255], odd, None);
        let frame = RawFrame {
            resolution: odd,
            format: PixelFormat::Bgra8,
            stride,
            pixels: &pixels,
            capture_micros: 0,
        };
        let mut out = vec![0u8; 5 * 4 * 3 / 2 + 4];
        assert!(to_nv12(&frame, odd, &mut out).is_err());
    }

    #[test]
    fn a_buffer_smaller_than_it_claims_is_refused() {
        // Everything here can come from a capture backend that got it wrong,
        // and the failure mode of trusting it is reading someone else's memory.
        let frame = RawFrame {
            resolution: SMALL,
            format: PixelFormat::Bgra8,
            stride: 16,
            pixels: &[0u8; 8],
            capture_micros: 0,
        };
        let mut out = vec![0u8; PixelFormat::Nv12.frame_size(SMALL)];
        assert!(to_nv12(&frame, SMALL, &mut out).is_err());
    }

    #[test]
    fn a_frame_that_is_not_four_bytes_a_pixel_is_refused() {
        let frame = RawFrame {
            resolution: SMALL,
            format: PixelFormat::Nv12,
            stride: 16,
            pixels: &[0u8; 64],
            capture_micros: 0,
        };
        let mut out = vec![0u8; PixelFormat::Nv12.frame_size(SMALL)];
        assert!(matches!(
            to_nv12(&frame, SMALL, &mut out),
            Err(CodecError::UnsupportedInput(PixelFormat::Nv12))
        ));
    }
}

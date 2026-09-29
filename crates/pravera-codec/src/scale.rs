//! Shrinking a frame before it reaches the encoder.
//!
//! Only downwards. A client that wants the picture larger than it arrives gets
//! that from its own GPU during presentation, for free and with a better
//! filter than anything worth writing here; sending more pixels than the
//! viewer's window can show is pure waste on the link and in the encoder.
//!
//! ## Why box averaging
//!
//! Nearest-neighbour is cheaper and wrong for this content. Dropping rows and
//! columns from text turns strokes into dotted lines and makes fine grids
//! shimmer as the window moves — and a remote desktop is mostly text and fine
//! grids. Averaging every source pixel that falls inside a destination pixel
//! costs one pass over the source and keeps thin features visible as grey
//! instead of deleting them.

use pravera_core::{PixelFormat, Resolution};

use crate::{CodecError, RawFrame, Result};

/// Reduces frames to a fixed size.
///
/// Holds its own output buffer and reuses it, so a running stream does not
/// allocate per frame.
#[derive(Debug)]
pub struct Scaler {
    target: Resolution,
    buffer: Vec<u8>,
}

impl Scaler {
    pub fn new(target: Resolution) -> Result<Scaler> {
        if target.width == 0 || target.height == 0 {
            return Err(CodecError::BadDimensions {
                resolution: target,
                reason: "cannot scale to nothing",
            });
        }
        Ok(Scaler {
            target,
            buffer: Vec::new(),
        })
    }

    pub fn target(&self) -> Resolution {
        self.target
    }

    /// Whether this frame needs scaling at all.
    pub fn is_needed_for(&self, resolution: Resolution) -> bool {
        resolution != self.target
    }

    /// Shrink one frame.
    ///
    /// The result borrows this scaler's buffer, so it lives until the next
    /// call — long enough to hand to an encoder, which is the only thing that
    /// ever wants it.
    pub fn scale<'a>(&'a mut self, frame: &RawFrame<'_>) -> Result<RawFrame<'a>> {
        frame.check()?;
        if !matches!(frame.format, PixelFormat::Bgra8 | PixelFormat::Rgba8) {
            return Err(CodecError::UnsupportedInput(frame.format));
        }

        let source = frame.resolution;
        if source.width < self.target.width || source.height < self.target.height {
            return Err(CodecError::BadDimensions {
                resolution: source,
                reason: "scaling up belongs on the client's GPU, not here",
            });
        }

        let (dst_w, dst_h) = (self.target.width as usize, self.target.height as usize);
        let (src_w, src_h) = (source.width as usize, source.height as usize);
        let out_stride = dst_w * 4;
        self.buffer.resize(out_stride * dst_h, 0);

        for dy in 0..dst_h {
            // The half-open source row band this destination row covers. The
            // `.max(top + 1)` guards the degenerate case where the ratio is so
            // close to 1 that the band would be empty and the row would come
            // out black.
            let top = dy * src_h / dst_h;
            let bottom = (((dy + 1) * src_h).div_ceil(dst_h)).max(top + 1).min(src_h);

            for dx in 0..dst_w {
                let left = dx * src_w / dst_w;
                let right = (((dx + 1) * src_w).div_ceil(dst_w))
                    .max(left + 1)
                    .min(src_w);

                let mut sums = [0u32; 4];
                let mut counted = 0u32;
                for sy in top..bottom {
                    let row = sy * frame.stride;
                    for sx in left..right {
                        let at = row + sx * 4;
                        for (channel, sum) in sums.iter_mut().enumerate() {
                            *sum += frame.pixels[at + channel] as u32;
                        }
                        counted += 1;
                    }
                }

                let at = dy * out_stride + dx * 4;
                for (channel, sum) in sums.iter().enumerate() {
                    self.buffer[at + channel] = (sum / counted.max(1)) as u8;
                }
            }
        }

        Ok(RawFrame {
            resolution: self.target,
            format: frame.format,
            stride: out_stride,
            pixels: &self.buffer,
            capture_micros: frame.capture_micros,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of solid colour.
    fn solid(resolution: Resolution, rgba: [u8; 4]) -> Vec<u8> {
        rgba.iter()
            .copied()
            .cycle()
            .take(resolution.width as usize * resolution.height as usize * 4)
            .collect()
    }

    fn frame<'a>(resolution: Resolution, pixels: &'a [u8]) -> RawFrame<'a> {
        RawFrame {
            resolution,
            format: PixelFormat::Bgra8,
            stride: resolution.width as usize * 4,
            pixels,
            capture_micros: 7,
        }
    }

    #[test]
    fn a_solid_colour_stays_exactly_that_colour() {
        // Averaging identical pixels must not drift. If it does, every flat
        // area of the desktop shifts a shade on every scaled frame.
        let source = Resolution::new(64, 48);
        let pixels = solid(source, [0x12, 0x34, 0x56, 0xff]);

        let mut scaler = Scaler::new(Resolution::new(16, 12)).unwrap();
        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();

        assert_eq!(scaled.resolution, Resolution::new(16, 12));
        assert_eq!(scaled.stride, 16 * 4);
        assert_eq!(scaled.pixels.len(), 16 * 12 * 4);
        assert!(scaled
            .pixels
            .chunks_exact(4)
            .all(|p| p == [0x12, 0x34, 0x56, 0xff]));
    }

    #[test]
    fn capture_time_and_channel_order_pass_through_untouched() {
        let source = Resolution::new(8, 8);
        let pixels = solid(source, [1, 2, 3, 4]);

        let mut scaler = Scaler::new(Resolution::new(4, 4)).unwrap();
        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();

        assert_eq!(scaled.capture_micros, 7);
        assert_eq!(scaled.format, PixelFormat::Bgra8);
    }

    #[test]
    fn halving_averages_each_two_by_two_block() {
        // Four distinct pixels collapse to their mean, not to whichever one
        // happened to be first.
        let source = Resolution::new(2, 2);
        let pixels = vec![
            0, 0, 0, 255, // black
            100, 100, 100, 255, //
            200, 200, 200, 255, //
            0, 0, 0, 255, //
        ];

        let mut scaler = Scaler::new(Resolution::new(1, 1)).unwrap();
        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();

        assert_eq!(&scaled.pixels[..3], &[75, 75, 75]);
        assert_eq!(scaled.pixels[3], 255);
    }

    #[test]
    fn a_thin_line_survives_as_grey_rather_than_disappearing() {
        // The whole reason this is not nearest-neighbour. A one-pixel line is
        // a letter stroke; dropping it is how downscaled text turns to mush.
        let source = Resolution::new(4, 4);
        let mut pixels = solid(source, [0, 0, 0, 255]);
        for x in 0..4 {
            // A single bright row at y = 1, which nearest-neighbour would skip.
            let at = 4 + x * 4;
            pixels[at..at + 3].copy_from_slice(&[255, 255, 255]);
        }

        let mut scaler = Scaler::new(Resolution::new(2, 2)).unwrap();
        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();

        assert!(
            scaled.pixels[0] > 100,
            "the line vanished: {:?}",
            &scaled.pixels[..4]
        );
    }

    #[test]
    fn padding_in_the_source_is_not_read_as_pixels() {
        let source = Resolution::new(4, 4);
        let stride = 4 * 4 + 32;
        let mut pixels = vec![0xffu8; stride * 4];
        for y in 0..4 {
            let row = y * stride;
            pixels[row..row + 16].copy_from_slice(&[0u8; 16]);
        }

        let mut scaler = Scaler::new(Resolution::new(2, 2)).unwrap();
        let scaled = scaler
            .scale(&RawFrame {
                resolution: source,
                format: PixelFormat::Bgra8,
                stride,
                pixels: &pixels,
                capture_micros: 0,
            })
            .unwrap();

        assert!(
            scaled.pixels.iter().all(|&b| b == 0),
            "padding bytes leaked into the picture: {:?}",
            scaled.pixels
        );
    }

    #[test]
    fn a_ratio_barely_above_one_still_covers_every_row() {
        // 100 rows into 99 leaves most bands one row tall. An off-by-one in
        // the band arithmetic shows up here as black stripes.
        let source = Resolution::new(100, 100);
        let pixels = solid(source, [40, 50, 60, 255]);

        let mut scaler = Scaler::new(Resolution::new(99, 99)).unwrap();
        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();

        assert!(scaled
            .pixels
            .chunks_exact(4)
            .all(|p| p == [40, 50, 60, 255]));
    }

    #[test]
    fn scaling_to_the_same_size_is_a_faithful_copy() {
        let source = Resolution::new(8, 4);
        let pixels: Vec<u8> = (0..(8 * 4 * 4) as u8).collect();

        let mut scaler = Scaler::new(source).unwrap();
        assert!(!scaler.is_needed_for(source));

        let scaled = scaler.scale(&frame(source, &pixels)).unwrap();
        assert_eq!(scaled.pixels, &pixels[..]);
    }

    #[test]
    fn scaling_up_is_refused_rather_than_faked() {
        let source = Resolution::new(4, 4);
        let pixels = solid(source, [0, 0, 0, 255]);

        let mut scaler = Scaler::new(Resolution::new(8, 8)).unwrap();
        assert!(matches!(
            scaler.scale(&frame(source, &pixels)),
            Err(CodecError::BadDimensions { .. })
        ));
    }

    #[test]
    fn a_zero_sized_target_is_refused_at_construction() {
        assert!(Scaler::new(Resolution::new(0, 100)).is_err());
        assert!(Scaler::new(Resolution::new(100, 0)).is_err());
    }
}

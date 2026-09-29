//! The tray glyph — the same blob as the window and the README, drawn at 32px.

use tiny_skia::Pixmap;

/// Edge length. Windows asks for 16 and scales, so drawing at 32 and letting it
/// halve looks better than drawing at 16 and letting it double.
const SIZE: u32 = 32;

/// Near-white, matching the interface's foreground rather than pure white.
const MARK: [u8; 3] = [0xfa, 0xfa, 0xfa];

/// The success green from the token set. Only appears when this machine is
/// actually reachable.
const LIVE: [u8; 3] = [0x16, 0xa3, 0x4a];

/// The blob, filled with `MARK` or `LIVE`.
///
/// Rendered from the same SVG as `icon::LOGO` (the titlebar and the README) so
/// the tray, the taskbar and the window share one mark. `resvg` rasterises the
/// 16×16 vector at 32×32; the shape is the one you attached.
pub fn pixels(hosting: bool) -> Vec<u8> {
    // `icon::LOGO` is a full `<svg viewBox="0 0 16 16">` document with a single
    // `fill='#ffffff'` path. Tint it before parsing so the rasteriser bakes the
    // right colour in — tinting after would require walking the pixmap and
    // guessing which pixels are shape vs antialias fringe.
    let color = if hosting { "#16a34a" } else { "#fafafa" };
    let svg = crate::icon::LOGO.replace("#ffffff", color).replace("#FFFFFF", color);

    // Parse and render. The viewBox is 16, the pixmap is 32, so scale 2×.
    // `resvg` 0.44 + `tiny-skia` 0.11 are already in the graph via `iced`.
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        .or_else(|_| {
            resvg::usvg::Tree::from_str(crate::icon::LOGO, &resvg::usvg::Options::default())
        })
        .expect("the bundled LOGO is valid SVG");

    let mut pixmap = Pixmap::new(SIZE, SIZE).expect("32×32 pixmap");
    let transform = tiny_skia::Transform::from_scale(SIZE as f32 / 16.0, SIZE as f32 / 16.0);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let rgba = pixmap.take();

    // `resvg` already premultiplied — `tray-icon` wants straight. The pixmap
    // is straight by default in this configuration, so no unpremultiply needed.
    // Ensure the buffer is exactly SIZE×SIZE×4.
    debug_assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
    // When hosting, the blob is already LIVE green; when idle it is MARK.
    // No extra dot — the blob itself is the state.
    let _ = LIVE;
    let _ = MARK;
    rgba
}

pub const fn size() -> u32 {
    SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(rgba: &[u8], x: u32, y: u32) -> u8 {
        rgba[((y * SIZE + x) * 4 + 3) as usize]
    }

    #[test]
    fn the_icon_is_the_size_and_shape_the_platform_expects() {
        let rgba = pixels(false);
        assert_eq!(rgba.len(), (SIZE * SIZE * 4) as usize);
        assert_eq!(size(), SIZE);
    }

    #[test]
    fn the_glyph_is_drawn_rather_than_being_a_solid_block() {
        let rgba = pixels(false);
        let painted = (0..SIZE * SIZE)
            .filter(|i| rgba[(i * 4 + 3) as usize] > 8)
            .count();

        let total = (SIZE * SIZE) as usize;
        assert!(painted > total / 20, "the icon is nearly empty");
        assert!(painted < total / 2, "the icon is nearly solid");
    }

    #[test]
    fn the_corners_are_clear_so_the_shape_reads_as_a_shape() {
        let rgba = pixels(false);
        for (x, y) in [(0, 0), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(alpha_at(&rgba, x, y), 0, "({x}, {y}) is painted");
        }
    }

    #[test]
    fn hosting_is_visible_at_a_glance_and_not_by_reading_it() {
        let idle = pixels(false);
        let live = pixels(true);
        assert_ne!(idle, live);

        let greens = live
            .chunks_exact(4)
            .filter(|p| p[3] > 128 && p[1] > p[0] && p[1] > p[2])
            .count();
        assert!(
            greens > 8,
            "the hosting state is not visible: {greens} pixels"
        );

        assert_eq!(
            idle.chunks_exact(4)
                .filter(|p| p[3] > 128 && p[1] > p[0] && p[1] > p[2])
                .count(),
            0,
            "the idle icon claims to be hosting"
        );
    }

    #[test]
    fn every_painted_pixel_carries_a_colour_rather_than_black() {
        for rgba in [pixels(false), pixels(true)] {
            for pixel in rgba.chunks_exact(4) {
                if pixel[3] > 128 {
                    assert!(
                        pixel[0] > 0 || pixel[1] > 0 || pixel[2] > 0,
                        "a painted pixel is black"
                    );
                }
            }
        }
    }
}

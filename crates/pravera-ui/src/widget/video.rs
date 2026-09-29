//! The remote screen, drawn inside the window.
//!
//! An `iced::widget::shader` program: iced hands it a `wgpu::Device` and
//! `Queue` during layout, and the picture is composited into the same render
//! pass as the rest of the interface. That is the whole reason for using a
//! shader widget rather than an `image` — the session toolbar has to float
//! *over* the stream, not beside it, and nothing else in iced composites a
//! per-frame texture with the UI on top.
//!
//! ## The CPU staging path, and why it is here to stay
//!
//! Frames arrive as tightly packed RGBA in ordinary memory, because the
//! software decoder converts YUV to RGBA on the way out. They are uploaded with
//! `write_texture` — one copy per frame, on the CPU.
//!
//! That copy is the thing P7 removes, by importing the decoder's GPU surface
//! directly through a shared handle on Windows and a dma-buf on Linux. Until
//! then this path is not a placeholder standing in for the real one: it *is*
//! the real one on any machine without a hardware decoder, and it stays as the
//! fallback forever. A remote desktop that only works on the right GPU is not
//! a remote desktop.
//!
//! ## Aspect ratio is not cosmetic
//!
//! [`fit`] letterboxes the picture inside the widget. Stretching would be
//! easier and is wrong for a reason beyond appearance: the client sends pointer
//! positions as a fraction of the *captured surface*, so the mapping from a
//! window pixel back to that fraction has to agree exactly with what is on
//! screen. One function computes the content rectangle, and both the vertex
//! shader and [`point_in`] use it. Two implementations of the same rectangle
//! is how a cursor ends up a few pixels off, everywhere, forever.

use std::sync::Arc;

use iced::widget::shader::{self, Primitive};
use iced::{mouse, Rectangle, Size};
use pravera_core::Resolution;

/// One decoded picture, ready to upload.
///
/// Reference-counted because the primitive is rebuilt on every redraw and the
/// frame behind it usually is not. Cloning this must stay cheap or a still
/// picture would cost a full-frame copy per repaint of any other widget.
#[derive(Debug, Clone)]
pub struct Picture {
    pub resolution: Resolution,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub pixels: Arc<Vec<u8>>,
    /// Rises with every frame the decoder produced.
    ///
    /// The upload is skipped when this has not changed, which is the common
    /// case: iced redraws for a hover animation far more often than a still
    /// desktop produces frames.
    pub generation: u64,
}

impl Picture {
    pub fn new(resolution: Resolution, pixels: Vec<u8>, generation: u64) -> Picture {
        Picture {
            resolution,
            pixels: Arc::new(pixels),
            generation,
        }
    }

    /// Whether the buffer really holds the frame it claims to.
    ///
    /// Checked before upload rather than trusted: `write_texture` with a short
    /// buffer is a validation error that takes down the renderer, and the
    /// bytes came from a decoder fed by a peer.
    pub fn is_consistent(&self) -> bool {
        self.resolution.width > 0
            && self.resolution.height > 0
            && self.pixels.len()
                == self.resolution.width as usize * self.resolution.height as usize * 4
    }
}

/// Where the picture sits inside the widget, keeping its aspect ratio.
///
/// Centred, never cropped, never stretched. Returns an empty rectangle for a
/// degenerate input rather than dividing by zero — a widget can genuinely be
/// laid out at zero width for a frame while a pane animates open.
pub fn fit(bounds: Rectangle, picture: Resolution) -> Rectangle {
    if bounds.width <= 0.0 || bounds.height <= 0.0 || picture.width == 0 || picture.height == 0 {
        return Rectangle::new(bounds.position(), Size::new(0.0, 0.0));
    }

    let scale = (bounds.width / picture.width as f32).min(bounds.height / picture.height as f32);
    let width = picture.width as f32 * scale;
    let height = picture.height as f32 * scale;

    Rectangle {
        x: bounds.x + (bounds.width - width) / 2.0,
        y: bounds.y + (bounds.height - height) / 2.0,
        width,
        height,
    }
}

/// Where a window position lands on the remote screen, as a fraction.
///
/// `None` when the point is in the letterbox rather than on the picture, which
/// is the correct answer: there is nowhere on the remote desktop that
/// corresponds to a black bar, and clamping would park the pointer on an edge
/// the person never pointed at.
pub fn point_in(bounds: Rectangle, picture: Resolution, point: iced::Point) -> Option<(f32, f32)> {
    let content = fit(bounds, picture);
    if content.width <= 0.0 || content.height <= 0.0 || !content.contains(point) {
        return None;
    }
    Some((
        ((point.x - content.x) / content.width).clamp(0.0, 1.0),
        ((point.y - content.y) / content.height).clamp(0.0, 1.0),
    ))
}

// --------------------------------------------------------------- the shader

/// What the vertex shader needs to letterbox.
///
/// `scale` is the fraction of the widget the picture occupies on each axis, in
/// normalised device coordinates. One axis is always 1.0.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    scale: [f32; 2],
    _padding: [f32; 2],
}

const SHADER: &str = r#"
struct Uniforms {
    scale: vec2<f32>,
};

@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var smooth_sampler: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// A full-screen triangle rather than a quad: three vertices instead of six,
// no index buffer, and no seam down the diagonal where two triangles meet.
@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOut {
    var out: VertexOut;
    let x = f32((index << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(index & 2u) * 2.0 - 1.0;

    out.position = vec4<f32>(x * uniforms.scale.x, y * uniforms.scale.y, 0.0, 1.0);
    // Texture space runs downwards; clip space runs upwards.
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

@fragment
fn fragment(in: VertexOut) -> @location(0) vec4<f32> {
    // Opaque: the desktop has no transparency, and forcing alpha to 1 stops a
    // stray alpha byte from the decoder blending the picture with the UI
    // behind it.
    return vec4<f32>(textureSample(picture, smooth_sampler, in.uv).rgb, 1.0);
}
"#;

/// The GPU state one video widget needs, kept across frames by iced.
#[derive(Debug)]
pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    /// Rebuilt when the stream's resolution changes, which happens when the
    /// client switches display or the host's own resolution changes.
    texture: Option<(Resolution, wgpu::Texture, wgpu::BindGroup)>,
    /// The generation already on the GPU.
    uploaded: Option<u64>,
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pravera video"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pravera video bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("pravera video pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pravera video pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vertex"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fragment"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // Opaque. The picture covers whatever is behind it; the
                    // letterbox bars are simply not drawn, so the container's
                    // background shows through there.
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pravera video sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            // Linear both ways. A remote desktop is almost never shown at
            // exactly 1:1, and nearest sampling at 97% scale shimmers on every
            // line of text as the window moves.
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pravera video uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        VideoPipeline {
            pipeline,
            layout,
            sampler,
            uniforms,
            texture: None,
            uploaded: None,
        }
    }
}

impl VideoPipeline {
    /// Make sure a texture of the right size exists, rebuilding if not.
    fn ensure_texture(&mut self, device: &wgpu::Device, resolution: Resolution) {
        if matches!(&self.texture, Some((existing, ..)) if *existing == resolution) {
            return;
        }

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pravera video frame"),
            size: wgpu::Extent3d {
                width: resolution.width,
                height: resolution.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Unorm, not Srgb. The decoder already produced display-ready
            // values, so asking the GPU to linearise them would wash the
            // picture out.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pravera video bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.uniforms.as_entire_binding(),
                },
            ],
        });

        self.texture = Some((resolution, texture, bind_group));
        // A new texture holds nothing, whatever was uploaded to the last one.
        self.uploaded = None;
    }
}

/// One frame's worth of drawing.
#[derive(Debug)]
pub struct VideoPrimitive {
    picture: Picture,
    /// The fraction of the widget the picture covers, per axis.
    scale: [f32; 2],
}

impl VideoPrimitive {
    pub fn new(picture: Picture, bounds: Rectangle) -> VideoPrimitive {
        let content = fit(bounds, picture.resolution);
        let scale = if bounds.width > 0.0 && bounds.height > 0.0 {
            [content.width / bounds.width, content.height / bounds.height]
        } else {
            [0.0, 0.0]
        };
        VideoPrimitive { picture, scale }
    }
}

impl Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        _viewport: &shader::Viewport,
    ) {
        if !self.picture.is_consistent() {
            // Refusing here rather than uploading a short buffer: a wgpu
            // validation error is not recoverable, and this data came from a
            // decoder fed by a peer.
            return;
        }

        pipeline.ensure_texture(device, self.picture.resolution);
        queue.write_buffer(
            &pipeline.uniforms,
            0,
            bytemuck::bytes_of(&Uniforms {
                scale: self.scale,
                _padding: [0.0; 2],
            }),
        );

        if pipeline.uploaded == Some(self.picture.generation) {
            return;
        }
        let Some((resolution, texture, _)) = &pipeline.texture else {
            return;
        };

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &self.picture.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(resolution.width * 4),
                rows_per_image: Some(resolution.height),
            },
            wgpu::Extent3d {
                width: resolution.width,
                height: resolution.height,
                depth_or_array_layers: 1,
            },
        );
        pipeline.uploaded = Some(self.picture.generation);
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some((_, _, bind_group)) = &pipeline.texture else {
            // Nothing has arrived yet. Returning `true` claims the draw
            // anyway, so iced does not fall back to `render` and open a second
            // pass to draw nothing in.
            return true;
        };
        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

/// The shader program backing the session view.
#[derive(Debug)]
pub struct Video {
    picture: Picture,
}

impl Video {
    pub fn new(picture: Picture) -> Video {
        Video { picture }
    }
}

impl<Message> shader::Program<Message> for Video {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _state: &(), _cursor: mouse::Cursor, bounds: Rectangle) -> VideoPrimitive {
        VideoPrimitive::new(self.picture.clone(), bounds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Resolution = Resolution::new(1920, 1080);

    fn widget(width: f32, height: f32) -> Rectangle {
        Rectangle {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }
    }

    #[test]
    fn a_picture_in_a_widget_of_its_own_shape_fills_it_exactly() {
        let content = fit(widget(960.0, 540.0), SCREEN);
        assert_eq!(content, widget(960.0, 540.0));
    }

    #[test]
    fn a_wider_widget_gets_bars_at_the_sides_not_a_stretched_picture() {
        // 16:9 inside 2:1. The picture keeps its shape and is centred.
        let content = fit(widget(1200.0, 600.0), SCREEN);

        assert_eq!(content.height, 600.0);
        assert!((content.width - 1066.6666).abs() < 0.01, "{content:?}");
        assert_eq!(content.y, 0.0);
        assert!((content.x - 66.666_7).abs() < 0.01, "{content:?}");

        let ratio = content.width / content.height;
        assert!(
            (ratio - 16.0 / 9.0).abs() < 1e-4,
            "the picture was stretched"
        );
    }

    #[test]
    fn a_taller_widget_gets_bars_above_and_below() {
        let content = fit(widget(960.0, 900.0), SCREEN);

        assert_eq!(content.width, 960.0);
        assert_eq!(content.height, 540.0);
        assert_eq!(content.y, 180.0);
        assert_eq!(content.x, 0.0);
    }

    #[test]
    fn the_widgets_own_offset_is_carried_into_the_content_rectangle() {
        // The widget is rarely at the window origin. Forgetting this puts the
        // picture in the corner of the window instead of the panel.
        let panel = Rectangle {
            x: 240.0,
            y: 64.0,
            width: 1200.0,
            height: 600.0,
        };
        let content = fit(panel, SCREEN);

        assert!(content.x > panel.x);
        assert_eq!(content.y, panel.y);
        assert!(content.x + content.width <= panel.x + panel.width + 0.01);
    }

    #[test]
    fn a_zero_sized_widget_produces_nothing_rather_than_dividing_by_zero() {
        // A pane genuinely measures zero for a frame while it animates open.
        assert_eq!(fit(widget(0.0, 500.0), SCREEN).width, 0.0);
        assert_eq!(fit(widget(500.0, 0.0), SCREEN).height, 0.0);
        assert_eq!(fit(widget(500.0, 500.0), Resolution::new(0, 0)).width, 0.0);
    }

    #[test]
    fn the_corners_of_the_picture_are_the_corners_of_the_remote_screen() {
        let bounds = widget(1200.0, 600.0);
        let content = fit(bounds, SCREEN);

        let top_left = point_in(bounds, SCREEN, iced::Point::new(content.x, content.y));
        assert_eq!(top_left, Some((0.0, 0.0)));

        let bottom_right = point_in(
            bounds,
            SCREEN,
            iced::Point::new(
                content.x + content.width - 0.001,
                content.y + content.height - 0.001,
            ),
        );
        let (x, y) = bottom_right.expect("the bottom-right corner is on the picture");
        assert!(x > 0.999 && y > 0.999, "({x}, {y})");
    }

    #[test]
    fn the_centre_of_the_picture_is_the_centre_of_the_remote_screen() {
        let bounds = widget(1200.0, 600.0);
        let centre = iced::Point::new(
            bounds.x + bounds.width / 2.0,
            bounds.y + bounds.height / 2.0,
        );

        let (x, y) = point_in(bounds, SCREEN, centre).expect("the centre is on the picture");
        assert!((x - 0.5).abs() < 1e-5, "{x}");
        assert!((y - 0.5).abs() < 1e-5, "{y}");
    }

    #[test]
    fn a_click_in_the_letterbox_maps_nowhere() {
        // There is no part of the remote desktop under a black bar. Clamping
        // to the edge would move the pointer somewhere nobody pointed.
        let bounds = widget(1200.0, 600.0);
        assert_eq!(point_in(bounds, SCREEN, iced::Point::new(4.0, 300.0)), None);
        assert_eq!(
            point_in(bounds, SCREEN, iced::Point::new(1196.0, 300.0)),
            None
        );
    }

    #[test]
    fn a_point_outside_the_widget_maps_nowhere() {
        let bounds = Rectangle {
            x: 100.0,
            y: 100.0,
            width: 400.0,
            height: 225.0,
        };
        assert_eq!(
            point_in(bounds, SCREEN, iced::Point::new(50.0, 150.0)),
            None
        );
        assert_eq!(
            point_in(bounds, SCREEN, iced::Point::new(300.0, 90.0)),
            None
        );
    }

    #[test]
    fn a_frame_whose_buffer_does_not_match_its_size_is_refused() {
        // Uploading a short buffer is a wgpu validation error, and it would be
        // reached with data a peer supplied.
        let honest = Picture::new(Resolution::new(4, 2), vec![0; 4 * 2 * 4], 1);
        assert!(honest.is_consistent());

        let short = Picture::new(Resolution::new(4, 2), vec![0; 8], 1);
        assert!(!short.is_consistent());

        let empty = Picture::new(Resolution::new(0, 0), Vec::new(), 1);
        assert!(!empty.is_consistent());
    }

    #[test]
    fn the_scale_the_shader_gets_matches_the_rectangle_the_pointer_uses() {
        // The bug this prevents is a cursor that is consistently a few pixels
        // off: the picture drawn in one place and the pointer measured against
        // another.
        let bounds = widget(1200.0, 600.0);
        let picture = Picture::new(SCREEN, vec![0; 1920 * 1080 * 4], 1);
        let primitive = VideoPrimitive::new(picture, bounds);

        let content = fit(bounds, SCREEN);
        assert!((primitive.scale[0] - content.width / bounds.width).abs() < 1e-6);
        assert!((primitive.scale[1] - content.height / bounds.height).abs() < 1e-6);
        assert!(primitive.scale[1] >= 0.999, "the taller axis should fill");
    }

    #[test]
    fn cloning_a_picture_does_not_copy_the_pixels() {
        // Every redraw rebuilds the primitive. If this copied, a hover
        // animation would cost a full-frame memcpy per tick.
        let picture = Picture::new(Resolution::new(64, 64), vec![7; 64 * 64 * 4], 3);
        let copy = picture.clone();
        assert!(Arc::ptr_eq(&picture.pixels, &copy.pixels));
    }
}

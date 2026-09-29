//! A live backdrop blur — the iced equivalent of CSS `backdrop-filter: blur()`.
//!
//! Every earlier attempt here copied pixels: GDI `BitBlt` ghosted (chrome +
//! offset), and `window::screenshot` duplicated the UI (a snapshot is one
//! frame stale, drawn into a logical-size area from physical pixels, and it
//! carries the window-fade alpha, so the sharp UI and the blurred copy both
//! showed). A backdrop-filter is not a copy at all: it samples the *live*
//! framebuffer behind the element, every frame, on the GPU.
//!
//! That needs two things iced 0.14 does not hand out by default, and both are
//! in place:
//!
//! 1. The window surface must be samplable. Stock iced_wgpu configures it
//!    `RENDER_ATTACHMENT`-only; the vendored copy in `third_party/iced_wgpu`
//!    adds `TEXTURE_BINDING` (one flagged line in `window/compositor.rs`).
//! 2. A primitive that renders *after* everything behind it. `draw()`
//!    returning `false` makes iced end its main render pass and call
//!    [`Primitive::render`] with the raw encoder and the frame view. Stack
//!    children render on their own layers, so at that moment the frame holds
//!    exactly the content behind the scrim; the dialog renders later, on top.
//!
//! The blur is progressive: several separable Gaussian passes (horizontal,
//! vertical) with a growing radius, ping-ponging between two intermediate
//! textures, then a composite that writes the result back over the frame
//! region with the dim tint. Small spacing first is what keeps it smooth —
//! one wide pass samples every `radius` pixels and the unsampled gaps come
//! through as squares — while the later wide passes only ever read data the
//! early ones already smoothed, so their bigger steps cannot show. The
//! intermediates are also written one tap-reach past the scrim: otherwise
//! the first pass's taps above the scrim's top edge would sample cleared
//! transparent pixels and the composite would write a strip the window shows
//! straight through (the hole under the drag bar). The frame is never copied
//! to the CPU and nothing can go stale or misaligned.

use std::sync::Mutex;

use iced::widget::shader::{self, Primitive};
use iced::{mouse, Rectangle};

/// Tap spacing per pass, physical pixels, one entry per blur iteration.
/// Ascending on purpose: early small steps keep the result gapless, late
/// wide steps run over already-smooth data where they cannot mosaic.
const RADII_PX: [f32; 4] = [2.0, 3.0, 5.0, 8.0];
/// How far taps reach past the scrim, summed over the whole chain. Every
/// blur pass writes this far beyond the scrim (clamped to the frame) because
/// pollution propagates: a cleared transparent pixel within reach of any
/// pass eventually blends into the composite — that was the translucent
/// strip under the drag bar. Slightly over-wide is harmless; the composite
/// still confines itself to the scrim.
const fn total(radii: &[f32; RADII_PX.len()]) -> f32 {
    let mut sum = 0.0;
    let mut i = 0;
    while i < radii.len() {
        sum += radii[i];
        i += 1;
    }
    sum
}
const REACH_PX: f32 = 4.0 * total(&RADII_PX);
/// How much the blur is dimmed: deep enough that the dialog is the only thing
/// competing for attention, light enough that the page behind stays legible.
const TINT: f32 = 0.5;

/// The two intermediate targets of the separable blur. Owned by the
/// pipeline; rebuilt on resize in `prepare`.
#[derive(Debug)]
struct Temps {
    a_view: wgpu::TextureView,
    b_view: wgpu::TextureView,
    size: (u32, u32),
}

/// Everything the GPU needs, behind a lock because [`Primitive::render`]
/// hands out `&Pipeline` while bind groups are built there — the frame view
/// first exists when `render` is called, and `render` receives no device, so
/// the device is kept here. The handful of bind groups built per frame are
/// handle allocations, not uploads.
#[derive(Debug)]
struct Inner {
    device: wgpu::Device,
    /// Blur passes write whole texels into their own cleared targets.
    blur_pipeline: wgpu::RenderPipeline,
    /// The composite pass blends: the fragment's alpha is the scrim's fade
    /// strength, so `blur·strength + frame·(1−strength)` happens in blending
    /// and the fade is a uniform, not a re-render.
    composite_pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Two per blur iteration (horizontal, vertical) plus the composite.
    uniforms: Vec<wgpu::Buffer>,
    format: wgpu::TextureFormat,
    temps: Option<Temps>,
}

impl Inner {
    /// Make sure the intermediate targets match the frame size.
    fn ensure_temps(&mut self, size: (u32, u32)) {
        if matches!(&self.temps, Some(temps) if temps.size == size) {
            return;
        }
        let make = |label: &'static str| {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                // Same format as the frame: no conversion, and the composite
                // pass writes values the surface already understands.
                format: self.format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            texture.create_view(&wgpu::TextureViewDescriptor::default())
        };
        self.temps = Some(Temps {
            a_view: make("pravera backdrop blur a"),
            b_view: make("pravera backdrop blur b"),
            size,
        });
    }

    /// Bind `view` as the pass input, driven by uniform `index`.
    fn bind_input(&self, view: &wgpu::TextureView, uniform: &wgpu::Buffer) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pravera backdrop blur bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        })
    }

    /// One blur-or-composite pass. `target` is written, `input` is sampled
    /// through `bind_group`. The quad covers the whole frame; the scissor
    /// confines it to `region`.
    fn pass(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        label: &'static str,
        pipeline: &wgpu::RenderPipeline,
        target: &wgpu::TextureView,
        load: wgpu::LoadOp<wgpu::Color>,
        bind_group: &wgpu::BindGroup,
        region: Rectangle<u32>,
    ) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_scissor_rect(region.x, region.y, region.width, region.height);
        pass.draw(0..3, 0..1);
    }
}

/// Shared GPU state for every backdrop primitive; created once per renderer.
#[derive(Debug)]
pub struct BackdropPipeline {
    inner: Mutex<Inner>,
}

impl shader::Pipeline for BackdropPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pravera backdrop blur"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pravera backdrop blur layout"),
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
                    visibility: wgpu::ShaderStages::FRAGMENT,
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
            label: Some("pravera backdrop blur pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });

        let targets = |blend: wgpu::BlendState| {
            [Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend),
                write_mask: wgpu::ColorWrites::ALL,
            })]
        };
        let blur_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pravera backdrop blur pipeline"),
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
                // Replace: the blurred frame *is* the region's new content.
                targets: &targets(wgpu::BlendState::REPLACE),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let composite_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("pravera backdrop blur composite pipeline"),
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
                // Color blends by the fragment's alpha — the fade strength —
                // over the frame already on the surface. Alpha itself is left
                // untouched (`Zero, One`): the window-fade animation owns it,
                // and the composite must not stomp it to the fade value and
                // punch a hole in the window mid-animation.
                targets: &targets(wgpu::BlendState {
                    color: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::SrcAlpha,
                        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                        operation: wgpu::BlendOperation::Add,
                    },
                    alpha: wgpu::BlendComponent {
                        src_factor: wgpu::BlendFactor::Zero,
                        dst_factor: wgpu::BlendFactor::One,
                        operation: wgpu::BlendOperation::Add,
                    },
                }),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pravera backdrop blur sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = (0..RADII_PX.len() * 2 + 1)
            .map(|_| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("pravera backdrop blur uniforms"),
                    size: std::mem::size_of::<Params>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                })
            })
            .collect();

        BackdropPipeline {
            inner: Mutex::new(Inner {
                device: device.clone(),
                blur_pipeline,
                composite_pipeline,
                layout,
                sampler,
                uniforms,
                format,
                temps: None,
            }),
        }
    }
}

/// What one pass is told on the GPU. Two vec4s keep the layout boring.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    /// `1 / frame size`, turning pixel offsets into UV steps.
    texel: [f32; 2],
    /// Tap direction: `(1, 0)` horizontal, `(0, 1)` vertical.
    direction: [f32; 2],
    radius: f32,
    tint: f32,
    /// 0 and 1 = Gaussian along `direction`; 2 = composite with the tint.
    mode: u32,
    /// Composite only: the scrim's fade strength, consumed by blending.
    fade: f32,
}

const SHADER: &str = r#"
struct Params {
    texel: vec2<f32>,
    direction: vec2<f32>,
    radius: f32,
    tint: f32,
    mode: u32,
    fade: f32,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<uniform> params: Params;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Full-screen triangle, same shape as the video widget's.
@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOut {
    var out: VertexOut;
    let x = f32((index << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(index & 2u) * 2.0 - 1.0;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}

// Gaussian tap at `offset` pixels. Sigma is 2× the tap spacing so the nine
// taps (±4 spacings) still sit inside the kernel's tail.
fn tap(offset: f32) -> f32 {
    let sigma = params.radius * 2.0;
    return exp(-0.5 * (offset * offset) / (sigma * sigma));
}

@fragment
fn fragment(in: VertexOut) -> @location(0) vec4<f32> {
    if params.mode == 2u {
        // Composite: dim, and hand the blend factor to the pipeline — the
        // fragment alpha is the fade strength, so the blurred result eases
        // in and out over the frame already on the surface.
        let c = textureSample(src, samp, in.uv);
        return vec4<f32>(c.rgb * (1.0 - params.tint), params.fade);
    }

    var sum = vec3<f32>(0.0);
    var sum_a = 0.0;
    var weight_sum = 0.0;
    for (var i = -4i; i <= 4i; i++) {
        let offset = f32(i) * params.radius;
        let uv = in.uv + params.direction * offset * params.texel;
        let c = textureSample(src, samp, uv);
        // Premultiplied blur: dark pixels next to bright ones must not bleed
        // through their alpha, or text fringes come out as halos.
        let weight = tap(offset);
        sum += c.rgb * c.a * weight;
        sum_a += c.a * weight;
        weight_sum += weight;
    }
    let a = sum_a / weight_sum;
    let rgb = select(vec3<f32>(0.0), sum / weight_sum / max(a, 0.0001), a > 0.0001);
    return vec4<f32>(rgb, a);
}
"#;

/// One draw call's worth of strength — the blur itself lives in the pipeline.
#[derive(Debug, Clone, Copy)]
pub struct BackdropPrimitive {
    /// 0 = the frame passes through untouched, 1 = fully frosted. The fade
    /// in and out around a dialog rides on this.
    pub strength: f32,
}

impl Primitive for BackdropPrimitive {
    type Pipeline = BackdropPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        _device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &Rectangle,
        viewport: &shader::Viewport,
    ) {
        let physical = viewport.physical_size();
        let size = (physical.width.max(1), physical.height.max(1));
        let mut inner = pipeline.inner.lock().expect("backdrop blur lock");

        let texel = [1.0 / size.0 as f32, 1.0 / size.1 as f32];
        // Horizontal and vertical params per iteration, then the composite.
        for (index, radius) in RADII_PX.iter().enumerate() {
            let horizontal = Params {
                texel,
                direction: [1.0, 0.0],
                radius: *radius,
                tint: TINT,
                mode: 0,
                fade: 0.0,
            };
            let vertical = Params {
                direction: [0.0, 1.0],
                mode: 1,
                ..horizontal
            };
            queue.write_buffer(&inner.uniforms[index * 2], 0, bytemuck::bytes_of(&horizontal));
            queue.write_buffer(
                &inner.uniforms[index * 2 + 1],
                0,
                bytemuck::bytes_of(&vertical),
            );
        }
        let composite = Params {
            texel,
            direction: [1.0, 0.0],
            radius: RADII_PX[0],
            tint: TINT,
            mode: 2,
            fade: self.strength.clamp(0.0, 1.0),
        };
        queue.write_buffer(
            inner.uniforms.last().expect("composite uniform"),
            0,
            bytemuck::bytes_of(&composite),
        );

        inner.ensure_temps(size);
    }

    fn draw(&self, _pipeline: &Self::Pipeline, _render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        // Always take the `render` fallback: sampling the frame is illegal
        // inside the main pass it is an attachment of, and the fallback is
        // the one place iced ends that pass and hands us the encoder.
        false
    }

    fn render(
        &self,
        pipeline: &Self::Pipeline,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        clip_bounds: &Rectangle<u32>,
    ) {
        let inner = pipeline.inner.lock().expect("backdrop blur lock");
        if self.strength <= 0.001 {
            // Fully faded out: the frame already holds exactly what should be
            // on it. Running the passes anyway would cost a dozen draws to
            // blend nothing.
            return;
        }
        let clip = Rectangle {
            x: clip_bounds.x,
            y: clip_bounds.y,
            width: clip_bounds.width.max(1),
            height: clip_bounds.height.max(1),
        };

        let Some(temps) = &inner.temps else {
            return;
        };

        // Sample the live frame. Legal only because the main render pass has
        // ended (iced ended it on seeing `draw() -> false`) and the surface
        // carries TEXTURE_BINDING — the one-line vendored iced_wgpu patch.
        let mut input = inner.bind_input(target, &inner.uniforms[0]);

        // Each iteration reads the previous output and ping-pongs A → B. All
        // blur passes write `REACH_PX` past the scrim — clamped to the frame
        // — so every pixel the composite can reach traces back to real frame
        // content rather than a cleared transparent border.
        let reach = REACH_PX.ceil() as u32;
        let x0 = clip.x.saturating_sub(reach);
        let y0 = clip.y.saturating_sub(reach);
        let x1 = (clip.x + clip.width + reach).min(temps.size.0);
        let y1 = (clip.y + clip.height + reach).min(temps.size.1);
        let expanded = Rectangle {
            x: x0,
            y: y0,
            width: (x1 - x0).max(1),
            height: (y1 - y0).max(1),
        };

        for index in 0..RADII_PX.len() {
            inner.pass(
                encoder,
                "pravera backdrop blur h pass",
                &inner.blur_pipeline,
                &temps.a_view,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                &input,
                expanded,
            );
            let vertical_input = inner.bind_input(&temps.a_view, &inner.uniforms[index * 2 + 1]);
            inner.pass(
                encoder,
                "pravera backdrop blur v pass",
                &inner.blur_pipeline,
                &temps.b_view,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                &vertical_input,
                expanded,
            );
            input = inner.bind_input(&temps.b_view, &inner.uniforms[(index + 1) * 2]);
        }

        // Composite back onto the frame, confined to the scrim itself. That
        // pass loads: everything already on the frame outside the scrim must
        // survive; inside it, the blurred pixels cover it all.
        let composite_input = inner.bind_input(&temps.b_view, inner.uniforms.last().unwrap());
        inner.pass(
            encoder,
            "pravera backdrop blur composite pass",
            &inner.composite_pipeline,
            target,
            wgpu::LoadOp::Load,
            &composite_input,
            clip,
        );
    }
}

/// The scrim widget: blurs whatever is rendered behind it, live, fading with
/// `strength`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Backdrop {
    pub strength: f32,
}

impl Backdrop {
    pub fn new(strength: f32) -> Self {
        Backdrop {
            strength: strength.clamp(0.0, 1.0),
        }
    }
}

impl<Message> shader::Program<Message> for Backdrop {
    type State = ();
    type Primitive = BackdropPrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        BackdropPrimitive {
            strength: self.strength,
        }
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        // The scrim sits over the shell, and a `Stack` takes the cursor from
        // the first layer that reports one. Leaving the default `None` let
        // the hand cursor of buttons *behind* the blur leak through; the
        // scrim is frosted glass, not a button, so it answers with the plain
        // arrow. Hover highlights behind it still come through — the stack
        // forwards the pointer to every layer regardless.
        mouse::Interaction::Idle
    }
}

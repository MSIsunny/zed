#![cfg_attr(target_family = "wasm", no_main)]
//! Demonstrates the `gpui_wgpu` external compositor path: a backend-specific
//! renderer (here, a tiny WGSL render pass standing in for e.g. a wgpu-based 3D
//! renderer) that GPUI composites into a rectangular region of its own scene, at a
//! controlled point in its own frame (see
//! `crates/gpui/src/external_compositor.rs` and
//! `crates/gpui_wgpu/src/wgpu_renderer.rs`).
//!
//! Not available on wasm: the canvas backend (`WgpuRenderer::new_from_canvas`) never
//! wires up an `ExternalCompositorRegistry`, so `Window::external_compositor_registry`
//! always returns `None` there; this example just shows a "no registry" status label
//! in that case instead of erroring out (the color set via
//! `ExternalCompositorElement::background` covers app-visible degradation on
//! backends without support — see that element's docs).
//!
//! Run with `cargo run -p gpui --example external_compositor`. Set
//! `EXTERNAL_COMPOSITOR_EXIT_AFTER=<n>` (a frame count) to have the example close
//! itself and exit with status `0` after `n` rendered frames — used to smoke-test it
//! headlessly (e.g. in CI), without a human watching the window.

use gpui::{
    AlphaMode, App, Bounds, Context, ExternalSlotDescriptor, ExternalSlotFormat,
    ExternalSlotHandle, Render, SharedString, Window, WindowBounds, WindowOptions, div,
    external_compositor, prelude::*, px, rgb, rgba, size,
};
use gpui_platform::application;
#[cfg(not(target_family = "wasm"))]
use gpui_wgpu::{
    ExternalComposeOutput, WgpuCompositorBackendCtx, WgpuExternalCompositor,
    register_external_compositor, wgpu,
};
use std::sync::Arc;

const DISPLAY_SIZE: f32 = 256.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct DemoTextureSize {
    width: u32,
    height: u32,
}

impl DemoTextureSize {
    fn for_window(window: &Window) -> Self {
        let scale_factor = window.scale_factor();
        Self {
            width: logical_pixels_to_device_texels(DISPLAY_SIZE, scale_factor),
            height: logical_pixels_to_device_texels(DISPLAY_SIZE, scale_factor),
        }
    }
}

fn logical_pixels_to_device_texels(logical_pixels: f32, scale_factor: f32) -> u32 {
    (logical_pixels * scale_factor).ceil().max(1.0) as u32
}

/// Reads `EXTERNAL_COMPOSITOR_EXIT_AFTER` once at startup.
fn exit_after_frames() -> Option<u64> {
    std::env::var("EXTERNAL_COMPOSITOR_EXIT_AFTER")
        .ok()
        .and_then(|value| value.parse().ok())
}

/// A minimal [`WgpuExternalCompositor`]: on its first `compose` call it creates a
/// `Rgba8UnormSrgb` render target sized in device texels plus a tiny WGSL pipeline.
/// Each `compose` call renders a fresh animated frame into that texture, proving
/// GPUI is sampling a live externally-rendered wgpu texture rather than a CPU-filled
/// image.
#[cfg(not(target_family = "wasm"))]
struct DemoCompositor {
    texture_size: DemoTextureSize,
    resources: Option<DemoResources>,
}

#[cfg(not(target_family = "wasm"))]
struct DemoResources {
    _texture: wgpu::Texture,
    view: Arc<wgpu::TextureView>,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
}

#[cfg(not(target_family = "wasm"))]
impl DemoCompositor {
    fn new(texture_size: DemoTextureSize) -> Self {
        Self {
            texture_size,
            resources: None,
        }
    }

    fn create_resources(&self, ctx: &mut WgpuCompositorBackendCtx<'_>) -> DemoResources {
        let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("external_compositor_example_demo_texture"),
            size: wgpu::Extent3d {
                width: self.texture_size.width,
                height: self.texture_size.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = Arc::new(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let uniform_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("external_compositor_example_uniform_buffer"),
            size: 16,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: false,
        });
        let bind_group_layout =
            ctx.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("external_compositor_example_bind_group_layout"),
                    entries: &[wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    }],
                });
        let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("external_compositor_example_bind_group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });
        let shader = ctx
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("external_compositor_example_shader"),
                source: wgpu::ShaderSource::Wgsl(
                    r#"
struct Params {
    frame: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0)
var<uniform> params: Params;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(3.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    let position = positions[vertex_index];
    var output: VertexOutput;
    output.position = vec4<f32>(position, 0.0, 1.0);
    output.uv = position * vec2<f32>(0.5, -0.5) + vec2<f32>(0.5, 0.5);
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let pixel = vec2<u32>(floor(input.position.xy));
    let cell_size = select(
        select(4u, 8u, input.uv.x >= 0.5),
        select(1u, 2u, input.uv.x >= 0.5),
        input.uv.y < 0.5,
    );
    let checker = ((pixel.x / cell_size + pixel.y / cell_size) & 1u) == 0u;
    var color = select(vec3<f32>(0.02, 0.02, 0.02), vec3<f32>(0.94, 0.94, 0.94), checker);

    if (input.uv.x >= 0.5 && input.uv.y >= 0.5) {
        color = vec3<f32>(input.uv.x, input.uv.y, 0.22);
        if ((pixel.x % 8u) == 0u || (pixel.y % 8u) == 0u) {
            color = vec3<f32>(0.0, 0.0, 0.0);
        }
    }

    if ((pixel.x % 64u) == 0u || (pixel.y % 64u) == 0u) {
        color = vec3<f32>(0.0, 0.45, 1.0);
    }

    let moving_offset = i32(u32(params.frame) % 128u) - 64;
    let diagonal_distance = abs(i32(pixel.x) - i32(pixel.y) - moving_offset);
    if (diagonal_distance == 0) {
        color = vec3<f32>(1.0, 0.95, 0.0);
    }

    if (pixel.x == 0u || pixel.y == 0u) {
        color = vec3<f32>(1.0, 0.0, 0.0);
    }

    return vec4<f32>(color, 1.0);
}
"#
                    .into(),
                ),
            });
        let pipeline_layout = ctx
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("external_compositor_example_pipeline_layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                immediate_size: 0,
            });
        let pipeline = ctx
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("external_compositor_example_pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        DemoResources {
            _texture: texture,
            view,
            pipeline,
            bind_group,
            uniform_buffer,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl WgpuExternalCompositor for DemoCompositor {
    fn compose(
        &mut self,
        _slot: ExternalSlotHandle,
        ctx: &mut WgpuCompositorBackendCtx<'_>,
    ) -> ExternalComposeOutput {
        if self.resources.is_none() {
            log::info!(
                "external_compositor example: creating {}x{} demo wgpu \
                 render target (swapchain target format {:?}, context generation {})",
                self.texture_size.width,
                self.texture_size.height,
                ctx.target_format,
                ctx.context_generation,
            );
            self.resources = Some(self.create_resources(ctx));
        }
        let Some(resources) = self.resources.as_ref() else {
            return ExternalComposeOutput::NotReady;
        };

        let params = [ctx.frame_index as f32, 0.0, 0.0, 0.0];
        let params_bytes = unsafe {
            std::slice::from_raw_parts(params.as_ptr() as *const u8, std::mem::size_of_val(&params))
        };
        ctx.queue
            .write_buffer(&resources.uniform_buffer, 0, params_bytes);

        {
            let mut pass = ctx.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("external_compositor_example_render_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &resources.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                ..Default::default()
            });
            pass.set_pipeline(&resources.pipeline);
            pass.set_bind_group(0, &resources.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        ExternalComposeOutput::Ready {
            view: resources.view.clone(),
        }
    }

    fn on_context_recreated(&mut self, new_generation: u64) {
        // The device this texture lived on is gone: drop it. The example's
        // per-frame poll (see `ExternalCompositorDemo::render`) will notice the
        // handle went stale via `ExternalCompositorRegistry::is_valid`, clean it up,
        // and register a fresh `DemoCompositor` under `new_generation`; that fresh
        // instance will lazily recreate the texture on its own first `compose`.
        log::warn!(
            "external_compositor example: graphics context recreated (generation \
             {new_generation}); dropping demo texture"
        );
        self.resources = None;
    }
}

struct ExternalCompositorDemo {
    handle: Option<ExternalSlotHandle>,
    texture_size: Option<DemoTextureSize>,
    frames_rendered: u64,
    exit_after: Option<u64>,
    status: SharedString,
}

impl ExternalCompositorDemo {
    fn new() -> Self {
        Self {
            handle: None,
            texture_size: None,
            frames_rendered: 0,
            exit_after: exit_after_frames(),
            status: "waiting for external compositor registry".into(),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn ensure_registered(&mut self, window: &mut Window) {
        let Some(registry) = window.external_compositor_registry() else {
            self.status = "this platform backend has no external compositor registry".into();
            return;
        };

        let texture_size = DemoTextureSize::for_window(window);
        let needs_register = match self.handle {
            Some(handle) => {
                !registry.borrow().is_valid(handle) || self.texture_size != Some(texture_size)
            }
            None => true,
        };
        if !needs_register {
            return;
        }

        if let Some(old_handle) = self.handle.take() {
            self.texture_size = None;
            // The previous handle went stale (a graphics context recreation
            // happened): this is `unregister`'s context-recreation cleanup case —
            // it frees the slot immediately, no frame-in-flight deferral, since a
            // stale slot is never composed.
            if let Err(error) = registry.borrow_mut().unregister(old_handle) {
                log::debug!("external_compositor example: stale slot cleanup: {error}");
            }
        }

        let generation = registry.borrow().current_context_generation();
        if let Some(specs) = window.gpu_specs() {
            log::info!(
                "external_compositor example: gpu = {} ({}), software_emulated = {}",
                specs.device_name,
                specs.driver_name,
                specs.is_software_emulated,
            );
        }
        log::info!(
            "external_compositor example: registering demo compositor under context \
             generation {generation} with {}x{} device texels at {:.2}x scale",
            texture_size.width,
            texture_size.height,
            window.scale_factor(),
        );

        let descriptor = ExternalSlotDescriptor {
            format: ExternalSlotFormat::Rgba8UnormSrgb,
            alpha_mode: AlphaMode::Straight,
            width: texture_size.width,
            height: texture_size.height,
            sample_count: 1,
            context_generation: generation,
        };
        match register_external_compositor(&registry, descriptor, DemoCompositor::new(texture_size))
        {
            Ok(handle) => {
                self.handle = Some(handle);
                self.texture_size = Some(texture_size);
                self.status = format!(
                    "compositor registered — {}x{} texels @ {:.2}x",
                    texture_size.width,
                    texture_size.height,
                    window.scale_factor(),
                )
                .into();
            }
            Err(error) => {
                self.status = format!("registration failed: {error}").into();
                log::error!("external_compositor example: {error}");
            }
        }
    }

    #[cfg(target_family = "wasm")]
    fn ensure_registered(&mut self, _window: &mut Window) {
        self.status = "external composition is not available on wasm".into();
    }
}

impl Render for ExternalCompositorDemo {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.frames_rendered += 1;
        self.ensure_registered(window);

        if let Some(exit_after) = self.exit_after
            && self.frames_rendered >= exit_after
        {
            log::info!(
                "external_compositor example: exiting after {} frames \
                 (EXTERNAL_COMPOSITOR_EXIT_AFTER={exit_after})",
                self.frames_rendered
            );
            std::process::exit(0);
        }

        // Keep redrawing every frame: the demo compositor's texture animates by
        // `WgpuCompositorBackendCtx::frame_index`, and this is also how the example
        // keeps polling `ExternalCompositorRegistry::is_valid` to notice context
        // recreation promptly.
        window.request_animation_frame();

        let content = if let Some(handle) = self.handle {
            div()
                .w(px(DISPLAY_SIZE))
                .h(px(DISPLAY_SIZE))
                .rounded(px(20.0))
                .overflow_hidden()
                .border_1()
                .border_color(gpui::white())
                .child(
                    external_compositor(handle)
                        .size_full()
                        .background(rgba(0x1a1a1aff)),
                )
        } else {
            div()
                .w(px(DISPLAY_SIZE))
                .h(px(DISPLAY_SIZE))
                .rounded(px(20.0))
                .border_1()
                .border_color(gpui::white())
                .bg(gpui::black())
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .size_full()
            .bg(rgb(0x1e1e1e))
            .p_4()
            .text_color(gpui::white())
            .child(format!(
                "external compositor demo — frame {} — {}",
                self.frames_rendered, self.status
            ))
            .child(div().flex().justify_center().items_center().child(content))
    }
}

fn run_example() {
    application().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(420.0), px(420.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ExternalCompositorDemo::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .filter_module("gpui", log::LevelFilter::Info)
        .filter_module("gpui_wgpu", log::LevelFilter::Info)
        .filter_module("external_compositor", log::LevelFilter::Info)
        .init();
    run_example();
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    gpui_platform::web_init();
    run_example();
}

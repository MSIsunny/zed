# Windows External WGPU Compositor Plan

This document records the current macOS implementation lessons and the proposed
Windows path for compositing external `wgpu` textures into GPUI. The intended
Windows route is DX12-only at the bottom:

```text
external renderer
  -> wgpu Device/Queue using the DX12 backend
  -> wgpu TextureView
  -> wgpu-hal dx12 ID3D12Resource
  -> D3D11On12 wrapped resource
  -> existing GPUI Windows D3D11-style sampling pipeline
```

The goal is to reuse the existing GPUI external compositor API and the existing
Windows renderer shape while ensuring that the real device/queue/resource owner
is DX12.

## Current State

The cross-platform GPUI layer is already mostly ready:

- `crates/gpui/src/external_compositor.rs` owns the backend-neutral slot
  registry, descriptors, context generation, resize, removal deferral, and stale
  handle behavior.
- `crates/gpui/src/elements/external_compositor.rs` turns a slot handle into a
  scene primitive.
- `crates/gpui/src/scene.rs` stores `ExternalCompositorPrimitive` and batches it
  as `PrimitiveBatch::ExternalCompositors`.
- `crates/gpui_wgpu/src/wgpu_renderer.rs` defines the public compositor trait:
  `WgpuExternalCompositor`, `WgpuCompositorBackendCtx`, and
  `register_external_compositor`.
- `crates/gpui/examples/external_compositor.rs` is the smoke example. It already
  uses device texels, `slot_descriptor`, dynamic resize, and `context_generation`.

The macOS platform path is implemented:

- `crates/gpui_macos/src/window.rs` stores an
  `Rc<RefCell<ExternalCompositorRegistry>>` on each window and exposes it through
  `PlatformWindow::external_compositor_registry`.
- `crates/gpui_macos/src/metal_renderer.rs` composes every distinct external
  slot once per frame with one `wgpu::CommandEncoder`, one submit, and one
  frame-level synchronization point.
- The macOS path validates that the external `wgpu` Metal device and the GPUI
  Metal renderer use the same `MTLDevice`.
- The macOS path retains the returned `Arc<wgpu::TextureView>` until the GPUI
  Metal command buffer completion handler runs, so the sampled texture cannot be
  dropped while the GPU still needs it.
- Headless and `render_to_image` paths pass an empty external compositor outcome
  map, so test-support rendering does not depend on a live platform registry.

The Windows platform path is not implemented yet:

- `crates/gpui_windows/src/window.rs` does not store or expose an
  `ExternalCompositorRegistry`.
- `crates/gpui_windows/src/directx_renderer.rs` currently skips
  `PrimitiveBatch::ExternalCompositors`.
- `crates/gpui_windows` currently uses a D3D11 renderer and does not enable the
  `windows` crate features for D3D12 or D3D11On12.
- `crates/gpui_wgpu/src/external_context.rs` has a Windows
  `from_d3d12_device` placeholder, but the recommended route below does not need
  to start from an existing D3D12 device. Instead, `wgpu` should create the DX12
  device and GPUI should derive its D3D11On12 device from that.

## Lessons From macOS

Keep these properties when implementing Windows:

1. The public API should stay backend-neutral.

   Applications should keep using `Window::external_compositor_registry`,
   `register_external_compositor`, and `external_compositor(handle)`. Windows
   should not introduce a separate public element or registration API.

2. Slot dimensions are device texels.

   High-DPI correctness depends on `ExternalSlotDescriptor.width` and `height`
   being the compositor render-target size in physical device texels. The example
   should continue deriving these values from `window.scale_factor()`, and the
   compositor should recreate size-dependent resources when
   `ctx.slot_descriptor` changes.

3. Compose once per distinct slot per frame.

   The renderer should deduplicate slot handles in the current scene, call
   `compose` once per distinct handle, and then draw all primitives that reference
   that handle from the same composed output.

4. Avoid per-slot CPU waits.

   macOS currently has one frame-level wait because the `wgpu` queue and GPUI
   Metal command buffer are separate. On Windows, if D3D11On12 uses the same
   underlying `ID3D12CommandQueue` as the `wgpu` DX12 backend, GPU queue ordering
   should allow a better steady-state path: submit `wgpu` compose work first,
   then flush the D3D11On12 draw work to the same queue, with no per-slot CPU
   wait.

5. Validate device identity.

   macOS compares raw `MTLDevice` pointers. Windows should similarly validate
   that the D3D11On12 device was created from the same `ID3D12Device` and
   `ID3D12CommandQueue` exposed by the `wgpu` DX12 device/queue.

6. Tie texture lifetime to GPU completion.

   The returned `Arc<wgpu::TextureView>` must stay alive until the GPUI sampling
   work has completed on the GPU. macOS uses the Metal command buffer completion
   handler. Windows should use a DX12 fence or a small frame-retention queue keyed
   by fence values.

7. Treat context recreation as a slot-generation boundary.

   Device lost or backend context recreation should invalidate live slots through
   the registry generation mechanism. App code should observe stale handles and
   re-register.

## Why WGPU-Owns-DX12 Is the Preferred Windows Route

Do not create an independent DX12 device for GPUI and another independent DX12
device for `wgpu`. A `wgpu` texture's `ID3D12Resource` cannot simply be sampled
from a different D3D12 device, even if both devices target the same physical GPU.
That route would require shared handles, import/export, and explicit
cross-device synchronization.

The cleaner route is:

1. Create the `wgpu::Device` and `wgpu::Queue` with `Backends::DX12`.
2. Use `Device::as_hal::<wgpu::hal::api::Dx12>()` to access the hal DX12 device.
3. Use `Queue::as_hal::<wgpu::hal::api::Dx12>()` to access the hal DX12 queue.
4. Extract the raw `ID3D12Device` and `ID3D12CommandQueue`.
5. Create the Windows renderer's D3D11 device via `D3D11On12CreateDevice` from
   that same raw D3D12 device and command queue.

This lets the existing D3D11-style shader/resource-view renderer continue to
work, while the actual resource owner and GPU queue are DX12. It also makes the
external `wgpu` texture and the GPUI sampling pass naturally belong to the same
device/queue family.

## Implementation Path

### 1. Add Windows Dependencies

Update workspace/windows feature usage so `gpui_windows` can use:

- `Win32_Graphics_Direct3D12`
- `Win32_Graphics_Direct3D11on12`

Add a Windows-target dependency from `gpui_windows` to `gpui_wgpu`.

Keep the implementation behind `cfg(target_os = "windows")`. The project should
fail clearly if DX12/D3D11On12 setup is unavailable, because this path is
intentionally DX12-only.

### 2. Introduce a Windows External WGPU Context

Add a Windows-specific context type, either in `gpui_wgpu::external_context` or
inside `gpui_windows`, that owns:

- `ExternalWgpuContext` or equivalent `Arc<wgpu::Device>` / `Arc<wgpu::Queue>`.
- Raw DX12 identity used for validation:
  - `ID3D12Device`
  - `ID3D12CommandQueue`
- A device-lost flag shared with the `wgpu` device callback.

Expected construction:

```text
wgpu::Instance::new(Backends::DX12)
  -> request_adapter
  -> request_device
  -> device.as_hal::<Dx12>()
  -> queue.as_hal::<Dx12>()
  -> raw ID3D12Device / ID3D12CommandQueue
```

Validation should ensure:

- The selected adapter backend is DX12.
- `device.as_hal::<Dx12>()` succeeds.
- `queue.as_hal::<Dx12>()` succeeds.
- The raw queue handed to D3D11On12 is the same queue extracted from `wgpu`.

### 3. Move Windows Renderer Onto D3D11On12

The existing Windows renderer uses D3D11 objects:

- `ID3D11Device`
- `ID3D11DeviceContext`
- `ID3D11Texture2D`
- `ID3D11ShaderResourceView`
- D3D11 shaders and buffers

To sample a `wgpu` DX12 texture through this renderer, those D3D11 objects must
come from a D3D11On12 device backed by the same `ID3D12Device` and queue.

Add a construction path for `DirectXDevices` or `DirectXRendererDevices` that:

1. Takes the raw DX12 device/queue from the Windows external wgpu context.
2. Calls `D3D11On12CreateDevice`.
3. Stores the resulting `ID3D11Device`, `ID3D11DeviceContext`, and
   `ID3D11On12Device`.
4. Uses this device for the atlas, render targets, path intermediate textures,
   sprite pipelines, and external compositor sampling.

Avoid creating a second plain D3D11 device for the renderer, because shader
resource views created on the D3D11On12 device cannot be bound to a separate
D3D11 device context.

### 4. Enable Windows Slot Registration

Mirror the macOS window integration:

- Add `external_compositors: Rc<RefCell<ExternalCompositorRegistry>>` to
  `WindowsWindowState`.
- Initialize it when creating the window state.
- Implement `PlatformWindow::external_compositor_registry` for `WindowsWindow`.
- Pass a clone of the registry into `DirectXRenderer::draw`, similar to macOS.

After this step, `Window::external_compositor_registry()` should return `Some`
on Windows, and `crates/gpui/examples/external_compositor.rs` should be able to
register a slot.

### 5. Compose External Slots Per Frame

Add a Windows equivalent of the macOS composition flow:

1. If the scene has no external compositor primitives, return an empty outcome map.
2. If the wgpu device-lost flag is set, invalidate the registry generation and
   skip this frame.
3. Create one `wgpu::CommandEncoder` for all external slots in the frame.
4. Iterate distinct `ExternalSlotHandle` values from `scene.external_compositors`.
5. For each valid live slot:
   - Clone the slot descriptor.
   - Take the compositor out of the registry.
   - Downcast it to `Box<dyn WgpuExternalCompositor>`.
   - Build `WgpuCompositorBackendCtx`.
   - Call `compose`.
   - Put the compositor back.
6. Before submission, ensure any ready texture that will be sampled by D3D11On12
   is transitioned to shader-resource usage.
7. Submit the single command buffer once for the frame.

For the transition step, prefer `wgpu::CommandEncoder::transition_resources`
where possible. The target steady state for external texture outputs is:

```text
wgpu render pass writes texture as COLOR_TARGET
  -> transition texture to TextureUses::RESOURCE
  -> D3D11On12 samples it as pixel shader resource
```

### 6. Convert Ready WGPU Views Into D3D11 Shader Resource Views

For each `ExternalComposeOutput::Ready { view }`:

1. Use `view.texture().as_hal::<wgpu::hal::api::Dx12>()`.
2. Clone the underlying `ID3D12Resource`.
3. Create or reuse a D3D11On12 wrapped resource with `CreateWrappedResource`.
4. Create or reuse an `ID3D11ShaderResourceView` for that wrapped resource.
5. Store this in the per-frame compose outcome together with alpha mode.

The cache key should include at least:

- Slot handle.
- Texture identity or underlying resource identity.
- Width/height.
- Format.
- Alpha mode if it changes shader behavior.

The initial version can be simpler and recreate wrapped resources/SRVs per
resize or resource change. Avoid rebuilding them every frame once the spike is
working.

### 7. Draw External Compositor Batches

Replace the current `PrimitiveBatch::ExternalCompositors(_) => Ok(())` arm in
`crates/gpui_windows/src/directx_renderer.rs`.

The draw path should follow the existing textured sprite/path-intermediate shape:

- Upload one instance per external compositor primitive.
- Respect primitive bounds and content mask.
- Bind the external SRV.
- Bind the existing sampler or a dedicated clamp/linear sampler.
- Draw a 4-vertex triangle strip.
- Apply alpha handling equivalent to macOS/wgpu:
  - straight-alpha inputs should be premultiplied by the shader or equivalent
    blend setup.
  - premultiplied inputs should pass through color channels.

If possible, reuse the existing `PipelineState<T>::draw_range_with_texture`
pattern. If a new shader module is needed, add it to the existing
`shader_resources::ShaderModule` enum and `shaders.hlsl`.

### 8. Synchronize D3D11On12 Access

For every wrapped external texture used in a frame:

1. Call `ID3D11On12Device::AcquireWrappedResources` before issuing D3D11 draw
   calls that sample it.
2. Draw the external compositor primitive(s).
3. Call `ID3D11On12Device::ReleaseWrappedResources`.
4. Flush the D3D11 context at the end of the frame or at the point required by
   the existing renderer present flow.

Because `wgpu` and D3D11On12 should submit to the same underlying
`ID3D12CommandQueue`, the intended ordering is:

```text
wgpu queue submit external compose commands
D3D11On12 records/samples released wrapped resources
D3D11 context flush submits draw commands to the same DX12 queue
present
```

This should avoid per-slot CPU waits. If a first spike needs a conservative
frame-level wait for diagnosis, keep it temporary and remove it before accepting
the final implementation.

### 9. Retain Texture Lifetimes Until GPU Completion

The frame outcome must retain:

- `Arc<wgpu::TextureView>`
- wrapped D3D11On12 resource
- `ID3D11ShaderResourceView`

until the GPU has finished sampling them.

Recommended implementation:

- Add a DX12 fence signaled after the D3D11On12 draw work is flushed.
- Store retained resources in a small pending list keyed by fence value.
- At the beginning or end of each frame, release entries whose fence has
  completed.

For an early spike, retaining the last few frames is acceptable as a temporary
debugging simplification. The engineering-grade path should use a fence.

### 10. Handle Device Loss and Context Recreation

Device loss can come from either the Windows renderer path or the `wgpu` device.
Unify both into the existing registry-generation model:

- Mark the Windows external wgpu context lost when the `wgpu` device-lost
  callback fires.
- On the next draw, call the registry context-recreation path and skip external
  composition for that frame.
- Recreate the `wgpu` DX12 context and the D3D11On12 renderer resources.
- Let user code observe stale handles through `registry.is_valid(handle)` and
  re-register.

The example's existing `on_context_recreated` and re-registration logic should
continue to work.

## Suggested Code Anchors

- `crates/gpui_windows/Cargo.toml`
  - Add `gpui_wgpu`.
  - Ensure Windows features include D3D12 and D3D11On12.

- `crates/gpui_windows/src/window.rs`
  - Add `ExternalCompositorRegistry` storage.
  - Implement `external_compositor_registry`.
  - Pass the registry into draw.

- `crates/gpui_windows/src/directx_devices.rs`
  - Add a D3D11On12-backed construction path, or introduce a parallel type if
    the old D3D11 path must remain temporarily for comparison.

- `crates/gpui_windows/src/directx_renderer.rs`
  - Own or receive the Windows external wgpu context.
  - Compose slots before drawing batches.
  - Convert `wgpu::TextureView` to wrapped D3D11On12 SRV.
  - Draw `PrimitiveBatch::ExternalCompositors`.
  - Retain frame resources by fence.

- `crates/gpui_wgpu/src/external_context.rs`
  - Add helper APIs for constructing a DX12-backed `ExternalWgpuContext` and
    exposing validated raw DX12 handles if that keeps the Windows crate smaller.

- `crates/gpui/examples/external_compositor.rs`
  - Should not need Windows-specific API changes. It is the primary smoke test.

## Acceptance Criteria

### Build and Test

On a Windows machine:

```powershell
cargo check -p gpui_windows
cargo check -p gpui_windows --features test-support
cargo test -p gpui external_compositor --lib
cargo check -p gpui --example external_compositor
cargo run -p gpui --example external_compositor
```

If `gpui_windows --features test-support` has unrelated platform-test failures,
record the exact failures and still keep the external compositor unit tests
passing.

### Runtime Smoke

Run:

```powershell
$env:EXTERNAL_COMPOSITOR_EXIT_AFTER = "5"
cargo run -p gpui --example external_compositor
```

Expected:

- The example logs successful compositor registration on Windows.
- `Window::external_compositor_registry()` returns `Some`.
- The demo compositor creates a DX12-backed `wgpu` render target.
- The window shows the external compositor texture, not just the fallback
  background.
- The process exits after the requested frame count.

### Device Identity

The implementation must log or assert in debug builds that:

- `wgpu` is using the DX12 backend.
- The `ID3D12Device` used for D3D11On12 is the one extracted from the `wgpu`
  DX12 hal device.
- The `ID3D12CommandQueue` used for D3D11On12 is the one extracted from the
  `wgpu` DX12 hal queue.

No path should silently fall back to a separate D3D11-only renderer for external
texture composition.

### Frame Composition Behavior

For a scene containing multiple primitives referencing the same slot:

- `WgpuExternalCompositor::compose` is called once per distinct slot handle per
  frame.
- There is one `wgpu::CommandEncoder` for all external slots in that frame.
- There is one `wgpu` queue submit for external composition in that frame.
- There is no per-slot CPU wait in the steady-state path.

### Resource State and Synchronization

The sampled DX12 resource must be transitioned to shader-resource usage before
D3D11On12 samples it.

The D3D11On12 path must:

- Acquire wrapped resources before draw.
- Release wrapped resources after draw.
- Flush to the same underlying DX12 queue.

The implementation should not rely on undefined D3D12 resource-state behavior.

### Lifetime

Returned `Arc<wgpu::TextureView>` values and wrapped D3D11On12 resources must be
retained until the GPU has completed the draw that samples them.

A fence-backed retention queue is the expected final mechanism. A fixed
multi-frame retention buffer is acceptable only as a temporary spike.

### Resize and HiDPI

On a high-DPI display:

- Slot descriptor dimensions are physical device texels, not logical pixels.
- The demo texture is not blurry relative to normal GPUI content.
- Resizing the window updates the registered slot via `ExternalCompositorRegistry::resize`.
- The compositor recreates its render target when `ctx.slot_descriptor` changes.
- Drag-resizing the window does not show persistent stale bounds or old-size
  textures.

### Device Loss

Simulated or real device loss should:

- Mark existing external slots stale.
- Call `on_context_recreated` on registered `WgpuExternalCompositor`
  implementations.
- Let the example unregister/re-register without panicking.
- Avoid sampling resources from the old device after recovery.

### Fallback

If DX12 or D3D11On12 initialization fails:

- Windows should report the failure clearly in logs.
- `Window::external_compositor_registry()` may return `None`, or registration may
  fail with a visible error path.
- The element fallback background should render instead of crashing.

## First Spike Checklist

The first Windows spike should stop after proving these three points:

1. `Window::external_compositor_registry()` returns `Some` on Windows.
2. A `wgpu` DX12 texture can be rendered by the example and its
   `ID3D12Resource` can be extracted from `TextureView::texture().as_hal::<Dx12>()`.
3. That resource can be wrapped with D3D11On12 and sampled by the GPUI Windows
   renderer in `external_compositor` example.

After those pass, harden the path with caching, resize, fences, device-lost
handling, and broader tests.

## Things To Avoid

- Do not use a separate D3D11 device to sample D3D11On12 resources.
- Do not use a separate DX12 device for `wgpu` and GPUI unless the design also
  implements shared-handle import/export and explicit cross-device sync.
- Do not add a Windows-specific public compositor API if the existing
  `gpui_wgpu::WgpuExternalCompositor` API can represent the use case.
- Do not accept per-slot CPU waits as the final implementation.
- Do not size external compositor textures in logical pixels.

use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
pub struct ExternalWgpuContext {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub device_lost: Arc<AtomicBool>,
}

impl ExternalWgpuContext {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let device_lost = Arc::new(AtomicBool::new(false));
        device.set_device_lost_callback({
            let device_lost = Arc::clone(&device_lost);
            move |reason, message| {
                log::error!(
                    "external compositor wgpu device lost: reason={reason:?}, message={message}"
                );
                if reason != wgpu::DeviceLostReason::Destroyed {
                    device_lost.store(true, Ordering::Relaxed);
                }
            }
        });

        Self {
            device: Arc::new(device),
            queue: Arc::new(queue),
            device_lost,
        }
    }

    pub fn from_wgpu_context(context: &crate::WgpuContext) -> Self {
        Self {
            device: Arc::clone(&context.device),
            queue: Arc::clone(&context.queue),
            device_lost: context.device_lost_flag(),
        }
    }

    #[cfg(target_os = "macos")]
    /// # Safety
    ///
    /// `metal_device` must be a valid, live `id<MTLDevice>` pointer. The returned
    /// context retains resources created from that device and must not outlive the
    /// platform object that owns the device.
    pub unsafe fn from_metal_device(metal_device: *mut std::ffi::c_void) -> anyhow::Result<Self> {
        unsafe { macos::from_metal_device(metal_device) }
    }

    #[cfg(target_os = "windows")]
    /// # Safety
    ///
    /// `_d3d12_device` must be a valid, live `ID3D12Device` pointer once this
    /// constructor is implemented.
    pub unsafe fn from_d3d12_device(_d3d12_device: *mut std::ffi::c_void) -> anyhow::Result<Self> {
        anyhow::bail!(
            "creating an external wgpu context from a D3D12 device is not implemented yet"
        )
    }
}

#[cfg(target_os = "windows")]
pub struct Dx12ExternalWgpuContext {
    pub context: ExternalWgpuContext,
    pub d3d12_device_raw: *mut c_void,
    pub d3d12_queue_raw: *mut c_void,
}

#[cfg(target_os = "windows")]
impl Dx12ExternalWgpuContext {
    pub fn new() -> anyhow::Result<Self> {
        windows_dx12::new_dx12_external_context()
    }
}

#[cfg(target_os = "windows")]
pub unsafe fn dx12_texture_resource_raw(
    texture_view: &wgpu::TextureView,
) -> anyhow::Result<*mut c_void> {
    unsafe { windows_dx12::texture_resource_raw(texture_view) }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::ExternalWgpuContext;
    use anyhow::Context as _;
    use std::ffi::c_void;
    use wgpu::hal::{Adapter as _, Instance as _};

    pub(super) unsafe fn from_metal_device(
        metal_device: *mut c_void,
    ) -> anyhow::Result<ExternalWgpuContext> {
        if metal_device.is_null() {
            anyhow::bail!("cannot create external wgpu context from a null Metal device");
        }

        let instance = unsafe {
            wgpu::Instance::from_hal::<wgpu::hal::api::Metal>(wgpu::hal::metal::Instance {})
        };
        let Some(hal_instance) = (unsafe { instance.as_hal::<wgpu::hal::api::Metal>() }) else {
            anyhow::bail!("external compositor wgpu instance is not backed by Metal");
        };

        let required_features = wgpu::Features::empty();
        let memory_hints = wgpu::MemoryHints::MemoryUsage;
        let adapters = unsafe { hal_instance.enumerate_adapters(None) };

        for hal_adapter in adapters {
            let required_limits = wgpu::Limits::downlevel_defaults()
                .using_resolution(hal_adapter.capabilities.limits.clone())
                .using_alignment(hal_adapter.capabilities.limits.clone());
            let Ok(open_device) = (unsafe {
                hal_adapter
                    .adapter
                    .open(required_features, &required_limits, &memory_hints)
            }) else {
                continue;
            };

            let opened_metal_device =
                objc2::rc::Retained::as_ptr(open_device.device.raw_device()) as *mut c_void;
            if opened_metal_device != metal_device {
                continue;
            }

            let adapter = unsafe { instance.create_adapter_from_hal(hal_adapter) };
            let descriptor = wgpu::DeviceDescriptor {
                label: Some("gpui_external_compositor_device"),
                required_features,
                required_limits,
                memory_hints,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            };
            let (device, queue) =
                unsafe { adapter.create_device_from_hal(open_device, &descriptor) }
                    .context("failed to create wgpu device from GPUI Metal device")?;

            validate_metal_device(&device, metal_device)?;

            let adapter_info = adapter.get_info();
            log::info!(
                "external compositor wgpu context is backed by GPUI Metal device: {} ({:?})",
                adapter_info.name,
                adapter_info.backend
            );

            return Ok(ExternalWgpuContext::new(device, queue));
        }

        anyhow::bail!(
            "failed to find a wgpu Metal adapter backed by GPUI Metal device ({metal_device:p})"
        )
    }

    fn validate_metal_device(
        device: &wgpu::Device,
        metal_device: *mut c_void,
    ) -> anyhow::Result<()> {
        let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Metal>() }) else {
            anyhow::bail!("external compositor wgpu device is not backed by Metal");
        };
        let wgpu_metal_device = objc2::rc::Retained::as_ptr(hal_device.raw_device()) as *mut c_void;
        if wgpu_metal_device != metal_device {
            anyhow::bail!(
                "external compositor wgpu Metal device ({wgpu_metal_device:p}) does not match \
                 GPUI Metal device ({metal_device:p})"
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod windows_dx12 {
    use super::{Dx12ExternalWgpuContext, ExternalWgpuContext};
    use anyhow::Context as _;
    use std::ffi::c_void;
    use windows_core_062::Interface as _;

    pub(super) fn new_dx12_external_context() -> anyhow::Result<Dx12ExternalWgpuContext> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        let adapter = gpui::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|error| anyhow::anyhow!("failed to request DX12 wgpu adapter: {error}"))?;
        let adapter_info = adapter.get_info();
        if adapter_info.backend != wgpu::Backend::Dx12 {
            anyhow::bail!(
                "external compositor wgpu adapter is not DX12: {} ({:?})",
                adapter_info.name,
                adapter_info.backend
            );
        }

        let required_features = wgpu::Features::empty();
        let (device, queue) = gpui::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("gpui_windows_external_compositor_device"),
                required_features,
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
            }),
        )
        .context("failed to create DX12 wgpu device for external compositing")?;

        let (d3d12_device_raw, d3d12_queue_raw) = {
            let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }) else {
                anyhow::bail!("external compositor wgpu device is not backed by DX12");
            };
            (
                hal_device.raw_device().as_raw(),
                hal_device.raw_queue().as_raw(),
            )
        };

        validate_dx12_handles(&device, d3d12_device_raw, d3d12_queue_raw)?;

        log::info!(
            "external compositor wgpu context is backed by DX12: {} ({:?})",
            adapter_info.name,
            adapter_info.backend
        );

        Ok(Dx12ExternalWgpuContext {
            context: ExternalWgpuContext::new(device, queue),
            d3d12_device_raw,
            d3d12_queue_raw,
        })
    }

    fn validate_dx12_handles(
        device: &wgpu::Device,
        d3d12_device_raw: *mut c_void,
        d3d12_queue_raw: *mut c_void,
    ) -> anyhow::Result<()> {
        let Some(hal_device) = (unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }) else {
            anyhow::bail!("external compositor wgpu device is not backed by DX12");
        };

        let hal_device_raw = hal_device.raw_device().as_raw();
        if hal_device_raw != d3d12_device_raw {
            anyhow::bail!(
                "external compositor DX12 device identity mismatch: wgpu={hal_device_raw:p}, \
                 expected={d3d12_device_raw:p}"
            );
        }

        let hal_queue_raw = hal_device.raw_queue().as_raw();
        if hal_queue_raw != d3d12_queue_raw {
            anyhow::bail!(
                "external compositor DX12 queue identity mismatch: wgpu={hal_queue_raw:p}, \
                 expected={d3d12_queue_raw:p}"
            );
        }

        log::debug!(
            "external compositor DX12 identity validated: device={hal_device_raw:p}, \
             queue={hal_queue_raw:p}"
        );
        Ok(())
    }

    pub(super) unsafe fn texture_resource_raw(
        texture_view: &wgpu::TextureView,
    ) -> anyhow::Result<*mut c_void> {
        let Some(hal_texture) =
            (unsafe { texture_view.texture().as_hal::<wgpu::hal::api::Dx12>() })
        else {
            anyhow::bail!("external compositor returned a non-DX12 wgpu texture on Windows");
        };
        Ok(unsafe { hal_texture.raw_resource().as_raw() })
    }
}

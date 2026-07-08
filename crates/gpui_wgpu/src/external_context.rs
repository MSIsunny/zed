use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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

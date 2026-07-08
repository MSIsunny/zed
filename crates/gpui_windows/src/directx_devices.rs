use anyhow::{Context, Result};
use gpui_wgpu::{Dx12ExternalWgpuContext, ExternalWgpuContext};
use itertools::Itertools;
use std::ffi::c_void;
use windows::Win32::Graphics::{
    Direct3D::{
        D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
    },
    Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
        D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
        ID3D11Device, ID3D11DeviceContext,
    },
    Direct3D11on12::{D3D11On12CreateDevice, ID3D11On12Device},
    Direct3D12::{ID3D12CommandQueue, ID3D12Device},
    Dxgi::{
        CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter1,
        IDXGIFactory6,
    },
};
use windows::core::{IUnknown, Interface};

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    pub(crate) d3d11on12_device: ID3D11On12Device,
    pub(crate) d3d12_device: ID3D12Device,
    pub(crate) d3d12_queue: ID3D12CommandQueue,
    pub(crate) external_wgpu_context: ExternalWgpuContext,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        let dx12_context =
            Dx12ExternalWgpuContext::new().context("Creating DX12 external wgpu context")?;
        let d3d12_device: ID3D12Device = clone_com_interface(
            dx12_context.d3d12_device_raw,
            "Borrowing external wgpu D3D12 device",
        )?;
        let d3d12_queue: ID3D12CommandQueue = clone_com_interface(
            dx12_context.d3d12_queue_raw,
            "Borrowing external wgpu D3D12 command queue",
        )?;
        let adapter = get_adapter_by_luid(&dxgi_factory, &d3d12_device)
            .context("Getting DXGI adapter for external wgpu DX12 device")?;
        log_adapter_info(&adapter);
        let (device, device_context, d3d11on12_device, feature_level) =
            create_d3d11on12_device(&d3d12_device, &d3d12_queue, debug_layer_available)
                .context("Creating D3D11On12 device")?;

        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created D3D11On12 device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created D3D11On12 device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created D3D11On12 device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
            d3d11on12_device,
            d3d12_device,
            d3d12_queue,
            external_wgpu_context: dx12_context.context,
        })
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

#[inline]
fn get_adapter_by_luid(
    dxgi_factory: &IDXGIFactory6,
    d3d12_device: &ID3D12Device,
) -> Result<IDXGIAdapter1> {
    let luid = unsafe { d3d12_device.GetAdapterLuid() };
    unsafe {
        dxgi_factory
            .EnumAdapterByLuid::<IDXGIAdapter1>(luid)
            .context("Enumerating adapter by DX12 device LUID")
    }
}

#[inline]
fn log_adapter_info(adapter: &IDXGIAdapter1) {
    if let Ok(desc) = unsafe { adapter.GetDesc1() } {
        let gpu_name = String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string();
        log::info!("Using GPU: {}", gpu_name);
    }
}

#[inline]
fn create_d3d11on12_device(
    d3d12_device: &ID3D12Device,
    d3d12_queue: &ID3D12CommandQueue,
    debug_layer_available: bool,
) -> Result<(
    ID3D11Device,
    ID3D11DeviceContext,
    ID3D11On12Device,
    D3D_FEATURE_LEVEL,
)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    let command_queue: IUnknown = d3d12_queue
        .cast()
        .context("Casting DX12 command queue for D3D11On12CreateDevice")?;

    unsafe {
        D3D11On12CreateDevice(
            d3d12_device,
            device_flags.0 as u32,
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            Some(&[Some(command_queue)]),
            0,
            Some(&mut device),
            Some(&mut context),
            Some(&mut feature_level),
        )?;
    }

    let device = device.context("D3D11On12CreateDevice returned no D3D11 device")?;
    validate_d3d11_device_features(&device)?;
    let context = context.context("D3D11On12CreateDevice returned no D3D11 device context")?;
    let d3d11on12_device = device
        .cast()
        .context("Casting D3D11 device to ID3D11On12Device")?;

    Ok((device, context, d3d11on12_device, feature_level))
}

#[inline]
fn clone_com_interface<T: Interface>(raw: *mut c_void, context: &'static str) -> Result<T> {
    if raw.is_null() {
        anyhow::bail!("{context}: null COM pointer");
    }
    unsafe { T::from_raw_borrowed(&raw) }
        .cloned()
        .with_context(|| context)
}

#[inline]
fn validate_d3d11_device_features(device: &ID3D11Device) -> Result<()> {
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}

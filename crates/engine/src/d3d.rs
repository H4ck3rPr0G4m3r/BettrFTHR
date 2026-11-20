//! D3D11 device creation on the adapter that actually drives the captured monitor.
//! Capturing on one GPU and encoding on another is the classic hybrid-laptop/iGPU failure,
//! so everything downstream uses this device and its adapter LUID.

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HMODULE, LUID, POINT, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::core::Interface;

/// Wrapper to move COM pointers between threads. D3D11 with multithread protection and
/// Media Foundation objects are free-threaded; windows-rs just can't know that.
pub struct Sendable<T>(pub T);
unsafe impl<T> Send for Sendable<T> {}
unsafe impl<T> Sync for Sendable<T> {}
impl<T> Sendable<T> {
    /// Use this inside closures: `x.0` would make Rust 2021+ capture only the inner,
    /// non-Send field.
    pub fn get(&self) -> &T {
        &self.0
    }
}
impl<T: Clone> Clone for Sendable<T> {
    fn clone(&self) -> Self {
        Sendable(self.0.clone())
    }
}

#[derive(Clone)]
#[allow(dead_code)] // monitor/rect are used by display-change recovery (M3)
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    pub adapter_name: String,
    pub monitor: HMONITOR,
    pub monitor_rect: RECT,
}

pub fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

pub fn create_for_monitor(monitor: HMONITOR) -> Result<Gpu> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut found: Option<(IDXGIAdapter1, RECT)> = None;
    let mut first: Option<IDXGIAdapter1> = None;
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        if first.is_none() {
            first = Some(adapter.clone());
        }
        let mut j = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(j) } {
            j += 1;
            let desc = unsafe { output.GetDesc() }?;
            if desc.Monitor == monitor {
                found = Some((adapter.clone(), desc.DesktopCoordinates));
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (adapter, rect) = match found {
        Some(f) => f,
        None => {
            tracing::warn!("monitor not attached to any enumerated adapter; using the first adapter");
            (first.ok_or_else(|| anyhow!("no DXGI adapters"))?, RECT::default())
        }
    };
    let desc = unsafe { adapter.GetDesc1() }?;
    let name_len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    let adapter_name = String::from_utf16_lossy(&desc.Description[..name_len]);

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("no device"))?;
    let context = context.ok_or_else(|| anyhow!("no context"))?;
    // Capture callback, converter and the MF encoder all touch the immediate context.
    let mt: ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    tracing::info!(adapter = %adapter_name, "D3D11 device created");
    Ok(Gpu {
        device,
        context,
        luid: desc.AdapterLuid,
        adapter_name,
        monitor,
        monitor_rect: rect,
    })
}

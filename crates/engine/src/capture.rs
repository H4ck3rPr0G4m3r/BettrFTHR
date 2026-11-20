//! Windows.Graphics.Capture source. Each arriving frame is copied (GPU→GPU) into a single
//! "latest frame" texture and released immediately, so the WGC pool never starves; the
//! constant-rate ticker reads from that texture.

use crate::clock;
use crate::d3d::{Gpu, Sendable};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

pub struct LatestFrame {
    pub tex: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    /// Number of frames received so far (0 = nothing captured yet).
    pub frames: u64,
    pub last_frame_hns: i64,
}

pub struct MonitorCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    pub latest: Arc<Mutex<Sendable<LatestFrame>>>,
    /// Set when the source size changes; the pipeline must be rebuilt.
    pub size_changed: Arc<AtomicBool>,
    pub width: u32,
    pub height: u32,
}

pub fn create_bgra_texture(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D(BGRA)")?;
    Ok(tex.unwrap())
}

impl MonitorCapture {
    pub fn start(gpu: &Gpu, monitor: HMONITOR, cursor: bool) -> Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")?;
        let size = item.Size()?;
        let (width, height) = (size.Width.max(2) as u32, size.Height.max(2) as u32);

        let dxgi: IDXGIDevice = gpu.device.cast()?;
        let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;

        let latest = Arc::new(Mutex::new(Sendable(LatestFrame {
            tex: create_bgra_texture(gpu, width, height)?,
            width,
            height,
            frames: 0,
            last_frame_hns: 0,
        })));
        let size_changed = Arc::new(AtomicBool::new(false));

        let ctx = Sendable(gpu.context.clone());
        let device = Sendable(winrt_device.clone());
        let latest_cb = latest.clone();
        let changed_cb = size_changed.clone();
        let pool_size = Mutex::new(size);
        pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
            move |sender, _| {
                let pool = sender.ok()?;
                let frame = pool.TryGetNextFrame()?;
                let content = frame.ContentSize()?;
                let surface = frame.Surface()?;
                let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
                let src: ID3D11Texture2D = unsafe { access.GetInterface()? };
                {
                    let mut l = latest_cb.lock();
                    let w = (content.Width.max(0) as u32).min(l.0.width);
                    let h = (content.Height.max(0) as u32).min(l.0.height);
                    let region = D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: w,
                        bottom: h,
                        back: 1,
                    };
                    unsafe {
                        ctx.get()
                            .CopySubresourceRegion(&l.0.tex, 0, 0, 0, 0, &src, 0, Some(&region))
                    };
                    l.0.frames += 1;
                    l.0.last_frame_hns = clock::now_hns();
                }
                drop(frame);
                let mut ps = pool_size.lock();
                if content.Width != ps.Width || content.Height != ps.Height {
                    tracing::info!(w = content.Width, h = content.Height, "capture source size changed");
                    *ps = SizeInt32 {
                        Width: content.Width,
                        Height: content.Height,
                    };
                    pool.Recreate(device.get(), DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, *ps)?;
                    changed_cb.store(true, Ordering::SeqCst);
                }
                Ok(())
            },
        ))?;

        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(cursor);
        // Windows 11+: no yellow capture border. Older builds reject this; harmless.
        if let Err(e) = session.SetIsBorderRequired(false) {
            tracing::debug!("SetIsBorderRequired unsupported: {e}");
        }
        session.StartCapture()?;
        tracing::info!(width, height, "monitor capture started");
        Ok(Self {
            session,
            pool,
            latest,
            size_changed,
            width,
            height,
        })
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

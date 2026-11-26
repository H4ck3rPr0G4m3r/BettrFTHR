//! Windows.Graphics.Capture source. The newest arriving frame is parked in [`LatestFrame`]
//! (replacing, and so releasing, the one before it); the constant-rate ticker copies it
//! (GPU→GPU) into a single "latest frame" texture and releases it. Frames the ticker never
//! sees cost nothing, so a 144-360 Hz monitor costs no more copies than one at the
//! recording rate, and a static screen none at all. At most one frame is held, so the WGC
//! pool never starves.
//!
//! A WGC monitor session can silently go blank: frames keep arriving (the cursor is still
//! drawn into them) but every other pixel is zero, and it stays that way until the session
//! is recreated. Seen with VALORANT after hours of uptime. [`BlankProbe`] spots that state
//! and [`MonitorCapture::reopen`] swaps in a fresh session that writes into the same
//! texture, so the converter, the encoder and the replay buffer carry on untouched.

use crate::clock;
use crate::d3d::{Gpu, Sendable};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11DeviceContext, ID3D11Texture2D,
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
    /// The newest frame not yet copied into `tex`, with the size of the region to copy.
    pending: Option<(Direct3D11CaptureFrame, u32, u32)>,
}

impl LatestFrame {
    /// Copies the newest captured frame into `tex` and hands it back to the pool. False when
    /// nothing new arrived since the last call: `tex` already holds the newest picture.
    pub fn update(&mut self, ctx: &ID3D11DeviceContext) -> bool {
        let Some((frame, w, h)) = self.pending.take() else {
            return false;
        };
        let src = frame
            .Surface()
            .and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>())
            .and_then(|a| unsafe { a.GetInterface::<ID3D11Texture2D>() });
        let src = match src {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!("capture frame without a texture: {e}");
                return false;
            }
        };
        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: w,
            bottom: h,
            back: 1,
        };
        // Queued on our device; releasing the frame right after is what the callback
        // always did after its own copy.
        unsafe { ctx.CopySubresourceRegion(&self.tex, 0, 0, 0, 0, &src, 0, Some(&region)) };
        true
    }
}

pub type Latest = Arc<Mutex<Sendable<LatestFrame>>>;

/// One WGC session (frame pool + session) feeding the shared latest-frame texture.
struct Session {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

pub struct MonitorCapture {
    session: Option<Session>,
    monitor: HMONITOR,
    cursor: bool,
    pub latest: Latest,
    /// Set when the source size changes or the source disappears; the pipeline must be rebuilt.
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

fn monitor_item(monitor: HMONITOR) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    unsafe { interop.CreateForMonitor(monitor) }.context("CreateForMonitor")
}

impl MonitorCapture {
    pub fn start(gpu: &Gpu, monitor: HMONITOR, cursor: bool) -> Result<Self> {
        let item = monitor_item(monitor)?;
        let size = item.Size()?;
        let (width, height) = (size.Width.max(2) as u32, size.Height.max(2) as u32);
        let latest = Arc::new(Mutex::new(Sendable(LatestFrame {
            tex: create_bgra_texture(gpu, width, height)?,
            width,
            height,
            frames: 0,
            last_frame_hns: 0,
            pending: None,
        })));
        let size_changed = Arc::new(AtomicBool::new(false));
        let session = open_session(gpu, &item, cursor, &latest, &size_changed)?;
        tracing::info!(width, height, "monitor capture started");
        Ok(Self {
            session: Some(session),
            monitor,
            cursor,
            latest,
            size_changed,
            width,
            height,
        })
    }

    /// Replace the WGC session with a fresh one that writes into the same texture. If the
    /// monitor's size changed meanwhile, flags `size_changed` so the pipeline is rebuilt.
    pub fn reopen(&mut self, gpu: &Gpu) -> Result<()> {
        self.session = None; // close first: the old callback must stop delivering
        self.latest.lock().0.pending = None;
        let item = monitor_item(self.monitor)?;
        let size = item.Size()?;
        if size.Width.max(2) as u32 != self.width || size.Height.max(2) as u32 != self.height {
            self.size_changed.store(true, Ordering::SeqCst);
        }
        self.session = Some(open_session(gpu, &item, self.cursor, &self.latest, &self.size_changed)?);
        Ok(())
    }
}

impl Drop for MonitorCapture {
    fn drop(&mut self) {
        // Hand the parked frame back before its pool closes.
        self.latest.lock().0.pending = None;
    }
}

fn open_session(
    gpu: &Gpu,
    item: &GraphicsCaptureItem,
    cursor: bool,
    latest: &Latest,
    size_changed: &Arc<AtomicBool>,
) -> Result<Session> {
    let size = item.Size()?;
    let dxgi: IDXGIDevice = gpu.device.cast()?;
    let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;

    let device = Sendable(winrt_device.clone());
    let latest_cb = latest.clone();
    let changed_cb = size_changed.clone();
    let pool_size = Mutex::new(size);
    pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
        move |sender, _| {
            let pool = sender.ok()?;
            let frame = pool.TryGetNextFrame()?;
            let content = frame.ContentSize()?;
            {
                // Park it for the ticker; the frame it replaces (never copied) goes back to
                // the pool right here.
                let mut l = latest_cb.lock();
                let w = (content.Width.max(0) as u32).min(l.0.width);
                let h = (content.Height.max(0) as u32).min(l.0.height);
                l.0.pending = Some((frame, w, h));
                l.0.frames += 1;
                l.0.last_frame_hns = clock::now_hns();
            }
            let mut ps = pool_size.lock();
            if content.Width != ps.Width || content.Height != ps.Height {
                tracing::info!(w = content.Width, h = content.Height, "capture source size changed");
                latest_cb.lock().0.pending = None; // a frame of the old size; the pipeline is rebuilt
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

    // Monitor unplugged / display topology changed: the item dies; rebuild the pipeline.
    let closed_cb = size_changed.clone();
    item.Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(
        move |_, _| {
            tracing::warn!("capture item closed (display disconnected?)");
            closed_cb.store(true, Ordering::SeqCst);
            Ok(())
        },
    ))?;
    let session = pool.CreateCaptureSession(item)?;
    let _ = session.SetIsCursorCaptureEnabled(cursor);
    // Windows 11+: no yellow capture border. Older builds reject this; harmless.
    if let Err(e) = session.SetIsBorderRequired(false) {
        tracing::debug!("SetIsBorderRequired unsupported: {e}");
    }
    session.StartCapture()?;
    Ok(Session { session, pool })
}

/// Rows of the frame sampled by [`BlankProbe`], spread evenly from top to bottom.
pub const BLANK_ROWS: usize = 16;

/// True when the sampled BGRA rows carry no picture: every pixel is exactly black apart from
/// a handful (the cursor, which WGC draws even when the content is gone). Dark scenes have
/// noise and dithering, and black loading screens a lit logo or progress bar.
pub fn is_blank(bgra: &[u8]) -> bool {
    let lit = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] | p[1] | p[2] != 0)
        .count();
    lit * 200 <= bgra.len() / 4 // ≤ 0.5 %
}

/// Asynchronous readback of a few rows of the latest frame: `sample` queues a GPU copy and
/// a later `poll` reads it without stalling the caller.
pub struct BlankProbe {
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    pending: bool,
}

impl BlankProbe {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: BLANK_ROWS as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D(staging)")?;
        Ok(Self {
            staging: tex.unwrap(),
            width,
            height,
            pending: false,
        })
    }

    /// Queue a copy of the sampled rows of `src` (the latest-frame texture).
    pub fn sample(&mut self, ctx: &ID3D11DeviceContext, src: &ID3D11Texture2D) {
        for i in 0..BLANK_ROWS as u32 {
            let y = (2 * i + 1) * self.height / (2 * BLANK_ROWS as u32);
            let row = D3D11_BOX {
                left: 0,
                top: y,
                front: 0,
                right: self.width,
                bottom: y + 1,
                back: 1,
            };
            unsafe { ctx.CopySubresourceRegion(&self.staging, 0, 0, i, 0, src, 0, Some(&row)) };
        }
        self.pending = true;
    }

    /// Verdict on the last `sample` once the GPU has finished the copy; `None` until then.
    pub fn poll(&mut self, ctx: &ID3D11DeviceContext) -> Option<bool> {
        if !self.pending {
            return None;
        }
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        let flags = D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32;
        unsafe { ctx.Map(&self.staging, 0, D3D11_MAP_READ, flags, Some(&mut map)) }.ok()?;
        let row_bytes = self.width as usize * 4;
        let mut px = Vec::with_capacity(row_bytes * BLANK_ROWS);
        for i in 0..BLANK_ROWS {
            let row = unsafe {
                std::slice::from_raw_parts((map.pData as *const u8).add(i * map.RowPitch as usize), row_bytes)
            };
            px.extend_from_slice(row);
        }
        unsafe { ctx.Unmap(&self.staging, 0) };
        self.pending = false;
        Some(is_blank(&px))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(w: usize, rows: usize, fill: [u8; 4]) -> Vec<u8> {
        fill.iter().copied().cycle().take(w * rows * 4).collect()
    }

    #[test]
    fn blank_frames_are_detected() {
        let w = 1920;
        // What a broken WGC session hands us: zero colour, opaque alpha.
        assert!(is_blank(&bgra(w, BLANK_ROWS, [0, 0, 0, 255])));
        // ...with the cursor drawn on top of it.
        let mut px = bgra(w, BLANK_ROWS, [0, 0, 0, 255]);
        for x in 900..932 {
            px[(w + x) * 4..(w + x) * 4 + 3].copy_from_slice(&[255, 255, 255]);
        }
        assert!(is_blank(&px));
    }

    #[test]
    fn dark_scenes_are_not_blank() {
        let w = 1920;
        // A dark scene still has some colour in most pixels.
        assert!(!is_blank(&bgra(w, BLANK_ROWS, [1, 2, 1, 255])));
        // A black loading screen with a lit progress bar across one sampled row.
        let mut px = bgra(w, BLANK_ROWS, [0, 0, 0, 255]);
        for x in 200..1700 {
            px[(3 * w + x) * 4..(3 * w + x) * 4 + 3].copy_from_slice(&[40, 200, 240]);
        }
        assert!(!is_blank(&px));
    }
}

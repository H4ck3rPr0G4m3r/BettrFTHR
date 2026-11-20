//! Hardware H.264 encoding through Media Foundation. One code path covers NVENC, AMD AMF and
//! Intel QSV because each vendor ships an MF transform; we pick the one that lives on the
//! same adapter as the capture device.

use crate::d3d::Sendable;
use crate::h264::{AccessUnit, annexb_to_avcc};
use anyhow::{Context, Result, anyhow, bail};
use crossbeam_channel::Receiver;
use std::mem::ManuallyDrop;
use std::thread::JoinHandle;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};

#[derive(Clone, Copy, Debug)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub gop_frames: u32,
}

pub struct EncodeJob {
    pub texture: Sendable<ID3D11Texture2D>,
    pub pts: i64,
    pub duration: i64,
}

pub struct EncodedFrame {
    pub pts: i64,
    pub duration: i64,
    pub au: AccessUnit,
}

pub struct VideoEncoder {
    thread: Option<JoinHandle<()>>,
    pub name: String,
}

impl VideoEncoder {
    /// Starts the encoder thread. Frames are fed through `jobs`; dropping the sender drains
    /// and stops the encoder. Encoded frames go to `on_frame`; a fatal error goes to `on_error`.
    pub fn start(
        device: &ID3D11Device,
        luid: LUID,
        adapter_name: String,
        cfg: EncoderConfig,
        jobs: Receiver<EncodeJob>,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
        on_error: impl FnOnce(anyhow::Error) + Send + 'static,
    ) -> Result<Self> {
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<String>>(1);
        let device = Sendable(device.clone());
        let thread = std::thread::Builder::new()
            .name("video-encoder".into())
            .spawn(move || {
                let _com = ComScope::init();
                let mut session = match EncoderSession::new(device.get(), luid, &adapter_name, cfg)
                {
                    Ok(s) => {
                        let _ = init_tx.send(Ok(s.name.clone()));
                        s
                    }
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                if let Err(e) = session.run(&jobs, on_frame) {
                    tracing::error!("video encoder failed: {e:#}");
                    on_error(e);
                }
            })?;
        let name = init_rx
            .recv()
            .map_err(|_| anyhow!("encoder thread died during init"))??;
        Ok(Self {
            thread: Some(thread),
            name,
        })
    }

    pub fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.join();
    }
}

pub struct ComScope;
impl ComScope {
    pub fn init() -> Self {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        ComScope
    }
}
impl Drop for ComScope {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct EncoderSession {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    _manager: IMFDXGIDeviceManager,
    name: String,
    frames_out: u64,
}

fn set_codec_value(codec: &ICodecAPI, api: &GUID, value: VARIANT, label: &str) {
    if let Err(e) = unsafe { codec.SetValue(api, &value) } {
        tracing::debug!("encoder ignores {label}: {e}");
    }
}

impl EncoderSession {
    fn new(
        device: &ID3D11Device,
        luid: LUID,
        adapter_name: &str,
        cfg: EncoderConfig,
    ) -> Result<Self> {
        let (activate, name) = find_hardware_h264(luid, adapter_name)?;
        let transform: IMFTransform =
            unsafe { activate.ActivateObject() }.context("activate encoder MFT")?;
        let attrs = unsafe { transform.GetAttributes() }?;
        let is_async = unsafe { attrs.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) != 0;
        if !is_async {
            bail!("{name} is not an async hardware MFT");
        }
        unsafe { attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1) }?;
        let events: IMFMediaEventGenerator = transform.cast()?;

        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }?;
        let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
        unsafe { manager.ResetDevice(device, token) }?;
        unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
            .context("encoder rejected the D3D11 device")?;

        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            set_codec_value(
                &codec,
                &CODECAPI_AVEncCommonRateControlMode,
                VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                "rate control",
            );
            set_codec_value(
                &codec,
                &CODECAPI_AVEncCommonMeanBitRate,
                VARIANT::from(cfg.bitrate_kbps * 1000),
                "bitrate",
            );
            set_codec_value(
                &codec,
                &CODECAPI_AVEncMPVGOPSize,
                VARIANT::from(cfg.gop_frames),
                "gop",
            );
            set_codec_value(
                &codec,
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                VARIANT::from(0u32),
                "b-frames",
            );
            set_codec_value(
                &codec,
                &CODECAPI_AVLowLatencyMode,
                VARIANT::from(true),
                "low latency",
            );
        }

        let size = ((cfg.width as u64) << 32) | cfg.height as u64;
        let rate = ((cfg.fps as u64) << 32) | 1;
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps * 1000)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            set_color(&out)?;
            transform
                .SetOutputType(0, &out, 0)
                .context("SetOutputType(H.264)")?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            set_color(&inp)?;
            transform
                .SetInputType(0, &inp, 0)
                .context("SetInputType(NV12)")?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        tracing::info!(encoder = %name, w = cfg.width, h = cfg.height, fps = cfg.fps, kbps = cfg.bitrate_kbps, "video encoder ready");
        Ok(Self {
            transform,
            events,
            _manager: manager,
            name,
            frames_out: 0,
        })
    }

    fn run(
        &mut self,
        jobs: &Receiver<EncodeJob>,
        mut on_frame: impl FnMut(EncodedFrame),
    ) -> Result<()> {
        let mut draining = false;
        loop {
            let event = unsafe {
                self.events
                    .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
            }
            .context("encoder GetEvent")?;
            let kind = unsafe { event.GetType() }?;
            let status = unsafe { event.GetStatus() }?;
            if status.is_err() {
                bail!("encoder event {kind} failed: {status:?}");
            }
            if kind == METransformNeedInput.0 as u32 {
                if draining {
                    continue;
                }
                match jobs.recv() {
                    Ok(job) => self.push(job)?,
                    Err(_) => {
                        draining = true;
                        unsafe {
                            self.transform
                                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                            self.transform
                                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                        }
                    }
                }
            } else if kind == METransformHaveOutput.0 as u32 {
                if let Some(frame) = self.pull()? {
                    on_frame(frame);
                }
            } else if kind == METransformDrainComplete.0 as u32 {
                tracing::info!(frames = self.frames_out, "video encoder drained");
                return Ok(());
            }
        }
    }

    fn push(&mut self, job: EncodeJob) -> Result<()> {
        unsafe {
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &job.texture.0, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(job.pts)?;
            sample.SetSampleDuration(job.duration)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("encoder ProcessInput")?;
        }
        Ok(())
    }

    fn pull(&mut self) -> Result<Option<EncodedFrame>> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let own = if provides {
            None
        } else {
            unsafe {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(4 << 20))?)?;
                Some(s)
            }
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(own),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let res = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut out[0].pSample) };
        unsafe { ManuallyDrop::drop(&mut out[0].pEvents) };
        match res {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                unsafe {
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                }
                tracing::info!("encoder output type renegotiated");
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("encoder ProcessOutput"),
        }
        let sample = sample.ok_or_else(|| anyhow!("encoder produced no sample"))?;
        let (pts, duration, data) = unsafe {
            let pts = sample.GetSampleTime().unwrap_or(0);
            let duration = sample.GetSampleDuration().unwrap_or(0);
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            (pts, duration, data)
        };
        self.frames_out += 1;
        Ok(Some(EncodedFrame {
            pts,
            duration,
            au: annexb_to_avcc(&data),
        }))
    }
}

impl Drop for EncoderSession {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

unsafe fn set_color(t: &IMFMediaType) -> windows::core::Result<()> {
    unsafe {
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

fn attr_string(a: &IMFActivate, key: &GUID) -> Option<String> {
    let mut p = PWSTR::null();
    let mut len = 0u32;
    unsafe {
        a.GetAllocatedString(key, &mut p, &mut len).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Lists hardware H.264 encoders, optionally restricted to one adapter (diagnostics).
pub fn list_hardware_h264(luid: Option<LUID>) -> Vec<String> {
    enum_hardware_h264(luid)
        .map(|v| v.into_iter().map(|(_, n)| n).collect())
        .unwrap_or_default()
}

/// With `luid`, uses MFTEnum2 so Media Foundation itself returns only the encoders that run
/// on that adapter (MFTEnumEx doesn't report adapters at all).
fn enum_hardware_h264(luid: Option<LUID>) -> Result<Vec<(IMFActivate, String)>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        match luid {
            Some(l) => {
                let mut attrs: Option<IMFAttributes> = None;
                MFCreateAttributes(&mut attrs, 1)?;
                let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes"))?;
                let mut blob = [0u8; 8];
                blob[..4].copy_from_slice(&l.LowPart.to_le_bytes());
                blob[4..].copy_from_slice(&l.HighPart.to_le_bytes());
                attrs.SetBlob(&MFT_ENUM_ADAPTER_LUID, &blob)?;
                MFTEnum2(
                    MFT_CATEGORY_VIDEO_ENCODER,
                    flags,
                    Some(&input),
                    Some(&output),
                    &attrs,
                    &mut ptr,
                    &mut count,
                )
                .context("MFTEnum2")?;
            }
            None => MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &mut ptr,
                &mut count,
            )
            .context("MFTEnumEx")?,
        }
    }
    let mut list = Vec::new();
    if !ptr.is_null() {
        let slots = unsafe { std::slice::from_raw_parts_mut(ptr, count as usize) };
        for slot in slots.iter_mut() {
            if let Some(a) = slot.take() {
                let name = attr_string(&a, &MFT_FRIENDLY_NAME_Attribute)
                    .unwrap_or_else(|| "unknown encoder".into());
                list.push((a, name));
            }
        }
        unsafe { CoTaskMemFree(Some(ptr as *const _)) };
    }
    Ok(list)
}

fn vendor_matches(encoder: &str, adapter: &str) -> bool {
    let (e, a) = (encoder.to_ascii_lowercase(), adapter.to_ascii_lowercase());
    ["nvidia", "amd", "intel"]
        .iter()
        .any(|v| e.contains(v) && (a.contains(v) || (*v == "amd" && a.contains("radeon"))))
}

fn find_hardware_h264(luid: LUID, adapter_name: &str) -> Result<(IMFActivate, String)> {
    match enum_hardware_h264(Some(luid)) {
        Ok(mut list) if !list.is_empty() => return Ok(list.swap_remove(0)),
        Ok(_) => {
            tracing::warn!("no encoder registered for the capture adapter; matching by vendor")
        }
        Err(e) => tracing::warn!("MFTEnum2 failed ({e:#}); matching by vendor"),
    }
    let mut list = enum_hardware_h264(None)?;
    let names: Vec<_> = list.iter().map(|(_, n)| n.clone()).collect();
    match list
        .iter()
        .position(|(_, n)| vendor_matches(n, adapter_name))
    {
        Some(i) => Ok(list.swap_remove(i)),
        None => bail!("no hardware H.264 encoder for {adapter_name} (found: {names:?})"),
    }
}

#[cfg(test)]
mod tests {
    use super::vendor_matches;

    #[test]
    fn vendor_matching() {
        assert!(vendor_matches(
            "NVIDIA H.264 Encoder MFT",
            "NVIDIA GeForce RTX 3060 Ti"
        ));
        assert!(vendor_matches("AMDh264Encoder", "AMD Radeon(TM) Graphics"));
        assert!(!vendor_matches(
            "AMDh264Encoder",
            "NVIDIA GeForce RTX 3060 Ti"
        ));
    }
}

//! Re-encoding for upload limits (Discord): the Media Foundation source reader decodes and
//! scales on the GPU, the same hardware encoder as capture encodes, our muxer writes.
//! Unlike the lossless export, cuts here are frame-accurate.

use crate::clock::HNS_PER_SEC;
use crate::convert::create_nv12_texture;
use crate::d3d::{self, Sendable};
use crate::encoder::{EncodeJob, EncoderConfig, VideoEncoder};
use crate::ring::Packet;
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, HSTRING, Interface};

#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Plan {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
}

/// Picks resolution, frame rate and bitrate so `seconds` of video plus audio fit in
/// `target_bytes`. Lower bitrates get fewer pixels: blocky 1080p looks worse than clean 720p.
pub fn plan(target_bytes: u64, seconds: f64, audio_kbps: u32, src_w: u32, src_h: u32, src_fps: u32) -> Result<Plan> {
    let total_kbps = target_bytes as f64 * 8.0 / 1000.0 / seconds.max(0.1);
    // Container overhead and rate-control wobble: aim 6% under the limit.
    let video_kbps = (total_kbps * 0.94 - audio_kbps as f64).floor();
    if video_kbps < 250.0 {
        bail!(
            "{seconds:.0} s is too long to fit in {} MB; trim it shorter",
            target_bytes / 1024 / 1024
        );
    }
    let video_kbps = video_kbps as u32;
    let height = match video_kbps {
        k if k >= 5000 => 1080,
        k if k >= 2200 => 720,
        k if k >= 1100 => 540,
        _ => 480,
    }
    .min(src_h);
    let width = ((src_w as u64 * height as u64 / src_h.max(1) as u64) as u32) & !1;
    let fps = if video_kbps < 3000 { src_fps.min(30) } else { src_fps.min(60) };
    Ok(Plan {
        width,
        height: height & !1,
        fps: fps.max(1),
        video_kbps,
    })
}

pub struct Encoded {
    pub packets: Vec<Packet>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

const POOL: usize = 16;

/// Decodes `[start, end)` of the input's video, scales it to the plan and encodes it.
/// Packet times start at 0 = `start`.
pub fn video(input: &Path, start: f64, end: f64, plan: &Plan, progress: &mut dyn FnMut(f64)) -> Result<Encoded> {
    let gpu = d3d::create_for_monitor(d3d::primary_monitor())?;
    let (reader, _mgr) = unsafe { open_reader(input, &gpu.device, plan)? };

    let pool: Vec<ID3D11Texture2D> = (0..POOL)
        .map(|_| create_nv12_texture(&gpu, plan.width, plan.height))
        .collect::<Result<_>>()?;
    let out: Arc<Mutex<Vec<Packet>>> = Arc::default();
    let params: Arc<Mutex<Option<(Vec<u8>, Vec<u8>)>>> = Arc::default();
    let failure: Arc<Mutex<Option<String>>> = Arc::default();
    let (tx, rx) = crossbeam_channel::bounded::<EncodeJob>(4);
    let mut encoder = {
        let (out, params, failure) = (out.clone(), params.clone(), failure.clone());
        VideoEncoder::start(
            &gpu.device,
            gpu.luid,
            gpu.adapter_name.clone(),
            EncoderConfig {
                width: plan.width,
                height: plan.height,
                fps: plan.fps,
                bitrate_kbps: plan.video_kbps,
                // Exports aren't cut again losslessly: longer GOPs spend the bits on detail.
                gop_frames: plan.fps * 2,
            },
            rx,
            move |frame| {
                if let (Some(sps), Some(pps)) = (&frame.au.sps, &frame.au.pps) {
                    *params.lock() = Some((sps.clone(), pps.clone()));
                }
                if !frame.au.avcc.is_empty() {
                    out.lock().push(Packet {
                        pts: frame.pts,
                        duration: frame.duration,
                        key: frame.au.idr,
                        data: Arc::from(frame.au.avcc),
                    });
                }
            },
            move |e| *failure.lock() = Some(format!("{e:#}")),
        )?
    };

    let start_hns = (start * HNS_PER_SEC as f64) as i64;
    let end_hns = (end * HNS_PER_SEC as f64) as i64;
    let period = HNS_PER_SEC / plan.fps as i64;
    let mut next_slot = 0i64;
    // Exactly this many output frames, so the video ends where the audio does.
    let slots = ((end_hns - start_hns) as f64 / period as f64).round() as i64;
    let mut sent = 0usize;
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
    unsafe {
        reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(start_hns))?;
        loop {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample: Option<IMFSample> = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || ts >= end_hns {
                break;
            }
            let Some(sample) = sample else { continue };
            // Frames before the cut (decoded from the previous keyframe) are skipped; a lower
            // output frame rate keeps one source frame per output slot.
            let rel = ts - start_hns;
            if rel < -period / 2 {
                continue;
            }
            let slot = (rel + period / 2).div_euclid(period);
            if slot >= slots {
                break;
            }
            if slot < next_slot {
                continue;
            }
            next_slot = slot + 1;
            let buf = sample.GetBufferByIndex(0)?;
            let dx: IMFDXGIBuffer = buf.cast().context("decoder did not output GPU frames")?;
            let mut tex: Option<ID3D11Texture2D> = None;
            dx.GetResource(&ID3D11Texture2D::IID, &mut tex as *mut _ as *mut _)?;
            let tex = tex.context("no texture in decoded frame")?;
            let sub = dx.GetSubresourceIndex()?;
            let dst = &pool[sent % POOL];
            gpu.context.CopySubresourceRegion(dst, 0, 0, 0, 0, &tex, sub, None);
            if let Some(e) = failure.lock().clone() {
                bail!("encoder failed: {e}");
            }
            tx.send(EncodeJob {
                texture: Sendable(dst.clone()),
                pts: slot * period,
                duration: period,
            })
            .map_err(|_| anyhow!("encoder stopped"))?;
            sent += 1;
            progress((rel as f64 / (end_hns - start_hns).max(1) as f64).clamp(0.0, 1.0));
        }
    }
    drop(tx); // drains the encoder
    encoder.join();
    if let Some(e) = failure.lock().clone() {
        bail!("encoder failed: {e}");
    }
    let (sps, pps) = params.lock().clone().context("encoder produced no parameter sets")?;
    let mut packets = std::mem::take(&mut *out.lock());
    packets.sort_by_key(|p| p.pts);
    if packets.is_empty() || !packets[0].key {
        bail!("encoder output did not start with a keyframe ({} frames sent)", sent);
    }
    Ok(Encoded { packets, sps, pps })
}

/// Source reader with GPU decoding and scaling to the planned size, NV12 out.
unsafe fn open_reader(
    input: &Path,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    plan: &Plan,
) -> Result<(IMFSourceReader, IMFDXGIDeviceManager)> {
    unsafe {
        let mut token = 0u32;
        let mut mgr: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
        let mgr = mgr.context("no DXGI device manager")?;
        mgr.ResetDevice(device, token)?;

        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 3)?;
        let attrs = attrs.context("no attributes")?;
        attrs.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr)?;
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)?;

        let url = HSTRING::from(input.as_os_str());
        let reader = MFCreateSourceReaderFromURL(&url, &attrs).context("open the clip for decoding")?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(stream, true)?;
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((plan.width as u64) << 32) | plan.height as u64)?;
        reader
            .SetCurrentMediaType(stream, None, &t)
            .context("decoder can't output scaled NV12")?;
        Ok((reader, mgr))
    }
}

#[cfg(test)]
mod tests {
    use super::plan;

    #[test]
    fn plans_fit_and_scale_down_with_bitrate() {
        let p = plan(10 << 20, 30.0, 192, 1920, 1080, 60).unwrap();
        assert!(p.video_kbps > 2000 && p.video_kbps < 2600, "{p:?}");
        assert_eq!((p.width, p.height, p.fps), (1280, 720, 30));
        let p = plan(10 << 20, 10.0, 192, 1920, 1080, 60).unwrap();
        assert_eq!((p.height, p.fps), (1080, 60));
        let p = plan(500 << 20, 30.0, 192, 2560, 1440, 144).unwrap();
        assert_eq!((p.height, p.fps), (1080, 60));
        let p = plan(10 << 20, 120.0, 192, 1920, 1080, 60).unwrap();
        assert_eq!(p.height, 480);
        assert!(plan(10 << 20, 600.0, 192, 1920, 1080, 60).is_err());
    }
}

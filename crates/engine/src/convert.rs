//! BGRA → NV12 (+ scaling) on the GPU via the D3D11 video processor, writing into a small
//! ring of NV12 textures the encoder reads from.

use crate::d3d::Gpu;
use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::core::Interface;

pub struct Converter {
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    pub out_width: u32,
    pub out_height: u32,
}

pub fn create_nv12_texture(gpu: &Gpu, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    if unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }.is_err() {
        // Some drivers reject VIDEO_ENCODER together with RENDER_TARGET.
        desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D(NV12)")?;
    }
    Ok(tex.unwrap())
}

impl Converter {
    pub fn new(
        gpu: &Gpu,
        src: &ID3D11Texture2D,
        in_w: u32,
        in_h: u32,
        out_w: u32,
        out_h: u32,
        fps: u32,
        pool: usize,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = gpu.device.cast()?;
        let video_context: ID3D11VideoContext = gpu.context.cast()?;
        let rate = DXGI_RATIONAL {
            Numerator: fps,
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("CreateVideoProcessorEnumerator")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                src,
                &enumerator,
                &in_desc,
                Some(&mut input_view),
            )
        }
        .context("CreateVideoProcessorInputView")?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut outputs = Vec::with_capacity(pool);
        for _ in 0..pool {
            let tex = create_nv12_texture(gpu, out_w, out_h)?;
            let mut view = None;
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &out_desc,
                    Some(&mut view),
                )
            }
            .context("CreateVideoProcessorOutputView")?;
            outputs.push((tex, view.unwrap()));
        }

        unsafe {
            // Input: full-range RGB. Output: BT.709 limited range (what players assume for HD).
            let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let ycc_709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &ycc_709_limited);
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // Letterbox when aspect ratios differ.
            let (dst, full) = fit_rect(in_w, in_h, out_w, out_h);
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&RECT {
                    left: 0,
                    top: 0,
                    right: in_w as i32,
                    bottom: in_h as i32,
                }),
            );
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
        }

        Ok(Self {
            video_context,
            processor,
            input_view: input_view.unwrap(),
            outputs,
            out_width: out_w,
            out_height: out_h,
        })
    }

    pub fn pool_len(&self) -> usize {
        self.outputs.len()
    }

    pub fn texture(&self, index: usize) -> &ID3D11Texture2D {
        &self.outputs[index].0
    }

    /// Converts the source texture into pool slot `index`.
    pub fn convert(&self, index: usize) -> Result<()> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let r = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.outputs[index].1,
                0,
                &streams,
            )
        };
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        r.context("VideoProcessorBlt")
    }
}

/// Destination rect that preserves aspect ratio inside `out_w × out_h`, plus the full rect.
fn fit_rect(in_w: u32, in_h: u32, out_w: u32, out_h: u32) -> (RECT, RECT) {
    let full = RECT {
        left: 0,
        top: 0,
        right: out_w as i32,
        bottom: out_h as i32,
    };
    let (iw, ih, ow, oh) = (in_w as u64, in_h as u64, out_w as u64, out_h as u64);
    if iw * oh == ih * ow {
        return (full, full);
    }
    if iw * oh > ih * ow {
        let h = (ow * ih / iw) as i32 & !1;
        let top = (out_h as i32 - h) / 2;
        (
            RECT {
                left: 0,
                top,
                right: out_w as i32,
                bottom: top + h,
            },
            full,
        )
    } else {
        let w = (oh * iw / ih) as i32 & !1;
        let left = (out_w as i32 - w) / 2;
        (
            RECT {
                left,
                top: 0,
                right: left + w,
                bottom: out_h as i32,
            },
            full,
        )
    }
}

/// Output size for a target height (0 = native), even dimensions, aspect preserved.
pub fn output_size(in_w: u32, in_h: u32, target_height: u32) -> (u32, u32) {
    if target_height == 0 || target_height >= in_h {
        return (in_w & !1, in_h & !1);
    }
    let w = (in_w as u64 * target_height as u64 / in_h as u64) as u32;
    (w & !1, target_height & !1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_keeps_aspect_and_even() {
        assert_eq!(output_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(output_size(2560, 1440, 1080), (1920, 1080));
        assert_eq!(output_size(1921, 1081, 0), (1920, 1080));
    }

    #[test]
    fn fit_rect_letterboxes() {
        let (dst, _) = fit_rect(1920, 1080, 1920, 1200);
        assert_eq!((dst.top, dst.bottom - dst.top), (60, 1080));
    }
}

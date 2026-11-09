//! Direct3D 11 helpers: device creation and a GPU colour converter / scaler
//! built on the D3D11 video processor (runs on the GPU's fixed-function video
//! block where available, never on the CPU).

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

#[derive(Clone)]
pub struct Device {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

// D3D11 devices are free-threaded and we enable multithread protection on the
// immediate context, so sharing them across threads is sound.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn new() -> Result<Self> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
        }
        let device: ID3D11Device = device.context("no device")?;
        let context: ID3D11DeviceContext = context.context("no context")?;
        // Media Foundation uses the device from its own threads.
        let mt: ID3D11Multithread = context.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        Ok(Self { device, context })
    }

    pub fn dxgi(&self) -> Result<IDXGIDevice> {
        Ok(self.device.cast()?)
    }

    pub fn texture(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        bind: D3D11_BIND_FLAG,
    ) -> Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        tex.context("CreateTexture2D")
    }

    /// Copies a BGRA texture to CPU memory as tightly packed RGBA.
    pub fn read_bgra_as_rgba(&self, tex: &ID3D11Texture2D) -> Result<(u32, u32, Vec<u8>)> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let staging_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            MipLevels: 1,
            ArraySize: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..desc
        };
        let mut staging = None;
        unsafe {
            self.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        }
        let staging = staging.context("staging")?;
        let (w, h) = (desc.Width, desc.Height);
        let mut out = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.context.CopyResource(&staging, tex);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(src.add(y * mapped.RowPitch as usize), w as usize * 4);
                let dst = &mut out[y * w as usize * 4..(y + 1) * w as usize * 4];
                for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
            self.context.Unmap(&staging, 0);
        }
        Ok((w, h, out))
    }
}

/// GPU colour conversion + scaling (e.g. BGRA screen capture -> NV12 for the
/// encoder, or NV12 decoded frame -> BGRA for display).
pub struct VideoConverter {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    pub out_w: u32,
    pub out_h: u32,
}

unsafe impl Send for VideoConverter {}

impl VideoConverter {
    pub fn new(dev: &Device, in_w: u32, in_h: u32, out_w: u32, out_h: u32, rgb_to_yuv: bool) -> Result<Self> {
        let video_device: ID3D11VideoDevice = dev.device.cast()?;
        let video_context: ID3D11VideoContext = dev.context.cast()?;
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };
        unsafe {
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            if let Ok(vc1) = video_context.cast::<ID3D11VideoContext1>() {
                let (cin, cout) = if rgb_to_yuv {
                    (DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709)
                } else {
                    (DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709)
                };
                vc1.VideoProcessorSetStreamColorSpace1(&processor, 0, cin);
                vc1.VideoProcessorSetOutputColorSpace1(&processor, cout);
            }
        }
        Ok(Self { video_device, video_context, enumerator, processor, out_w, out_h })
    }

    pub fn input_view(&self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(tex, &self.enumerator, &desc, Some(&mut view))?
        };
        view.context("input view")
    }

    /// Input view on one slice of a texture array (decoder output).
    pub fn input_view_slice(&self, tex: &ID3D11Texture2D, slice: u32) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(tex, &self.enumerator, &desc, Some(&mut view))?
        };
        view.context("input view")
    }

    pub fn output_view(&self, tex: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorOutputView(tex, &self.enumerator, &desc, Some(&mut view))?
        };
        view.context("output view")
    }

    /// Converts `src_rect` of the input into the output, letterboxed to keep
    /// the aspect ratio.
    pub fn blit(
        &self,
        input: &ID3D11VideoProcessorInputView,
        output: &ID3D11VideoProcessorOutputView,
        src_w: u32,
        src_h: u32,
    ) -> Result<()> {
        let src = RECT { left: 0, top: 0, right: src_w as i32, bottom: src_h as i32 };
        let dst = fit_rect(src_w, src_h, self.out_w, self.out_h);
        let full = RECT { left: 0, top: 0, right: self.out_w as i32, bottom: self.out_h as i32 };
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input.clone())),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let mut streams = [stream];
        let result = unsafe {
            self.video_context
                .VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&src));
            self.video_context
                .VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dst));
            self.video_context
                .VideoProcessorSetOutputTargetRect(&self.processor, true, Some(&full));
            self.video_context
                .VideoProcessorBlt(&self.processor, output, 0, &streams)
        };
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.context("VideoProcessorBlt")
    }
}

fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> RECT {
    let scale = (dst_w as f64 / src_w as f64).min(dst_h as f64 / src_h as f64);
    let w = (src_w as f64 * scale).round() as i32;
    let h = (src_h as f64 * scale).round() as i32;
    let x = (dst_w as i32 - w) / 2;
    let y = (dst_h as i32 - h) / 2;
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

/// Output size for a source, capped to `max_h` lines, even dimensions
/// (required by NV12 / H.264).
pub fn target_size(src_w: u32, src_h: u32, max_h: u32) -> (u32, u32) {
    let (w, h) = if src_h > max_h {
        let w = (src_w as u64 * max_h as u64 / src_h as u64) as u32;
        (w, max_h)
    } else {
        (src_w, src_h)
    };
    ((w.max(2) + 1) & !1, (h.max(2) + 1) & !1)
}

//! H.264 decoder through the Media Foundation decoder MFT, hardware accelerated
//! (DXVA / D3D11) when possible, with a software fallback.
//! Output: RGBA frames ready to be shown by the UI.

use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use super::d3d::{Device, VideoConverter};
use super::encoder::mf_startup;

pub struct DecodedFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

struct GpuOut {
    conv: VideoConverter,
    tex_w: u32,
    tex_h: u32,
    bgra: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    staging: ID3D11Texture2D,
    w: u32,
    h: u32,
}

/// H.264 decoder: Media Foundation (hardware, then software) when present,
/// otherwise the built-in OpenH264.
pub enum Decoder {
    Mf(MfDecoder),
    Soft(super::soft::SoftDecoder),
}

impl Decoder {
    pub fn new() -> Result<Self> {
        if super::soft::media_foundation_available() {
            match MfDecoder::new() {
                Ok(d) => return Ok(Self::Mf(d)),
                Err(e) => tracing::warn!("décodeur Media Foundation indisponible ({e:#}), passage à OpenH264"),
            }
        }
        Ok(Self::Soft(super::soft::SoftDecoder::new()?))
    }

    pub fn hardware(&self) -> bool {
        matches!(self, Self::Mf(d) if d.hardware)
    }

    /// Feeds one access unit, returns the newest decoded frame.
    pub fn decode(&mut self, data: &[u8], timestamp_us: u64, width: u32, height: u32) -> Result<Option<DecodedFrame>> {
        match self {
            Self::Mf(d) => d.decode(data, timestamp_us, width, height),
            Self::Soft(d) => d.decode(data),
        }
    }
}

pub struct MfDecoder {
    transform: IMFTransform,
    dev: Option<Device>,
    _manager: Option<IMFDXGIDeviceManager>,
    gpu: Option<GpuOut>,
    out_size: (u32, u32),
    stride: u32,
    pub hardware: bool,
}
unsafe impl Send for MfDecoder {}

impl MfDecoder {
    pub fn new() -> Result<Self> {
        match Self::create(true) {
            Ok(d) => Ok(d),
            Err(e) => {
                tracing::warn!("décodage matériel indisponible ({e:#}), passage en logiciel");
                Self::create(false)
            }
        }
    }

    fn create(hardware: bool) -> Result<Self> {
        mf_startup();
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)? };
        let attrs = unsafe { transform.GetAttributes()? };
        unsafe {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }
        if let Ok(codec) = transform.cast::<ICodecAPI>() {
            unsafe {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
            }
        }
        let (dev, manager) = if hardware {
            let aware = unsafe { attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) };
            anyhow::ensure!(aware != 0, "décodeur non compatible D3D11");
            let dev = Device::new()?;
            let mut token = 0u32;
            let mut manager = None;
            unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
            let manager = manager.context("device manager")?;
            unsafe {
                manager.ResetDevice(&dev.device, token)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            }
            (Some(dev), Some(manager))
        } else {
            (None, None)
        };

        unsafe {
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0).context("SetInputType H.264")?;
        }
        let mut d = Self { transform, dev, _manager: manager, gpu: None, out_size: (0, 0), stride: 0, hardware };
        d.set_output_type()?;
        unsafe {
            d.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(d)
    }

    fn set_output_type(&mut self) -> Result<()> {
        unsafe {
            for i in 0.. {
                let Ok(t) = self.transform.GetOutputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(0, &t, 0)?;
                    let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                    self.out_size = ((size >> 32) as u32, size as u32);
                    self.stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(self.out_size.0);
                    return Ok(());
                }
            }
        }
        anyhow::bail!("pas de sortie NV12")
    }

    /// Feeds one access unit. `width`/`height` are the real picture size (the
    /// decoder may pad to 16-line multiples). Returns the newest decoded frame.
    pub fn decode(&mut self, data: &[u8], timestamp_us: u64, width: u32, height: u32) -> Result<Option<DecodedFrame>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp_us as i64 * 10)?;
            if let Err(e) = self.transform.ProcessInput(0, &sample, 0)
                && e.code() != MF_E_NOTACCEPTING {
                    return Err(e.into());
                }
        }
        let mut last = None;
        loop {
            match self.pull(width, height)? {
                Pull::Frame(f) => last = Some(f),
                Pull::Again => continue,
                Pull::NeedInput => break,
            }
        }
        Ok(last)
    }

    fn pull(&mut self, width: u32, height: u32) -> Result<Pull> {
        unsafe {
            let info = self.transform.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            let sample = if provides {
                None
            } else {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1))?)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let res = self.transform.ProcessOutput(0, &mut out, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut out[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
            match res {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(Pull::NeedInput),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.set_output_type()?;
                    self.gpu = None;
                    return Ok(Pull::Again);
                }
                Err(e) => return Err(e.into()),
                Ok(()) => {}
            }
            let Some(sample) = sample else { return Ok(Pull::Again) };
            let w = width.min(self.out_size.0).max(2);
            let h = height.min(self.out_size.1).max(2);
            let buffer = sample.GetBufferByIndex(0)?;
            if let (Some(_), Ok(dxgi)) = (self.dev.as_ref(), buffer.cast::<IMFDXGIBuffer>()) {
                let mut raw = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw)?;
                let tex = ID3D11Texture2D::from_raw(raw);
                let slice = dxgi.GetSubresourceIndex()?;
                return Ok(Pull::Frame(self.gpu_convert(&tex, slice, w, h)?));
            }
            // Software path: NV12 in system memory.
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let src = std::slice::from_raw_parts(ptr, len as usize);
            let stride = self.stride.max(self.out_size.0) as usize;
            let rgba = nv12_to_rgba(src, stride, self.out_size.1 as usize, w as usize, h as usize);
            buffer.Unlock()?;
            Ok(Pull::Frame(DecodedFrame { width: w, height: h, rgba }))
        }
    }

    fn gpu_convert(&mut self, tex: &ID3D11Texture2D, slice: u32, w: u32, h: u32) -> Result<DecodedFrame> {
        let dev = self.dev.clone().unwrap();
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let stale = self
            .gpu
            .as_ref()
            .is_none_or(|g| g.tex_w != desc.Width || g.tex_h != desc.Height || g.w != w || g.h != h);
        if stale {
            let conv = VideoConverter::new(&dev, desc.Width, desc.Height, w, h, false)?;
            let bgra = dev.texture(w, h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET)?;
            let out_view = conv.output_view(&bgra)?;
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            unsafe { dev.device.CreateTexture2D(&staging_desc, None, Some(&mut staging))? };
            self.gpu = Some(GpuOut {
                conv,
                tex_w: desc.Width,
                tex_h: desc.Height,
                bgra,
                out_view,
                staging: staging.context("staging")?,
                w,
                h,
            });
        }
        let g = self.gpu.as_ref().unwrap();
        // Input views are cheap; the decoder rotates through array slices.
        let input = g.conv.input_view_slice(tex, slice)?;
        g.conv.blit(&input, &g.out_view, w, h)?;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        unsafe {
            dev.context.CopyResource(&g.staging, &g.bgra);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            dev.context.Map(&g.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let src = mapped.pData as *const u8;
            let row_bytes = w as usize * 4;
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(src.add(y * mapped.RowPitch as usize), row_bytes);
                let dst = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
                for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(row.chunks_exact(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = 255;
                }
            }
            dev.context.Unmap(&g.staging, 0);
        }
        Ok(DecodedFrame { width: w, height: h, rgba })
    }
}

enum Pull {
    Frame(DecodedFrame),
    Again,
    NeedInput,
}

/// BT.709 limited range NV12 -> RGBA (software fallback only).
fn nv12_to_rgba(src: &[u8], stride: usize, padded_h: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![255u8; w * h * 4];
    let uv_base = stride * padded_h;
    for y in 0..h {
        for x in 0..w {
            let yy = src.get(y * stride + x).copied().unwrap_or(16) as i32 - 16;
            let uv = uv_base + (y / 2) * stride + (x & !1);
            let u = src.get(uv).copied().unwrap_or(128) as i32 - 128;
            let v = src.get(uv + 1).copied().unwrap_or(128) as i32 - 128;
            let c = 298 * yy;
            let r = (c + 459 * v + 128) >> 8;
            let g = (c - 55 * u - 136 * v + 128) >> 8;
            let b = (c + 541 * u + 128) >> 8;
            let o = (y * w + x) * 4;
            out[o] = r.clamp(0, 255) as u8;
            out[o + 1] = g.clamp(0, 255) as u8;
            out[o + 2] = b.clamp(0, 255) as u8;
        }
    }
    out
}

//! Software H.264 (OpenH264, compiled in) used when Media Foundation or a
//! hardware encoder is missing — Windows "N" editions, VMs, very old GPUs.
//! Costs CPU instead of GPU, but needs nothing installed.

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::encoder::{BitRate, FrameRate, FrameType, RateControlMode, UsageType};
use openh264::formats::{YUVSlices, YUVSource};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::d3d::Device;
use super::decoder::DecodedFrame;
use super::encoder::{EncodedFrame, EncoderConfig};

pub struct SoftEncoder {
    enc: openh264::encoder::Encoder,
    dev: Device,
    staging: ID3D11Texture2D,
    w: usize,
    h: usize,
    u: Vec<u8>,
    v: Vec<u8>,
    on_output: Box<dyn FnMut(EncodedFrame) + Send>,
}
unsafe impl Send for SoftEncoder {}

impl SoftEncoder {
    pub fn new(dev: &Device, cfg: &EncoderConfig, on_output: impl FnMut(EncodedFrame) + Send + 'static) -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get().min(4)).unwrap_or(2) as u16;
        let config = openh264::encoder::EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(cfg.bitrate))
            .max_frame_rate(FrameRate::from_hz(cfg.fps as f32))
            .skip_frames(true)
            .num_threads(threads);
        let enc = openh264::encoder::Encoder::with_api_config(OpenH264API::from_source(), config)?;
        let staging = dev.texture_staging(cfg.width, cfg.height, DXGI_FORMAT_NV12)?;
        let (w, h) = (cfg.width as usize, cfg.height as usize);
        Ok(Self {
            enc,
            dev: dev.clone(),
            staging,
            w,
            h,
            u: vec![0; w / 2 * h / 2],
            v: vec![0; w / 2 * h / 2],
            on_output: Box::new(on_output),
        })
    }

    /// Same contract as the hardware encoder: always accepts the frame.
    pub fn encode(&mut self, nv12: &ID3D11Texture2D, timestamp_us: u64, keyframe: bool) -> Result<bool> {
        let (w, h) = (self.w, self.h);
        let ctx = &self.dev.context;
        let data = unsafe {
            ctx.CopyResource(&self.staging, nv12);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let pitch = m.RowPitch as usize;
            let base = m.pData as *const u8;
            let y = std::slice::from_raw_parts(base, pitch * h);
            // NV12: interleaved UV plane right after the Y plane.
            let uv = std::slice::from_raw_parts(base.add(pitch * h), pitch * h / 2);
            for row in 0..h / 2 {
                let src = &uv[row * pitch..row * pitch + w];
                let (du, dv) = (&mut self.u[row * w / 2..][..w / 2], &mut self.v[row * w / 2..][..w / 2]);
                for (i, p) in src.chunks_exact(2).enumerate() {
                    du[i] = p[0];
                    dv[i] = p[1];
                }
            }
            if keyframe {
                self.enc.force_intra_frame();
            }
            let yuv = YUVSlices::new((y, &self.u, &self.v), (w, h), (pitch, w / 2, w / 2));
            let res = self.enc.encode(&yuv).map(|b| (b.frame_type(), b.to_vec()));
            ctx.Unmap(&self.staging, 0);
            res?
        };
        let (kind, bytes) = data;
        if !bytes.is_empty() && !matches!(kind, FrameType::Skip | FrameType::Invalid) {
            (self.on_output)(EncodedFrame { data: bytes, keyframe: matches!(kind, FrameType::IDR), timestamp_us });
        }
        Ok(true)
    }
}

pub struct SoftDecoder {
    dec: openh264::decoder::Decoder,
}

impl SoftDecoder {
    pub fn new() -> Result<Self> {
        Ok(Self { dec: openh264::decoder::Decoder::new().context("OpenH264")? })
    }

    pub fn decode(&mut self, data: &[u8]) -> Result<Option<DecodedFrame>> {
        let Some(yuv) = self.dec.decode(data)? else { return Ok(None) };
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let (py, pu, pv) = (yuv.y(), yuv.u(), yuv.v());
        let mut rgba = vec![255u8; w * h * 4];
        // BT.709 limited range, matching the GPU converter on the sender side.
        for row in 0..h {
            let out = &mut rgba[row * w * 4..(row + 1) * w * 4];
            for (col, px) in out.chunks_exact_mut(4).enumerate() {
                let c = (py[row * sy + col] as i32 - 16) * 298;
                let d = pu[row / 2 * su + col / 2] as i32 - 128;
                let e = pv[row / 2 * sv + col / 2] as i32 - 128;
                px[0] = ((c + 459 * e + 128) >> 8).clamp(0, 255) as u8;
                px[1] = ((c - 55 * d - 136 * e + 128) >> 8).clamp(0, 255) as u8;
                px[2] = ((c + 541 * d + 128) >> 8).clamp(0, 255) as u8;
            }
        }
        Ok(Some(DecodedFrame { width: w as u32, height: h as u32, rgba }))
    }
}

/// Media Foundation is delay-loaded: calling it when mfplat.dll is missing
/// would crash, so every MF path checks this first.
pub fn media_foundation_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // MYSTREAM_FORCE_SOFTWARE=1 simulates a Windows "N" edition (tests).
    *AVAILABLE.get_or_init(|| {
        std::env::var_os("MYSTREAM_FORCE_SOFTWARE").is_none()
            && unsafe { windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::w!("mfplat.dll")).is_ok() }
    })
}

#[cfg(test)]
mod tests {
    /// Hardware-encoded stream decoded by OpenH264 (sender with GPU encoder,
    /// viewer on Windows "N"). Needs a GPU: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn soft_decodes_hardware_stream() {
        use crate::media::{capture, pipeline};
        crate::media::encoder::com_init();
        let src = capture::list_sources().into_iter().next().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let settings = pipeline::VideoSettings { fps: 30, max_height: 1080, bitrate: 5_000_000, always_encode: true };
        let (pipe, info) = pipeline::Pipeline::start(src.kind, settings, move |f| {
            let _ = tx.send(f.data);
        })
        .unwrap();
        assert!(!info.encoder_name.contains("OpenH264"), "test needs a hardware encoder");
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(pipe);
        let mut dec = super::SoftDecoder::new().unwrap();
        let decoded = rx.try_iter().filter(|d| matches!(dec.decode(d), Ok(Some(_)))).count();
        assert!(decoded > 30, "decoded {decoded} frames");
    }
}

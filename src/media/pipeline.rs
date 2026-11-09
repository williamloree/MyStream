//! Capture -> GPU colour conversion -> hardware encode, on a dedicated thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::capture::{Capturer, SourceKind, copy_region};
use super::d3d::{Device, VideoConverter, target_size};
use super::encoder::{EncodedFrame, Encoder, EncoderConfig, com_init};

#[derive(Clone, Debug)]
pub struct VideoSettings {
    pub fps: u32,
    pub max_height: u32,
    pub bitrate: u32,
    /// Encode every tick even when the screen did not change (benchmarks).
    pub always_encode: bool,
}

pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub encoder_name: String,
}

pub struct Stats {
    pub captured: AtomicU64,
    pub encoded: AtomicU64,
    pub bytes: AtomicU64,
}

pub struct Pipeline {
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    screenshot_tx: std::sync::mpsc::Sender<std::sync::mpsc::Sender<(u32, u32, Vec<u8>)>>,
    pub stats: Arc<Stats>,
}

impl Pipeline {
    /// Starts capturing. `on_frame` receives every encoded frame (called on the
    /// encoder thread, must be cheap).
    pub fn start(
        source: SourceKind,
        settings: VideoSettings,
        on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<(Self, VideoInfo)> {
        let stop = Arc::new(AtomicBool::new(false));
        let keyframe = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(Stats {
            captured: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        });
        let (info_tx, info_rx) = std::sync::mpsc::channel();
        let (shot_tx, shot_rx) = std::sync::mpsc::channel();
        let (stop2, kf2, stats2) = (stop.clone(), keyframe.clone(), stats.clone());
        let thread = std::thread::Builder::new().name("capture".into()).spawn(move || {
            com_init();
            if let Err(e) = run(source, settings, on_frame, stop2, kf2, stats2, shot_rx, &info_tx) {
                tracing::error!("capture: {e:#}");
                let _ = info_tx.send(Err(e));
            }
        })?;
        let info = info_rx.recv().map_err(|_| anyhow::anyhow!("capture thread died"))??;
        Ok((
            Self { stop, keyframe, thread: Some(thread), screenshot_tx: shot_tx, stats },
            info,
        ))
    }

    /// Set to `true` to get a keyframe as soon as possible.
    pub fn keyframe_flag(&self) -> Arc<AtomicBool> {
        self.keyframe.clone()
    }

    /// Full resolution RGBA copy of the current frame.
    pub fn screenshot(&self) -> Result<(u32, u32, Vec<u8>)> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.screenshot_tx.send(tx)?;
        Ok(rx.recv_timeout(Duration::from_secs(3))?)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const RING: usize = 4;

#[allow(clippy::too_many_arguments)]
fn run(
    source: SourceKind,
    settings: VideoSettings,
    mut on_frame: impl FnMut(EncodedFrame) + Send + 'static,
    stop: Arc<AtomicBool>,
    keyframe: Arc<AtomicBool>,
    stats: Arc<Stats>,
    shot_rx: std::sync::mpsc::Receiver<std::sync::mpsc::Sender<(u32, u32, Vec<u8>)>>,
    info_tx: &std::sync::mpsc::Sender<Result<VideoInfo>>,
) -> Result<()> {
    let dev = Device::new()?;
    let mut cap = Capturer::new(dev.clone(), &source, settings.fps)?;
    let (src_w, src_h) = cap.size();
    let (out_w, out_h) = target_size(src_w, src_h, settings.max_height);

    let stats_enc = stats.clone();
    let encoder = Encoder::new(
        &dev,
        &EncoderConfig { width: out_w, height: out_h, fps: settings.fps, bitrate: settings.bitrate },
        move |f| {
            stats_enc.encoded.fetch_add(1, Ordering::Relaxed);
            stats_enc.bytes.fetch_add(f.data.len() as u64, Ordering::Relaxed);
            on_frame(f)
        },
    )?;

    // Own copy of the latest captured frame: lets the capture pool reuse its
    // buffers immediately, and lets us re-encode a keyframe when the screen is
    // static and someone joins.
    let mut bgra_size = (src_w, src_h);
    let mut bgra = dev.texture(src_w, src_h, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
    let mut conv = VideoConverter::new(&dev, src_w, src_h, out_w, out_h, true)?;
    let mut input_view = conv.input_view(&bgra)?;
    let ring: Vec<ID3D11Texture2D> = (0..RING)
        .map(|_| dev.texture(out_w, out_h, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET))
        .collect::<Result<_>>()?;
    let mut out_views: Vec<_> = ring.iter().map(|t| conv.output_view(t)).collect::<Result<_>>()?;
    let mut ring_idx = 0usize;
    let mut content = (src_w, src_h);
    let mut have_frame = false;

    let _ = info_tx.send(Ok(VideoInfo { width: out_w, height: out_h, encoder_name: encoder.name.clone() }));

    let start = Instant::now();
    let interval = Duration::from_secs_f64(1.0 / settings.fps as f64);
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += interval;
        if next < Instant::now() {
            next = Instant::now() + interval;
        }

        let mut fresh = false;
        if let Some(frame) = cap.latest()? {
            stats.captured.fetch_add(1, Ordering::Relaxed);
            let (pw, ph) = cap.size();
            if (pw, ph) != bgra_size {
                bgra_size = (pw, ph);
                bgra = dev.texture(pw, ph, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
                conv = VideoConverter::new(&dev, pw, ph, out_w, out_h, true)?;
                input_view = conv.input_view(&bgra)?;
                out_views = ring.iter().map(|t| conv.output_view(t)).collect::<Result<_>>()?;
            }
            content = (frame.width, frame.height);
            copy_region(&dev, &frame.texture, &bgra, frame.width, frame.height);
            drop(frame);
            fresh = true;
            have_frame = true;
        }

        while let Ok(reply) = shot_rx.try_recv() {
            if have_frame {
                let copy = dev.texture(content.0, content.1, DXGI_FORMAT_B8G8R8A8_UNORM, D3D11_BIND_FLAG(0))?;
                copy_region(&dev, &bgra, &copy, content.0, content.1);
                if let Ok(img) = dev.read_bgra_as_rgba(&copy) {
                    let _ = reply.send(img);
                }
            }
        }

        let want_kf = keyframe.load(Ordering::Relaxed);
        if !have_frame || (!fresh && !want_kf && !settings.always_encode) {
            continue; // screen unchanged: zero GPU work
        }
        let nv12 = &ring[ring_idx];
        conv.blit(&input_view, &out_views[ring_idx], content.0, content.1)?;
        let ts = start.elapsed().as_micros() as u64;
        if encoder.encode(nv12, ts, want_kf)? {
            if want_kf {
                keyframe.store(false, Ordering::Relaxed);
            }
            ring_idx = (ring_idx + 1) % RING;
        }
    }
    Ok(())
}

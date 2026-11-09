//! System audio capture (WASAPI loopback, excluding MyStream's own output to
//! avoid echo) + Opus encoding, and Opus decoding + WASAPI playback.
//! Everything runs on the CPU and costs well under 1% of one core.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{IUnknown, Interface, Ref, implement};

use super::encoder::com_init;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// 20 ms Opus frames.
pub const FRAME: usize = 960;

pub(super) fn stereo_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 8,
        nBlockAlign: 8,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

pub(super) struct EventHandle(pub(super) HANDLE);
impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, windows::Win32::System::Com::IAgileObject)]
struct ActivateHandler(std::sync::mpsc::SyncSender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivateHandler_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let _ = self.0.try_send(());
        Ok(())
    }
}

impl windows::Win32::System::Com::IAgileObject_Impl for ActivateHandler_Impl {}

/// Loopback client capturing every process except our own.
fn process_loopback_client() -> Result<IAudioClient> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: unsafe { GetCurrentProcessId() },
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // ManuallyDrop: the blob points to `params` on our stack, PropVariantClear
    // must never try to free it.
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let handler: IActivateAudioInterfaceCompletionHandler = ActivateHandler(tx).into();
    let op = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*pv),
            &handler,
        )?
    };
    rx.recv_timeout(std::time::Duration::from_secs(5))
        .context("activation audio expirée")?;
    let mut hr = windows::core::HRESULT(0);
    let mut unk: Option<IUnknown> = None;
    unsafe { op.GetActivateResult(&mut hr, &mut unk)? };
    hr.ok()?;
    Ok(unk.context("pas de client audio")?.cast()?)
}

fn default_device(flow: EDataFlow) -> Result<IMMDevice> {
    device(flow, eConsole)
}

pub(super) fn device(flow: EDataFlow, role: ERole) -> Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    Ok(unsafe { enumerator.GetDefaultAudioEndpoint(flow, role)? })
}

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCapture {
    /// `on_packet` receives one Opus packet every 20 ms while audio plays.
    pub fn start(on_packet: impl FnMut(Vec<u8>) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("audio-capture".into()).spawn(move || {
            com_init();
            if let Err(e) = capture_loop(on_packet, &stop2, &ready_tx) {
                tracing::error!("capture audio: {e:#}");
                let _ = ready_tx.send(Err(e));
            }
        })?;
        ready_rx.recv().context("audio thread died")??;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn capture_loop(
    mut on_packet: impl FnMut(Vec<u8>),
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let fmt = stereo_format();
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    let client = match process_loopback_client().and_then(|c| unsafe {
        c.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("capture par processus indisponible ({e:#}), loopback global");
            let dev = default_device(eRender)?;
            let c: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
            unsafe { c.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 200_000, 0, &fmt, None)? };
            c
        }
    };
    let event = EventHandle(unsafe { CreateEventW(None, false, false, None)? });
    unsafe { client.SetEventHandle(event.0)? };
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };
    let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Stereo, opus::Application::Audio)?;
    encoder.set_bitrate(opus::Bitrate::Bits(96_000))?;
    unsafe { client.Start()? };
    let _ = ready.send(Ok(()));

    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * CHANNELS * 4);
    let mut out = vec![0u8; 4000];
    while !stop.load(Ordering::Relaxed) {
        unsafe { WaitForSingleObject(event.0, 100) };
        loop {
            let n = unsafe { capture.GetNextPacketSize()? };
            if n == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut bflags = 0u32;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut bflags, None, None)? };
            let count = frames as usize * CHANNELS;
            if bflags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                pcm.extend(std::iter::repeat_n(0.0, count));
            } else {
                pcm.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const f32, count) });
            }
            unsafe { capture.ReleaseBuffer(frames)? };
        }
        while pcm.len() >= FRAME * CHANNELS {
            let chunk: Vec<f32> = pcm.drain(..FRAME * CHANNELS).collect();
            // Pure silence is not worth sending.
            if chunk.iter().all(|s| s.abs() < 1e-5) {
                continue;
            }
            let len = encoder.encode_float(&chunk, &mut out)?;
            on_packet(out[..len].to_vec());
        }
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

/// Plays Opus packets on the default output device.
pub struct AudioPlayer {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    /// Volume in percent.
    pub volume: Arc<AtomicU32>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn start(volume: u32) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let vol = Arc::new(AtomicU32::new(volume));
        let (stop2, vol2) = (stop.clone(), vol.clone());
        let thread = std::thread::Builder::new().name("audio-play".into()).spawn(move || {
            com_init();
            if let Err(e) = play_loop(rx, &stop2, &vol2) {
                tracing::error!("lecture audio: {e:#}");
            }
        })?;
        Ok(Self { tx, stop, volume: vol, thread: Some(thread) })
    }

    pub fn push(&self, packet: Vec<u8>) {
        let _ = self.tx.send(packet);
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn play_loop(rx: std::sync::mpsc::Receiver<Vec<u8>>, stop: &AtomicBool, volume: &AtomicU32) -> Result<()> {
    let dev = default_device(eRender)?;
    let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    let fmt = stereo_format();
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            400_000,
            0,
            &fmt,
            None,
        )?
    };
    let event = EventHandle(unsafe { CreateEventW(None, false, false, None)? });
    unsafe { client.SetEventHandle(event.0)? };
    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let buffer_frames = unsafe { client.GetBufferSize()? };
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)?;
    let mut queue: VecDeque<f32> = VecDeque::new();
    let mut decoded = vec![0f32; FRAME * CHANNELS * 6];
    // Jitter buffer: wait for 60 ms before (re)starting, never exceed 200 ms.
    let prebuffer = SAMPLE_RATE as usize * CHANNELS * 60 / 1000;
    let max_queue = SAMPLE_RATE as usize * CHANNELS * 200 / 1000;
    let mut playing = false;
    unsafe { client.Start()? };
    while !stop.load(Ordering::Relaxed) {
        unsafe { WaitForSingleObject(event.0, 50) };
        while let Ok(p) = rx.try_recv() {
            if let Ok(n) = decoder.decode_float(&p, &mut decoded, false) {
                queue.extend(&decoded[..n * CHANNELS]);
            }
        }
        while queue.len() > max_queue {
            queue.pop_front();
        }
        if !playing && queue.len() >= prebuffer {
            playing = true;
        }
        let padding = unsafe { client.GetCurrentPadding()? };
        let avail = buffer_frames.saturating_sub(padding);
        if avail == 0 {
            continue;
        }
        let data = unsafe { render.GetBuffer(avail)? } as *mut f32;
        let out = unsafe { std::slice::from_raw_parts_mut(data, avail as usize * CHANNELS) };
        let gain = volume.load(Ordering::Relaxed) as f32 / 100.0;
        for s in out.iter_mut() {
            *s = if playing { queue.pop_front().map(|v| v * gain).unwrap_or(0.0) } else { 0.0 };
        }
        if playing && queue.is_empty() {
            playing = false;
        }
        unsafe { render.ReleaseBuffer(avail, 0)? };
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

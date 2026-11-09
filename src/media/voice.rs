//! Voice chat: microphone capture (WASAPI) -> RNNoise -> voice activity
//! detection or push-to-talk -> Opus, and a low-latency mixer playing every
//! participant through one output stream. Nothing is ever recorded.
//!
//! Latency budget: 20 ms Opus frames, ~10 ms capture/render periods and a
//! 40 ms jitter buffer, i.e. about 80 ms plus network transit.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::audio::{EventHandle, SAMPLE_RATE, device};
use super::encoder::com_init;

/// 20 ms of mono audio at 48 kHz.
const FRAME: usize = 960;
/// RNNoise works on 10 ms blocks.
const BLOCK: usize = nnnoiseless::FRAME_SIZE;
/// Keep the gate open this long after the last voiced block.
const HANGOVER_BLOCKS: u32 = 40;
const PREBUFFER: usize = SAMPLE_RATE as usize * 40 / 1000;
const MAX_QUEUE: usize = SAMPLE_RATE as usize * 120 / 1000;
const SPEAKING_HOLD: Duration = Duration::from_millis(300);

/// Settings and live state shared between the UI and the audio threads.
pub struct VoiceShared {
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub push_to_talk: AtomicBool,
    /// Windows virtual-key code for push-to-talk.
    pub ptt_key: AtomicU32,
    /// Voice detection threshold, 0 (very sensitive) to 100.
    pub threshold: AtomicU32,
    pub denoise: AtomicBool,
    /// Current microphone level, 0..100 (for the meter).
    pub level: AtomicU32,
    pub speaking: AtomicBool,
    /// Output volume in percent.
    pub volume: AtomicU32,
    /// Per-person volume in percent (0 = muted locally).
    pub gains: Mutex<HashMap<String, u32>>,
    /// Last time each person was heard.
    pub heard: Mutex<HashMap<String, Instant>>,
}

impl VoiceShared {
    pub fn new(s: &crate::store::VoiceSettings) -> Arc<Self> {
        Arc::new(Self {
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            push_to_talk: AtomicBool::new(s.push_to_talk),
            ptt_key: AtomicU32::new(s.ptt_key),
            threshold: AtomicU32::new(s.threshold),
            denoise: AtomicBool::new(s.denoise),
            level: AtomicU32::new(0),
            speaking: AtomicBool::new(false),
            volume: AtomicU32::new(s.volume),
            gains: Mutex::new(s.gains.clone()),
            heard: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_speaking(&self, id: &str) -> bool {
        self.heard.lock().unwrap().get(id).is_some_and(|t| t.elapsed() < SPEAKING_HOLD)
    }
}

pub fn key_down(vk: u32) -> bool {
    unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

/// Human name of a virtual-key code.
pub fn key_name(vk: u32) -> String {
    match vk {
        0x04 => "Clic molette".into(),
        0x05 => "Souris 4".into(),
        0x06 => "Souris 5".into(),
        0x10 | 0xA0 | 0xA1 => "Maj".into(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".into(),
        0x12 | 0xA4 | 0xA5 => "Alt".into(),
        0x14 => "Verr. Maj".into(),
        0x20 => "Espace".into(),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x60..=0x69 => format!("Pavé {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0xC0 => "²".into(),
        _ => format!("Touche {vk}"),
    }
}

/// Returns the first pressed key (keyboard or mouse buttons 3-5), if any.
/// Used to bind push-to-talk.
pub fn pressed_key() -> Option<u32> {
    (0x04..=0xFEu32)
        .filter(|vk| !matches!(vk, 0x07 | 0x0A..=0x0F | 0x5B | 0x5C | 0x5D | 0x1B))
        .find(|&vk| key_down(vk))
}

// ---------------------------------------------------------------- microphone

pub struct Mic {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mic {
    /// `on_packet` receives one Opus packet (20 ms) whenever the user talks.
    pub fn start(
        shared: Arc<VoiceShared>,
        repaint: Arc<dyn Fn() + Send + Sync>,
        on_packet: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("voice-mic".into()).spawn(move || {
            com_init();
            if let Err(e) = mic_loop(&shared, &*repaint, on_packet, &stop2, &ready_tx) {
                tracing::error!("micro: {e:#}");
                let _ = ready_tx.send(Err(e));
            }
            shared.speaking.store(false, Ordering::Relaxed);
            shared.level.store(0, Ordering::Relaxed);
        })?;
        ready_rx.recv().context("thread micro arrêté")??;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn mono_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: 1,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

/// Maps the 0..100 threshold setting to a level in dBFS.
fn threshold_db(t: u32) -> f32 {
    -70.0 + t.min(100) as f32 * 0.5
}

fn mic_loop(
    shared: &VoiceShared,
    repaint: &(dyn Fn() + Send + Sync),
    mut on_packet: impl FnMut(Vec<u8>),
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let dev = device(eCapture, eCommunications).context("aucun micro")?;
    let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    let fmt = mono_format();
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000,
            0,
            &fmt,
            None,
        )?
    };
    let event = EventHandle(unsafe { CreateEventW(None, false, false, None)? });
    unsafe { client.SetEventHandle(event.0)? };
    let capture: IAudioCaptureClient = unsafe { client.GetService()? };

    let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
    encoder.set_bitrate(opus::Bitrate::Bits(40_000))?;
    encoder.set_inband_fec(true)?;
    encoder.set_packet_loss_perc(10)?;
    let mut denoiser = nnnoiseless::DenoiseState::new();
    unsafe { client.Start()? };
    let _ = ready.send(Ok(()));

    let mut input: Vec<f32> = Vec::with_capacity(FRAME * 4);
    let mut frame: Vec<f32> = Vec::with_capacity(FRAME);
    let mut scaled = [0f32; BLOCK];
    let mut cleaned = [0f32; BLOCK];
    let mut out = vec![0u8; 1500];
    let mut hangover = 0u32;
    // MYSTREAM_VOICE_TEST=1 transmits continuously (selftest, no one talking).
    let always_open = std::env::var_os("MYSTREAM_VOICE_TEST").is_some();
    let mut voiced_in_frame = false;
    let mut was_speaking = false;
    while !stop.load(Ordering::Relaxed) {
        unsafe { WaitForSingleObject(event.0, 100) };
        loop {
            let n = unsafe { capture.GetNextPacketSize()? };
            if n == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)? };
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                input.extend(std::iter::repeat_n(0.0, frames as usize));
            } else {
                input.extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const f32, frames as usize) });
            }
            unsafe { capture.ReleaseBuffer(frames)? };
        }

        let mut consumed = 0;
        while input.len() - consumed >= BLOCK {
            let block = &input[consumed..consumed + BLOCK];
            consumed += BLOCK;
            let denoise = shared.denoise.load(Ordering::Relaxed);
            let prob = if denoise {
                for (d, s) in scaled.iter_mut().zip(block) {
                    *d = s * 32768.0;
                }
                let p = denoiser.process_frame(&mut cleaned, &scaled);
                for c in cleaned.iter_mut() {
                    *c /= 32768.0;
                }
                p
            } else {
                cleaned.copy_from_slice(block);
                1.0
            };
            let rms = (cleaned.iter().map(|s| s * s).sum::<f32>() / BLOCK as f32).sqrt();
            let db = 20.0 * rms.max(1e-9).log10();
            let level = ((db + 70.0) / 70.0 * 100.0).clamp(0.0, 100.0) as u32;
            shared.level.store(level, Ordering::Relaxed);

            let open = if always_open {
                true
            } else if shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed) {
                hangover = 0;
                false
            } else if shared.push_to_talk.load(Ordering::Relaxed) {
                hangover = 0;
                key_down(shared.ptt_key.load(Ordering::Relaxed))
            } else {
                let voiced = db > threshold_db(shared.threshold.load(Ordering::Relaxed)) && (!denoise || prob > 0.4);
                if voiced {
                    hangover = HANGOVER_BLOCKS;
                } else {
                    hangover = hangover.saturating_sub(1);
                }
                hangover > 0
            };
            voiced_in_frame |= open;
            frame.extend_from_slice(&cleaned);
            if frame.len() == FRAME {
                if voiced_in_frame {
                    let len = encoder.encode_float(&frame, &mut out)?;
                    on_packet(out[..len].to_vec());
                }
                if voiced_in_frame != was_speaking {
                    was_speaking = voiced_in_frame;
                    shared.speaking.store(was_speaking, Ordering::Relaxed);
                    repaint();
                }
                voiced_in_frame = false;
                frame.clear();
            }
        }
        input.drain(..consumed);
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

// ---------------------------------------------------------------- mixer

struct Source {
    decoder: opus::Decoder,
    queue: VecDeque<f32>,
    playing: bool,
    next_seq: Option<u32>,
}

pub struct Mixer {
    tx: std::sync::mpsc::Sender<(String, u32, Vec<u8>)>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Mixer {
    pub fn start(shared: Arc<VoiceShared>, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let thread = std::thread::Builder::new().name("voice-mix".into()).spawn(move || {
            com_init();
            if let Err(e) = mix_loop(rx, &shared, &*repaint, &stop2) {
                tracing::error!("sortie voix: {e:#}");
            }
        })?;
        Ok(Self { tx, stop, thread: Some(thread) })
    }

    pub fn push(&self, from: String, seq: u32, packet: Vec<u8>) {
        let _ = self.tx.send((from, seq, packet));
    }
}

impl Drop for Mixer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn mix_loop(
    rx: std::sync::mpsc::Receiver<(String, u32, Vec<u8>)>,
    shared: &VoiceShared,
    repaint: &(dyn Fn() + Send + Sync),
    stop: &AtomicBool,
) -> Result<()> {
    let dev = device(eRender, eConsole)?;
    let client: IAudioClient = unsafe { dev.Activate(CLSCTX_ALL, None)? };
    let fmt = super::audio::stereo_format();
    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            300_000,
            0,
            &fmt,
            None,
        )?
    };
    let event = EventHandle(unsafe { CreateEventW(None, false, false, None)? });
    unsafe { client.SetEventHandle(event.0)? };
    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let buffer_frames = unsafe { client.GetBufferSize()? };
    let mut sources: HashMap<String, Source> = HashMap::new();
    let mut decoded = vec![0f32; FRAME * 6];
    let mut speaking_last: HashMap<String, bool> = HashMap::new();
    unsafe { client.Start()? };
    while !stop.load(Ordering::Relaxed) {
        unsafe { WaitForSingleObject(event.0, 50) };
        while let Ok((from, seq, pkt)) = rx.try_recv() {
            let src = match sources.entry(from.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => v.insert(Source {
                    decoder: opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono)?,
                    queue: VecDeque::new(),
                    playing: false,
                    next_seq: None,
                }),
            };
            // A new talk spurt starts with a fresh sequence: only treat small
            // gaps as losses.
            let gap = src.next_seq.map(|n| seq.wrapping_sub(n)).unwrap_or(0);
            if gap > 0x8000_0000 {
                continue; // late or duplicate
            }
            if src.playing && (1..=3).contains(&gap) {
                for i in 0..gap {
                    let fec = i == gap - 1;
                    let input: &[u8] = if fec { &pkt } else { &[] };
                    if let Ok(n) = src.decoder.decode_float(input, &mut decoded[..FRAME], fec) {
                        src.queue.extend(&decoded[..n]);
                    }
                }
            }
            if let Ok(n) = src.decoder.decode_float(&pkt, &mut decoded, false) {
                src.queue.extend(&decoded[..n]);
            }
            src.next_seq = Some(seq.wrapping_add(1));
            shared.heard.lock().unwrap().insert(from, Instant::now());
        }
        for s in sources.values_mut() {
            // Cap latency: drop the oldest audio when the network bunches up.
            if s.queue.len() > MAX_QUEUE {
                let extra = s.queue.len() - PREBUFFER;
                s.queue.drain(..extra);
            }
            if !s.playing && s.queue.len() >= PREBUFFER {
                s.playing = true;
            }
        }

        let padding = unsafe { client.GetCurrentPadding()? };
        let avail = buffer_frames.saturating_sub(padding) as usize;
        // Keep at most ~20 ms queued in the device.
        let target = (SAMPLE_RATE as usize * 20 / 1000).saturating_sub(padding as usize);
        let n = avail.min(target);
        if n > 0 {
            let data = unsafe { render.GetBuffer(n as u32)? } as *mut f32;
            let out = unsafe { std::slice::from_raw_parts_mut(data, n * 2) };
            out.fill(0.0);
            let deaf = shared.deafened.load(Ordering::Relaxed);
            let master = shared.volume.load(Ordering::Relaxed) as f32 / 100.0;
            let gains = shared.gains.lock().unwrap().clone();
            for (id, s) in sources.iter_mut() {
                if !s.playing {
                    continue;
                }
                let gain = master * gains.get(id).copied().unwrap_or(100) as f32 / 100.0;
                for i in 0..n {
                    let Some(v) = s.queue.pop_front() else {
                        s.playing = false;
                        break;
                    };
                    if !deaf {
                        out[i * 2] += v * gain;
                        out[i * 2 + 1] += v * gain;
                    }
                }
            }
            for v in out.iter_mut() {
                *v = v.clamp(-1.0, 1.0);
            }
            unsafe { render.ReleaseBuffer(n as u32, 0)? };
        }

        // Speaking indicators: repaint only on changes.
        let mut changed = false;
        {
            let heard = shared.heard.lock().unwrap();
            for (id, t) in heard.iter() {
                let now = t.elapsed() < SPEAKING_HOLD;
                if speaking_last.insert(id.clone(), now) != Some(now) {
                    changed = true;
                }
            }
        }
        if changed {
            repaint();
        }
    }
    unsafe {
        let _ = client.Stop();
    }
    Ok(())
}

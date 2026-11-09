//! Hardware H.264 encoder through Media Foundation (NVENC / AMF / QuickSync,
//! whichever the GPU vendor ships). Input is an NV12 D3D11 texture: the frame
//! never leaves the GPU.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

use super::d3d::Device;

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: u64,
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

struct Shared {
    transform: IMFTransform,
    need_input: u32,
}
unsafe impl Send for Shared {}

pub struct Encoder {
    shared: Arc<Mutex<Shared>>,
    codec: ICodecAPI,
    event_thread: Option<std::thread::JoinHandle<()>>,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
}
unsafe impl Send for Encoder {}

pub fn mf_startup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

pub fn com_init() {
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn set_codec(codec: &ICodecAPI, api: &GUID, v: VARIANT) {
    unsafe {
        let _ = codec.SetValue(api, &v);
    }
}

fn friendly_name(act: &IMFActivate) -> String {
    unsafe {
        let Ok(mut len) = act.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) else {
            return "encodeur matériel".into();
        };
        let mut buf = vec![0u16; len as usize + 1];
        if act
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len))
            .is_err()
        {
            return "encodeur matériel".into();
        }
        String::from_utf16_lossy(&buf[..len as usize])
    }
}

impl Encoder {
    /// `on_output` is called from the encoder's event thread for every
    /// encoded access unit.
    pub fn new(
        dev: &Device,
        cfg: &EncoderConfig,
        on_output: impl FnMut(EncodedFrame) + Send + 'static,
    ) -> Result<Self> {
        if !super::soft::media_foundation_available() {
            bail!("Media Foundation absent");
        }
        mf_startup();
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&output_info),
                &mut activates,
                &mut count,
            )?;
        }
        if count == 0 {
            bail!("aucun encodeur H.264 matériel trouvé sur ce GPU");
        }
        let list = unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
        let act = list[0].clone().context("activate")?;
        let name = friendly_name(&act);
        let transform: IMFTransform = unsafe { act.ActivateObject()? };
        for a in list.iter_mut() {
            drop(a.take());
        }
        unsafe { CoTaskMemFree(Some(activates as *const _)) };

        let attrs = unsafe { transform.GetAttributes()? };
        unsafe {
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
        }

        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager)? };
        let manager = manager.context("device manager")?;
        unsafe {
            manager.ResetDevice(&dev.device, token)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
        }

        let codec: ICodecAPI = transform.cast()?;
        set_codec(&codec, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32));
        set_codec(&codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(cfg.bitrate));
        set_codec(&codec, &CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(cfg.bitrate * 3 / 2));
        set_codec(&codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        set_codec(&codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(cfg.fps * 10));
        set_codec(&codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
        set_codec(&codec, &CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(0u32));

        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(0, &out, 0).context("SetOutputType H.264")?;

            let mut set = false;
            for i in 0.. {
                let Ok(t) = transform.GetInputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    t.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height))?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1))?;
                    transform.SetInputType(0, &t, 0).context("SetInputType NV12")?;
                    set = true;
                    break;
                }
            }
            if !set {
                bail!("l'encodeur n'accepte pas le NV12");
            }

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        let shared = Arc::new(Mutex::new(Shared { transform: transform.clone(), need_input: 0 }));
        let events: IMFMediaEventGenerator = transform.cast()?;
        let events = SendBox(events);
        let shared2 = shared.clone();
        let event_thread = std::thread::Builder::new()
            .name("encoder-events".into())
            .spawn(move || {
                com_init();
                let events = events;
                if let Err(e) = event_loop(&events.0, &shared2, on_output) {
                    tracing::debug!("encoder event loop ended: {e:#}");
                }
            })?;

        tracing::info!("encodeur: {name} {}x{} @{} fps, {} kb/s", cfg.width, cfg.height, cfg.fps, cfg.bitrate / 1000);
        Ok(Self { shared, codec, event_thread: Some(event_thread), _manager: manager, name })
    }

    /// Submits an NV12 texture. Returns `false` if the encoder is still busy
    /// with previous frames (the frame is then simply skipped).
    pub fn encode(&self, nv12: &ID3D11Texture2D, timestamp_us: u64, keyframe: bool) -> Result<bool> {
        let mut s = self.shared.lock().unwrap();
        if s.need_input == 0 {
            return Ok(false);
        }
        if keyframe {
            set_codec(&self.codec, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32));
        }
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, nv12, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                let len = b2.GetContiguousLength()?;
                buffer.SetCurrentLength(len)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp_us as i64 * 10)?;
            sample.SetSampleDuration(10_000_000 / 30)?;
            s.transform.ProcessInput(0, &sample, 0)?;
        }
        s.need_input -= 1;
        Ok(true)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Shutting the MFT down makes the blocking GetEvent() in the event
        // thread return an error, which ends it.
        {
            let s = self.shared.lock().unwrap();
            unsafe {
                let _ = s.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                if let Ok(shutdown) = s.transform.cast::<IMFShutdown>() {
                    let _ = shutdown.Shutdown();
                }
            }
        }
        if let Some(t) = self.event_thread.take() {
            let _ = t.join();
        }
    }
}

struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

fn event_loop(
    events: &IMFMediaEventGenerator,
    shared: &Arc<Mutex<Shared>>,
    mut on_output: impl FnMut(EncodedFrame),
) -> Result<()> {
    let mut seq_header: Option<Vec<u8>> = None;
    loop {
        let ev = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
        let ty = unsafe { ev.GetType()? };
        if ty == METransformNeedInput.0 as u32 {
            shared.lock().unwrap().need_input += 1;
        } else if ty == METransformHaveOutput.0 as u32 {
            let s = shared.lock().unwrap();
            if let Some(frame) = pull_output(&s.transform, &mut seq_header)? {
                drop(s);
                on_output(frame);
            }
        } else if ty == METransformDrainComplete.0 as u32 {
            return Ok(());
        }
    }
}

fn pull_output(transform: &IMFTransform, seq_header: &mut Option<Vec<u8>>) -> Result<Option<EncodedFrame>> {
    unsafe {
        let info = transform.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
        let sample = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            let b = MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?;
            s.AddBuffer(&b)?;
            Some(s)
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: std::mem::ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: std::mem::ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let res = transform.ProcessOutput(0, &mut out, &mut status);
        let sample = std::mem::ManuallyDrop::take(&mut out[0].pSample);
        drop(std::mem::ManuallyDrop::take(&mut out[0].pEvents));
        if let Err(e) = res {
            if e.code() == MF_E_TRANSFORM_STREAM_CHANGE {
                let t = transform.GetOutputAvailableType(0, 0)?;
                transform.SetOutputType(0, &t, 0)?;
                return Ok(None);
            }
            if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                return Ok(None);
            }
            return Err(e.into());
        }
        let Some(sample) = sample else { return Ok(None) };
        if seq_header.is_none()
            && let Ok(t) = transform.GetOutputCurrentType(0) {
                let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).unwrap_or(0);
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() {
                        *seq_header = Some(buf);
                    }
                }
            }
        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let time = sample.GetSampleTime().unwrap_or(0);
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let mut data = Vec::with_capacity(len as usize + 64);
        let bytes = std::slice::from_raw_parts(ptr, len as usize);
        if keyframe && !super::h264::contains_sps(bytes)
            && let Some(h) = seq_header.as_ref() {
                data.extend_from_slice(h);
            }
        data.extend_from_slice(bytes);
        buffer.Unlock()?;
        Ok(Some(EncodedFrame { data, keyframe, timestamp_us: (time / 10) as u64 }))
    }
}

//! Screen / window capture through Windows.Graphics.Capture (WGC).
//! Frames stay on the GPU as D3D11 textures; nothing is copied to the CPU.

use anyhow::{Context, Result, bail};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::core::BOOL;
const TRUE: BOOL = BOOL(1);
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::Interface;

use super::d3d::Device;

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    Monitor(isize),
    Window(isize),
}

#[derive(Clone, Debug)]
pub struct Source {
    pub kind: SourceKind,
    pub name: String,
}

pub fn list_sources() -> Vec<Source> {
    let mut out = Vec::new();
    unsafe extern "system" fn mon_cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Source>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let mut name = format!("Écran {}", out.len() + 1);
        if unsafe { GetMonitorInfoW(m, &mut info.monitorInfo) }.as_bool() {
            let r = info.monitorInfo.rcMonitor;
            let primary = info.monitorInfo.dwFlags & 1 != 0;
            name = format!(
                "Écran {} ({}x{}){}",
                out.len() + 1,
                r.right - r.left,
                r.bottom - r.top,
                if primary { " — principal" } else { "" }
            );
        }
        out.push(Source { kind: SourceKind::Monitor(m.0 as isize), name });
        TRUE
    }
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(mon_cb), LPARAM(&mut out as *mut _ as isize));
    }

    let mut windows: Vec<Source> = Vec::new();
    unsafe extern "system" fn win_cb(hwnd: HWND, data: LPARAM) -> BOOL {
        let out = unsafe { &mut *(data.0 as *mut Vec<Source>) };
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex & WS_EX_TOOLWINDOW.0 != 0 {
                return TRUE;
            }
            if GetAncestor(hwnd, GA_ROOT) != hwnd {
                return TRUE;
            }
            let mut cloaked = 0u32;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut _,
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return TRUE;
            }
            let len = GetWindowTextLengthW(hwnd);
            if len == 0 {
                return TRUE;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = GetWindowTextW(hwnd, &mut buf);
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            if title == "Program Manager" || title == "MyStream" || title.starts_with("MyStream (") {
                return TRUE;
            }
            out.push(Source { kind: SourceKind::Window(hwnd.0 as isize), name: title });
        }
        TRUE
    }
    unsafe {
        let _ = EnumWindows(Some(win_cb), LPARAM(&mut windows as *mut _ as isize));
    }
    out.extend(windows);
    out
}

fn create_item(kind: &SourceKind) -> Result<GraphicsCaptureItem> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item = unsafe {
        match kind {
            SourceKind::Monitor(h) => interop.CreateForMonitor(HMONITOR(*h as _))?,
            SourceKind::Window(h) => interop.CreateForWindow(HWND(*h as _))?,
        }
    };
    Ok(item)
}

pub struct Capturer {
    _winrt_device: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    winrt_device: IDirect3DDevice,
    pool_size: SizeInt32,
}

unsafe impl Send for Capturer {}

pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    // Keeps the frame alive (and out of the pool) while we read the texture.
    _frame: windows::Graphics::Capture::Direct3D11CaptureFrame,
}

impl Capturer {
    pub fn new(dev: Device, kind: &SourceKind, fps: u32) -> Result<Self> {
        let item = create_item(kind).context("création de la source de capture")?;
        let dxgi = dev.dxgi()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let winrt_device: IDirect3DDevice = inspectable.cast()?;
        let size = item.Size()?;
        if size.Width <= 0 || size.Height <= 0 {
            bail!("source vide (fenêtre réduite ?)");
        }
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(true);
        // Windows 11: no yellow border, and ask DWM to produce frames no faster
        // than we consume them (saves GPU composition work).
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan {
            Duration: 10_000_000 / fps.max(1) as i64,
        });
        session.StartCapture()?;
        Ok(Self {
            _winrt_device: winrt_device.clone(),
            _item: item,
            pool,
            session,
            winrt_device,
            pool_size: size,
        })
    }

    /// Returns the most recent frame since the last call, or `None` when the
    /// screen did not change (nothing to encode at all in that case).
    pub fn latest(&mut self) -> Result<Option<CapturedFrame>> {
        let mut last = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(prev) = last.replace(frame) {
                let _ = prev.Close();
            }
        }
        let Some(frame) = last else { return Ok(None) };
        let content = frame.ContentSize()?;
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            // Window was resized: recreate the pool at the new size.
            if content.Width > 0 && content.Height > 0 {
                self.pool_size = content;
                self.pool.Recreate(
                    &self.winrt_device,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    content,
                )?;
            }
        }
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let w = (content.Width.max(2) as u32).min(self.pool_size.Width as u32);
        let h = (content.Height.max(2) as u32).min(self.pool_size.Height as u32);
        Ok(Some(CapturedFrame { texture, width: w, height: h, _frame: frame }))
    }

    pub fn size(&self) -> (u32, u32) {
        (self.pool_size.Width as u32, self.pool_size.Height as u32)
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// One-shot full resolution grab of a source, as RGBA.
pub fn grab_once(kind: &SourceKind) -> Result<(u32, u32, Vec<u8>)> {
    let dev = Device::new()?;
    let mut cap = Capturer::new(dev.clone(), kind, 60)?;
    let start = std::time::Instant::now();
    loop {
        if let Some(frame) = cap.latest()? {
            let (w, h) = (frame.width, frame.height);
            let copy = dev.texture(
                w,
                h,
                windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                windows::Win32::Graphics::Direct3D11::D3D11_BIND_FLAG(0),
            )?;
            copy_region(&dev, &frame.texture, &copy, w, h);
            return dev.read_bgra_as_rgba(&copy);
        }
        if start.elapsed().as_secs() > 3 {
            bail!("aucune image reçue");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub fn copy_region(dev: &Device, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, w: u32, h: u32) {
    let b = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: w,
        bottom: h,
        back: 1,
    };
    unsafe { dev.context.CopySubresourceRegion(dst, 0, 0, 0, 0, src, 0, Some(&b)) };
}

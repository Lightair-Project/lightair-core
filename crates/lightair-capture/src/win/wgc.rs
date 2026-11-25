//! Windows Graphics Capture: a display or one window, into a free-threaded frame pool whose
//! frames are copied into the capture's shared textures as they arrive.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use parking_lot::Mutex;
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::core::{IInspectable, Interface};

use super::gpu::{Gpu, Out, Publisher};
use super::screen::Monitor;
use super::{Deliver, com, fail, hns_ns, host_now_ns, interval_ns};
use crate::screen::Config;
use crate::{Error, Frame, PixelFormat};

/// Buffers in Windows Graphics Capture's own pool: each frame is copied out and given back at
/// once, so two are enough.
const POOL_BUFFERS: i32 = 2;

struct State {
    deliver: Arc<dyn Deliver>,
    device: IDirect3DDevice,
    publisher: Mutex<Publisher>,
    config: Mutex<Config>,
    /// The frame pool's texture size now (it follows the window's).
    pool_size: Mutex<SizeInt32>,
    pixel: DirectXPixelFormat,
    /// The picture size last delivered.
    size: Mutex<(u32, u32)>,
    /// When the last frame was delivered (host ns), for the frame rate limit.
    last_ns: AtomicI64,
}

// Windows Runtime objects used here are agile, Direct3D is free-threaded.
unsafe impl Send for State {}
unsafe impl Sync for State {}

/// A running Windows Graphics Capture.
pub struct Capture {
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived: i64,
    closed: i64,
    state: Arc<State>,
}

unsafe impl Send for Capture {}
unsafe impl Sync for Capture {}

/// Asks once whether this app may capture without the yellow border (Windows 11: allowed
/// without a prompt for desktop apps). Whether it's given shows only in whether the border goes.
fn ask_borderless() {
    static ASKED: OnceLock<()> = OnceLock::new();
    ASKED.get_or_init(|| {
        if let Ok(asking) = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless) {
            let _ = asking.join();
        }
    });
}

impl Capture {
    pub fn monitor(monitor: &Monitor, config: &Config, deliver: Arc<dyn Deliver>) -> Result<Capture, Error> {
        com();
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| fail("opening Windows Graphics Capture", e))?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(monitor.handle) }
            .map_err(|e| fail(&format!("capturing {}", monitor.display.name), e))?;
        Capture::start(item, false, config, deliver)
    }

    pub fn window(window: HWND, config: &Config, deliver: Arc<dyn Deliver>) -> Result<Capture, Error> {
        com();
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err(Error::Unsupported("window capture needs Windows 10 1903 or later".into()));
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| fail("opening Windows Graphics Capture", e))?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(window) }.map_err(|e| {
            if super::screen::top_windows().iter().any(|l| l.hwnd == window) {
                fail("capturing the window", e)
            } else {
                Error::NotFound(format!("window {:?}", window.0))
            }
        })?;
        Capture::start(item, true, config, deliver)
    }

    fn start(
        item: GraphicsCaptureItem,
        window: bool,
        config: &Config,
        deliver: Arc<dyn Deliver>,
    ) -> Result<Capture, Error> {
        let e = |what: &'static str| move |e: windows::core::Error| fail(what, e);
        let gpu = Gpu::engine()?;
        let dxgi: IDXGIDevice = gpu.device.cast().map_err(e("opening the Direct3D device"))?;
        let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .map_err(e("opening the Direct3D device"))?;
        let pixel = if config.format == PixelFormat::RgbaF16 {
            DirectXPixelFormat::R16G16B16A16Float
        } else {
            DirectXPixelFormat::B8G8R8A8UIntNormalized
        };
        let size = item.Size().map_err(e("reading the capture's size"))?;
        let size = SizeInt32 { Width: size.Width.max(2), Height: size.Height.max(2) };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, pixel, POOL_BUFFERS, size)
            .map_err(e("making the capture's frame pool"))?;
        let session = pool.CreateCaptureSession(&item).map_err(e("starting the capture"))?;

        // what this Windows lets a capture leave out: the pointer (Windows 10 2004), the yellow
        // border (Windows 11), frames faster than asked for (Windows 11 24H2); a window's own
        // pop-ups and menus go in (Windows 11)
        let _ = session.SetIsCursorCaptureEnabled(config.cursor);
        if super::contract(12) {
            ask_borderless();
            let _ = session.SetIsBorderRequired(false);
        }
        if config.fps > 0 {
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: interval_ns(config.fps) / 100 });
        }
        if window {
            let _ = session.SetIncludeSecondaryWindows(true);
        }

        let state = Arc::new(State {
            deliver: deliver.clone(),
            device,
            publisher: Mutex::new(Publisher::new(gpu, config.queue_depth)),
            config: Mutex::new(config.clone()),
            pool_size: Mutex::new(size),
            pixel,
            size: Mutex::new((size.Width as u32, size.Height as u32)),
            last_ns: AtomicI64::new(0),
        });
        let on_frame = state.clone();
        let arrived = pool
            .FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |pool, _| {
                if let Ok(pool) = pool.ok() {
                    on_frame.arrived(pool);
                }
                Ok(())
            }))
            .map_err(e("listening for frames"))?;
        let on_close = deliver.clone();
        let what = if window { "the window closed" } else { "the display went away" };
        let closed = item
            .Closed(&TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
                on_close.stopped(what);
                Ok(())
            }))
            .map_err(e("listening for the capture's end"))?;
        session.StartCapture().map_err(e("starting the capture"))?;
        Ok(Capture { item, pool, session, arrived, closed, state })
    }

    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        let _ = self.session.SetIsCursorCaptureEnabled(config.cursor);
        if config.fps > 0 {
            let _ = self.session.SetMinUpdateInterval(TimeSpan { Duration: interval_ns(config.fps) / 100 });
        }
        self.state.publisher.lock().set_depth(config.queue_depth);
        *self.state.config.lock() = config.clone();
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        *self.state.size.lock()
    }
}

impl State {
    /// A frame is waiting in the pool: copy it out, give it back, deliver the copy.
    fn arrived(&self, pool: &Direct3D11CaptureFramePool) {
        let Ok(frame) = pool.TryGetNextFrame() else { return };
        let config = self.config.lock().clone();
        let host_ns = frame.SystemRelativeTime().map(|t| hns_ns(t.Duration)).unwrap_or_else(|_| host_now_ns());
        let content = frame.ContentSize().unwrap_or(SizeInt32 { Width: 0, Height: 0 });

        // the window changed size: the pool's textures follow it (from the next frame)
        {
            let mut pool_size = self.pool_size.lock();
            if content.Width > 0
                && content.Height > 0
                && (content.Width != pool_size.Width || content.Height != pool_size.Height)
                && pool.Recreate(&self.device, self.pixel, POOL_BUFFERS, content).is_ok()
            {
                *pool_size = content;
            }
        }

        // at most the frame rate asked for (a little early is fine: frames don't come evenly)
        let interval = interval_ns(config.fps);
        let last = self.last_ns.load(Ordering::Relaxed);
        if interval > 0 && last != 0 && host_ns - last < interval * 3 / 4 {
            let _ = frame.Close();
            return;
        }

        let texture = frame
            .Surface()
            .and_then(|s| s.cast::<IDirect3DDxgiInterfaceAccess>())
            .and_then(|access| unsafe { access.GetInterface::<ID3D11Texture2D>() });
        let Ok(texture) = texture else {
            let _ = frame.Close();
            return;
        };
        let region = (content.Width > 0 && content.Height > 0).then_some(RECT {
            left: 0,
            top: 0,
            right: content.Width,
            bottom: content.Height,
        });
        let out = Out { size: config.size, format: config.format };
        let published = self.publisher.lock().publish(&texture, 0, region, &out);
        drop(texture);
        let _ = frame.Close();
        let Some(p) = published else { return };
        self.last_ns.store(host_ns, Ordering::Relaxed);
        *self.size.lock() = p.size;
        self.deliver.frame(Frame {
            image: p.image,
            host_ns,
            size: p.size,
            format: p.format,
            content: Some(p.content),
            content_scale: Some(1.0),
            scale: Some(1.0),
        });
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.arrived);
        let _ = self.item.RemoveClosed(self.closed);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

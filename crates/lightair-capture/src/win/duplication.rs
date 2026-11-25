//! Desktop Duplication: a display as it's scanned out, read on a thread of the capture's own.
//! Made again when Windows takes it away (`DXGI_ERROR_ACCESS_LOST`: a mode change, the secure
//! desktop, a fullscreen game taking or leaving the display).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::core::Interface;

use super::gpu::{Gpu, Out, Publisher};
use super::screen::{Monitor, monitors};
use super::{Deliver, com, fail, interval_ns, qpc_ns};
use crate::screen::Config;
use crate::{Error, Frame, PixelFormat};

struct Shared {
    config: Mutex<Config>,
    /// The part of the desktop to keep (desktop pixels), for a game's window; the whole display
    /// when `None`.
    crop: Mutex<Option<RECT>>,
    size: Mutex<(u32, u32)>,
    stop: AtomicBool,
}

/// A running Desktop Duplication of one display.
pub struct Capture {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// A duplication of one output, on a device of the adapter it's connected to.
struct Duplicated {
    duplication: IDXGIOutputDuplication,
    gpu: Arc<Gpu>,
    /// Where the output is on the desktop.
    desktop: RECT,
}

// DXGI objects are free-threaded.
unsafe impl Send for Duplicated {}

/// Finds the output showing `monitor` and duplicates it, in FP16 when HDR is asked for (and
/// the system has `DuplicateOutput1`), BGRA otherwise.
fn duplicate(monitor: HMONITOR, hdr: bool) -> Result<Duplicated, Error> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| fail("opening DXGI", e))?;
    let mut a = 0;
    while let Ok(found) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        while let Ok(output) = unsafe { found.EnumOutputs(o) } {
            o += 1;
            let Ok(desc): Result<DXGI_OUTPUT_DESC, _> = (unsafe { output.GetDesc() }) else { continue };
            if desc.Monitor != monitor {
                continue;
            }
            let luid = unsafe { found.GetDesc1() }.map_err(|e| fail("reading the graphics adapter", e))?.AdapterLuid;
            let gpu = Gpu::on(Some(luid))?;
            let duplication = match output.cast::<IDXGIOutput5>() {
                Ok(output5) => {
                    let formats: &[_] = if hdr {
                        &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM]
                    } else {
                        &[DXGI_FORMAT_B8G8R8A8_UNORM]
                    };
                    unsafe { output5.DuplicateOutput1(&gpu.device, 0, formats) }
                }
                Err(_) => output.cast::<IDXGIOutput1>().and_then(|o| unsafe { o.DuplicateOutput(&gpu.device) }),
            };
            let duplication = duplication.map_err(|e| {
                if e.code() == DXGI_ERROR_UNSUPPORTED {
                    Error::Unsupported(
                        "this display can't be duplicated from this graphics adapter (a laptop's second GPU?)".into(),
                    )
                } else {
                    fail("duplicating the display", e)
                }
            })?;
            return Ok(Duplicated { duplication, gpu, desktop: desc.DesktopCoordinates });
        }
    }
    Err(Error::NotFound("the display's output".into()))
}

impl Capture {
    /// Duplicates `monitor`, keeping only `crop` (desktop pixels) when given.
    pub fn start(
        monitor: &Monitor,
        crop: Option<RECT>,
        config: &Config,
        deliver: Arc<dyn Deliver>,
    ) -> Result<Capture, Error> {
        let shared = Arc::new(Shared {
            config: Mutex::new(config.clone()),
            crop: Mutex::new(crop),
            size: Mutex::new(monitor.display.pixel_size),
            stop: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let handle = monitor.handle.0 as isize;
        let uuid = monitor.display.uuid.clone();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("lightair-duplication".into())
            .spawn(move || run(HMONITOR(handle as *mut _), uuid, thread_shared, deliver, tx))
            .map_err(|e| Error::Failed(format!("starting the duplication thread: {e}")))?;
        let started = rx
            .recv_timeout(Duration::from_secs(8))
            .unwrap_or_else(|_| Err(Error::Failed("duplicating the display: no answer".into())));
        match started {
            Ok(()) => Ok(Capture { shared, thread: Some(thread) }),
            Err(e) => {
                shared.stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(e)
            }
        }
    }

    /// Keeps only this part of the desktop (pixels) from now on, or the whole display.
    pub fn set_crop(&self, crop: Option<RECT>) {
        *self.shared.crop.lock() = crop;
    }

    pub fn reconfigure(&self, config: &Config) -> Result<(), Error> {
        *self.shared.config.lock() = config.clone();
        Ok(())
    }

    pub fn size(&self) -> (u32, u32) {
        *self.shared.size.lock()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How long a duplication may stay lost (the display gone, the secure desktop up) before the
/// capture says it stopped.
const LOST_FOR: Duration = Duration::from_secs(10);

fn run(
    mut monitor: HMONITOR,
    uuid: String,
    shared: Arc<Shared>,
    deliver: Arc<dyn Deliver>,
    started: mpsc::Sender<Result<(), Error>>,
) {
    com();
    let hdr = shared.config.lock().format == PixelFormat::RgbaF16;
    let mut current = match duplicate(monitor, hdr) {
        Ok(d) => {
            let _ = started.send(Ok(()));
            Some(d)
        }
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    let mut publisher: Option<Publisher> = None;
    let mut lost_since: Option<Instant> = None;
    let mut last_ns = 0i64;
    while !shared.stop.load(Ordering::Relaxed) {
        let Some(dup) = current.as_ref() else {
            // lost: make it again (the monitor's handle may have changed with the mode)
            std::thread::sleep(Duration::from_millis(100));
            if let Some(m) = monitors().into_iter().find(|m| m.display.uuid == uuid) {
                monitor = m.handle;
                *shared.size.lock() = m.display.pixel_size;
            }
            match duplicate(monitor, hdr) {
                Ok(d) => {
                    current = Some(d);
                    lost_since = None;
                }
                Err(e) => {
                    let since = *lost_since.get_or_insert_with(Instant::now);
                    if since.elapsed() > LOST_FOR {
                        deliver.stopped(&format!("the display can't be duplicated now: {e}"));
                        return;
                    }
                }
            }
            continue;
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.duplication.AcquireNextFrame(100, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                current = None;
                publisher = None;
                continue;
            }
            Err(e) => {
                tracing::warn!("capture: duplication: {}", e.message());
                current = None;
                publisher = None;
                continue;
            }
        }
        let config = shared.config.lock().clone();
        // only the pointer moved: nothing new to show (Lightair draws its own pointer)
        let fresh = info.LastPresentTime != 0;
        let host_ns = qpc_ns(info.LastPresentTime);
        let interval = interval_ns(config.fps);
        let wanted = fresh && (interval == 0 || last_ns == 0 || host_ns - last_ns >= interval * 3 / 4);
        let mut published = None;
        if wanted && let Some(texture) = resource.as_ref().and_then(|r| r.cast::<ID3D11Texture2D>().ok()) {
            let publisher = publisher.get_or_insert_with(|| Publisher::new(dup.gpu.clone(), config.queue_depth));
            publisher.set_depth(config.queue_depth);
            let region = shared.crop.lock().map(|c| RECT {
                left: c.left - dup.desktop.left,
                top: c.top - dup.desktop.top,
                right: c.right - dup.desktop.left,
                bottom: c.bottom - dup.desktop.top,
            });
            published = publisher.publish(&texture, 0, region, &Out { size: config.size, format: config.format });
        }
        drop(resource);
        let _ = unsafe { dup.duplication.ReleaseFrame() };
        if let Some(p) = published {
            last_ns = host_ns;
            *shared.size.lock() = p.size;
            deliver.frame(Frame {
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
}

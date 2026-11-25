//! Direct3D 11: the devices captures copy on, and the shared textures frames are handed over in.
//!
//! What Windows Graphics Capture, Desktop Duplication and Media Foundation hand out are their own
//! textures, recycled as soon as they're released and not shareable with another device. So each
//! picture is copied on the GPU (or converted, when another size or NV12 is asked for, by the
//! Direct3D 11 video processor) into a texture of the capture's own that has an NT shared handle,
//! which the engine opens on its Direct3D 12 device (`ID3D12Device::OpenSharedHandle`). A shared
//! fence says when the copy is done. No pixel goes through the CPU.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::{Mutex, MutexGuard};
use windows::Win32::Foundation::{GENERIC_ALL, HANDLE, HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_COLOR,
    D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice, ID3D11Device, ID3D11Device5,
    ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Query, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter, IDXGIAdapter1,
    IDXGIFactory1, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use super::{Owned, fail};
use crate::{Error, PixelFormat, Rect};

// ------------------------------------------------------------------ what frames carry

/// A Windows frame's picture.
pub enum Image {
    /// A Direct3D 11 texture of the capture's own, with an NT shared handle.
    Texture(Texture),
    /// The bytes, for a camera that couldn't hand over a texture.
    Memory(Memory),
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Image::Texture(t) => f.debug_tuple("Texture").field(t).finish(),
            Image::Memory(m) => {
                f.debug_struct("Memory").field("len", &m.data.len()).field("stride", &m.stride).finish()
            }
        }
    }
}

/// A frame's pixels in memory: BGRA rows, or NV12's luma rows followed by its interleaved
/// chroma rows (half as many), each `stride` bytes apart.
#[derive(Clone)]
pub struct Memory {
    pub data: Vec<u8>,
    pub stride: usize,
}

/// A shared fence and the value it reaches once the frame's texture is written: the engine's
/// queue waits for it (`ID3D12CommandQueue::Wait` on the fence opened from `handle`) before it
/// reads the texture.
#[derive(Debug, Clone, Copy)]
pub struct SharedFence {
    pub handle: HANDLE,
    pub value: u64,
}

/// A captured picture in a Direct3D 11 texture the capture owns. The texture is the capture's
/// again (to write a later picture into) once this is dropped, so keep it until the GPU work
/// that reads it has finished.
pub struct Texture {
    slot: Arc<Slot>,
    gpu: Arc<Gpu>,
    value: Option<u64>,
}

impl Texture {
    /// The texture, on the capture's device ([`Texture::device`]).
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.slot.texture
    }

    /// Its NT shared handle, which stays the capture's: open it (`OpenSharedHandle`), don't
    /// close it. Every frame written into the same texture has the same handle, so an engine
    /// can keep what it opened by handle.
    pub fn shared_handle(&self) -> HANDLE {
        self.slot.handle.0
    }

    /// Its pixel format: `B8G8R8A8_UNORM`, `NV12` or `R16G16B16A16_FLOAT`.
    pub fn dxgi_format(&self) -> DXGI_FORMAT {
        self.slot.format
    }

    /// The device it was written on.
    pub fn device(&self) -> &ID3D11Device {
        &self.gpu.device
    }

    /// The graphics adapter it's on: a device on another adapter can't open it.
    pub fn adapter(&self) -> LUID {
        self.gpu.luid
    }

    /// What to wait for before reading it; `None` where the system has no shared fences
    /// (before Windows 10 1703), in which case the copy had finished before the frame was
    /// handed over.
    pub fn fence(&self) -> Option<SharedFence> {
        let fence = self.gpu.fence.as_ref()?;
        Some(SharedFence { handle: fence.handle.0, value: self.value? })
    }
}

impl std::fmt::Debug for Texture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Texture")
            .field("size", &self.slot.size)
            .field("format", &self.slot.format)
            .field("handle", &self.slot.handle.0)
            .field("value", &self.value)
            .finish()
    }
}

// ------------------------------------------------------------------ devices

/// The adapter captures put their textures on, by LUID; `None` for the system's first.
static ADAPTER: Mutex<Option<(u32, i32)>> = Mutex::new(None);

/// Puts later captures' textures on this graphics adapter (the engine's: a shared texture
/// opens only on a device of the same adapter). Captures already running keep theirs. Desktop
/// Duplication always uses the adapter the display is connected to.
pub fn set_adapter(luid: Option<LUID>) {
    *ADAPTER.lock() = luid.map(|l| (l.LowPart, l.HighPart));
}

/// The adapter set with [`set_adapter`], if any.
pub(crate) fn chosen_adapter() -> Option<LUID> {
    ADAPTER.lock().map(|(low, high)| LUID { LowPart: low, HighPart: high })
}

struct Fence {
    fence: ID3D11Fence,
    context: ID3D11DeviceContext4,
    handle: Owned,
    value: AtomicU64,
}

/// A Direct3D 11 device that captures on one adapter share, with the shared fence their copies
/// signal. Its immediate context is used under [`Gpu::lock`] only.
pub(crate) struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub luid: LUID,
    /// The device was made with video support (the video processor, Media Foundation's
    /// hardware decoders).
    pub video: bool,
    fence: Option<Fence>,
    lock: Mutex<()>,
}

// Direct3D 11 devices are free-threaded; the immediate context is only used under `lock`, and
// the device is multithread-protected for what other APIs (Media Foundation) do with it.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

fn same(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

static GPUS: Mutex<Vec<Weak<Gpu>>> = Mutex::new(Vec::new());

impl Gpu {
    /// The device on the engine's adapter ([`set_adapter`]), shared by every capture there.
    pub fn engine() -> Result<Arc<Gpu>, Error> {
        Gpu::on(chosen_adapter())
    }

    /// The device on adapter `luid` (the system's first when `None`).
    pub fn on(luid: Option<LUID>) -> Result<Arc<Gpu>, Error> {
        let adapter = adapter(luid)?;
        let luid = unsafe { adapter.GetDesc1() }.map_err(|e| fail("reading the graphics adapter", e))?.AdapterLuid;
        let mut gpus = GPUS.lock();
        gpus.retain(|g| g.strong_count() > 0);
        if let Some(gpu) = gpus.iter().filter_map(Weak::upgrade).find(|g| same(g.luid, luid)) {
            return Ok(gpu);
        }
        let gpu = Arc::new(Gpu::create(&adapter, luid)?);
        gpus.push(Arc::downgrade(&gpu));
        Ok(gpu)
    }

    fn create(adapter: &IDXGIAdapter1, luid: LUID) -> Result<Gpu, Error> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let make = |flags: D3D11_CREATE_DEVICE_FLAG| {
            let mut device = None;
            let mut context = None;
            unsafe {
                D3D11CreateDevice(
                    &adapter.cast::<IDXGIAdapter>()?,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .map(|()| (device, context))
        };
        let (made, video) = match make(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
            Ok(made) => (made, true),
            Err(_) => {
                (make(D3D11_CREATE_DEVICE_BGRA_SUPPORT).map_err(|e| fail("making a Direct3D 11 device", e))?, false)
            }
        };
        let (Some(device), Some(context)) = made else {
            return Err(Error::Failed("making a Direct3D 11 device: nothing made".into()));
        };
        if let Ok(threads) = device.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = threads.SetMultithreadProtected(true);
            }
        }
        let fence = shared_fence(&device, &context);
        Ok(Gpu { device, context, luid, video, fence, lock: Mutex::new(()) })
    }

    /// The immediate context's lock: hold it for every use of `context`.
    pub fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock()
    }

    /// After work on the immediate context (under the lock): signals the shared fence with a
    /// new value and returns it, or, without a fence, waits for the GPU to finish the work.
    fn finish(&self) -> Option<u64> {
        if let Some(fence) = &self.fence {
            let value = fence.value.fetch_add(1, Ordering::Relaxed) + 1;
            if unsafe { fence.context.Signal(&fence.fence, value) }.is_ok() {
                unsafe { self.context.Flush() };
                return Some(value);
            }
        }
        self.wait_idle();
        None
    }

    /// Waits until the GPU has done everything submitted on the immediate context.
    fn wait_idle(&self) {
        let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
        let mut query: Option<ID3D11Query> = None;
        if unsafe { self.device.CreateQuery(&desc, Some(&mut query)) }.is_err() {
            unsafe { self.context.Flush() };
            return;
        }
        let Some(query) = query else { return };
        unsafe { self.context.End(&query) };
        let start = std::time::Instant::now();
        loop {
            let mut done = windows::core::BOOL(0);
            let read = unsafe {
                self.context.GetData(
                    &query,
                    Some(&mut done as *mut _ as *mut _),
                    size_of::<windows::core::BOOL>() as u32,
                    0,
                )
            };
            if read.is_err() || done.as_bool() || start.elapsed() > std::time::Duration::from_millis(500) {
                break;
            }
            std::thread::yield_now();
        }
    }
}

/// The DXGI adapter with this LUID (the first when `None`).
pub(crate) fn adapter(luid: Option<LUID>) -> Result<IDXGIAdapter1, Error> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| fail("opening DXGI", e))?;
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        i += 1;
        let Some(wanted) = luid else { return Ok(adapter) };
        if let Ok(desc) = unsafe { adapter.GetDesc1() }
            && same(desc.AdapterLuid, wanted)
        {
            return Ok(adapter);
        }
    }
    Err(Error::NotFound("the graphics adapter".into()))
}

fn shared_fence(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Option<Fence> {
    let device5: ID3D11Device5 = device.cast().ok()?;
    let context: ID3D11DeviceContext4 = context.cast().ok()?;
    let mut fence: Option<ID3D11Fence> = None;
    unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }.ok()?;
    let fence = fence?;
    let handle = unsafe { fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()) }.ok()?;
    Some(Fence { fence, context, handle: Owned(handle), value: AtomicU64::new(0) })
}

// ------------------------------------------------------------------ shared textures

/// One of a capture's shared textures. In use while a [`Texture`] holds it.
struct Slot {
    texture: ID3D11Texture2D,
    handle: Owned,
    size: (u32, u32),
    format: DXGI_FORMAT,
}

unsafe impl Send for Slot {}
unsafe impl Sync for Slot {}

fn make_slot(gpu: &Gpu, size: (u32, u32), format: DXGI_FORMAT) -> Result<Slot, Error> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let mut texture = None;
    unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| fail("making a shared texture", e))?;
    let texture: ID3D11Texture2D = texture.ok_or_else(|| Error::Failed("making a shared texture".into()))?;
    let resource: IDXGIResource1 = texture.cast().map_err(|e| fail("sharing a texture", e))?;
    let handle = unsafe {
        resource.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, PCWSTR::null())
    }
    .map_err(|e| fail("sharing a texture", e))?;
    Ok(Slot { texture, handle: Owned(handle), size, format })
}

/// The DXGI format a pixel format is drawn in.
pub(crate) fn dxgi_of(format: PixelFormat) -> DXGI_FORMAT {
    match format {
        PixelFormat::Bgra => DXGI_FORMAT_B8G8R8A8_UNORM,
        PixelFormat::Nv12Video | PixelFormat::Nv12Full => DXGI_FORMAT_NV12,
        PixelFormat::RgbaF16 => DXGI_FORMAT_R16G16B16A16_FLOAT,
    }
}

/// The pixel format of a DXGI format, where it's one the engine draws as it is (NV12 taken as
/// video range).
pub(crate) fn pixel_of(format: DXGI_FORMAT) -> Option<PixelFormat> {
    match format {
        DXGI_FORMAT_B8G8R8A8_UNORM | DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => Some(PixelFormat::Bgra),
        DXGI_FORMAT_NV12 => Some(PixelFormat::Nv12Video),
        DXGI_FORMAT_R16G16B16A16_FLOAT => Some(PixelFormat::RgbaF16),
        _ => None,
    }
}

/// What a capture wants its pictures as.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Out {
    /// The picture's size; `None` for the source's own.
    pub size: Option<(u32, u32)>,
    pub format: PixelFormat,
}

/// A picture made by [`Publisher::publish`].
pub(crate) struct Published {
    pub image: Image,
    pub size: (u32, u32),
    pub format: PixelFormat,
    /// Where the source landed in the picture (pixels): all of it, or less when it was scaled
    /// to fit with its shape kept.
    pub content: Rect,
}

/// Copies (or converts) a capture's pictures into its shared textures, at most `depth` of
/// them, each written again only once no frame holds it.
pub(crate) struct Publisher {
    pub gpu: Arc<Gpu>,
    slots: Vec<Arc<Slot>>,
    depth: usize,
    converter: Option<Converter>,
    /// Frames dropped because every texture was still held.
    pub dropped: u64,
}

impl Publisher {
    pub fn new(gpu: Arc<Gpu>, depth: u32) -> Publisher {
        Publisher { gpu, slots: Vec::new(), depth: depth.clamp(2, 16) as usize, converter: None, dropped: 0 }
    }

    pub fn set_depth(&mut self, depth: u32) {
        self.depth = depth.clamp(2, 16) as usize;
    }

    /// A free shared texture of this size and format: one already made, a new one while there
    /// are fewer than `depth`, or a free one of another size made again.
    fn slot(&mut self, size: (u32, u32), format: DXGI_FORMAT) -> Option<Arc<Slot>> {
        // only this publisher clones slots, so a count of 1 can't go up behind its back
        let free = |s: &Arc<Slot>| Arc::strong_count(s) == 1;
        // the free texture written longest ago first, so the engine's reads have long finished
        if let Some(i) = self.slots.iter().position(|s| free(s) && s.size == size && s.format == format) {
            let slot = self.slots.remove(i);
            self.slots.push(slot.clone());
            return Some(slot);
        }
        if self.slots.len() >= self.depth {
            let i = self.slots.iter().position(free)?;
            self.slots.remove(i);
        }
        match make_slot(&self.gpu, size, format) {
            Ok(slot) => {
                let slot = Arc::new(slot);
                self.slots.push(slot.clone());
                Some(slot)
            }
            Err(e) => {
                tracing::warn!("capture: {e}");
                None
            }
        }
    }

    /// Copies `region` of `source`'s `subresource` (all of it when `None`) into a free shared
    /// texture, scaled and converted to `out` on the GPU when it asks for another size or
    /// format. `None` when every texture is held, or the copy failed.
    pub fn publish(
        &mut self,
        source: &ID3D11Texture2D,
        subresource: u32,
        region: Option<RECT>,
        out: &Out,
    ) -> Option<Published> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        let full = RECT { left: 0, top: 0, right: desc.Width as i32, bottom: desc.Height as i32 };
        let region = clip(region.unwrap_or(full), &full)?;
        let source_size = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
        let wanted = dxgi_of(out.format);
        let size = out.size.unwrap_or(source_size);
        // NV12's chroma is half each way, so its sides are even
        let size = if wanted == DXGI_FORMAT_NV12 { even(size) } else { (size.0.max(2), size.1.max(2)) };
        let same_format = desc.Format == wanted
            || (wanted == DXGI_FORMAT_B8G8R8A8_UNORM && desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB);
        let convertible =
            self.gpu.video && desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT && wanted != DXGI_FORMAT_R16G16B16A16_FLOAT;

        // the plan: a straight copy, a conversion, or (when the video processor can't) a copy in
        // the source's own format and size
        let (format, size, convert) = if same_format && size == source_size {
            (wanted, size, false)
        } else if convertible {
            (wanted, size, true)
        } else {
            let own = pixel_of(desc.Format)?;
            (dxgi_of(own), source_size, false)
        };
        let pixel = if format == DXGI_FORMAT_NV12 && out.format == PixelFormat::Nv12Full {
            PixelFormat::Nv12Full
        } else {
            pixel_of(format)?
        };
        let Some(slot) = self.slot(size, format) else {
            self.dropped += 1;
            return None;
        };
        let gpu = self.gpu.clone();
        let _lock = gpu.lock();
        let content = if convert {
            let converter = self.converter.get_or_insert_with(Converter::default);
            match converter.blt(&gpu, source, subresource, region, &slot, pixel) {
                Ok(content) => content,
                Err(e) => {
                    tracing::warn!("capture: converting a picture: {e}");
                    return None;
                }
            }
        } else {
            let area = D3D11_BOX {
                left: region.left as u32,
                top: region.top as u32,
                front: 0,
                right: region.left as u32 + size.0.min(source_size.0),
                bottom: region.top as u32 + size.1.min(source_size.1),
                back: 1,
            };
            unsafe { gpu.context.CopySubresourceRegion(&slot.texture, 0, 0, 0, 0, source, subresource, Some(&area)) };
            Rect { x: 0.0, y: 0.0, width: size.0 as f64, height: size.1 as f64 }
        };
        let value = gpu.finish();
        Some(Published {
            image: Image::Texture(Texture { slot, gpu: gpu.clone(), value }),
            size,
            format: pixel,
            content,
        })
    }
}

/// `r` inside `bounds`, or `None` when nothing's left.
fn clip(r: RECT, bounds: &RECT) -> Option<RECT> {
    let r = RECT {
        left: r.left.max(bounds.left),
        top: r.top.max(bounds.top),
        right: r.right.min(bounds.right),
        bottom: r.bottom.min(bounds.bottom),
    };
    (r.right - r.left >= 2 && r.bottom - r.top >= 2).then_some(r)
}

/// A size rounded down to even sides, at least 2×2.
fn even(size: (u32, u32)) -> (u32, u32) {
    ((size.0 & !1).max(2), (size.1 & !1).max(2))
}

/// `inner` scaled to fit `outer` with its shape kept, centred.
fn fit(inner: (u32, u32), outer: (u32, u32)) -> RECT {
    let scale = (outer.0 as f64 / inner.0.max(1) as f64).min(outer.1 as f64 / inner.1.max(1) as f64);
    let w = ((inner.0 as f64 * scale).round() as i32).clamp(1, outer.0 as i32);
    let h = ((inner.1 as f64 * scale).round() as i32).clamp(1, outer.1 as i32);
    let x = (outer.0 as i32 - w) / 2;
    let y = (outer.1 as i32 - h) / 2;
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

/// A video processor, with the input and output sizes it was made for.
type Processor = ((u32, u32), (u32, u32), ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor);

/// The Direct3D 11 video processor: scales and converts BGRA or NV12 to BGRA or NV12.
#[derive(Default)]
struct Converter {
    video: Option<(ID3D11VideoDevice, ID3D11VideoContext)>,
    processor: Option<Processor>,
    /// A copy of the source, for sources the video processor can't read directly.
    staging: Option<ID3D11Texture2D>,
}

/// Colour space bits (`D3D11_VIDEO_PROCESSOR_COLOR_SPACE`): BT.709 matrix, and the nominal range
/// (1: 16–235, 2: 0–255).
fn colour_space(full: bool) -> D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
    let matrix_709 = 1 << 2;
    let range = if full { 2 << 4 } else { 1 << 4 };
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: matrix_709 | range }
}

impl Converter {
    fn blt(
        &mut self,
        gpu: &Gpu,
        source: &ID3D11Texture2D,
        subresource: u32,
        region: RECT,
        slot: &Slot,
        pixel: PixelFormat,
    ) -> Result<Rect, Error> {
        let e = |what: &'static str| move |e: windows::core::Error| fail(what, e);
        if self.video.is_none() {
            let device: ID3D11VideoDevice = gpu.device.cast().map_err(e("opening the video processor"))?;
            let context: ID3D11VideoContext = gpu.context.cast().map_err(e("opening the video processor"))?;
            self.video = Some((device, context));
        }
        let Some((device, context)) = self.video.clone() else { unreachable!() };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        let input = (desc.Width, desc.Height);
        if self.processor.as_ref().is_none_or(|(i, o, ..)| *i != input || *o != slot.size) {
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: slot.size.0,
                OutputHeight: slot.size.1,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator =
                unsafe { device.CreateVideoProcessorEnumerator(&content) }.map_err(e("making a video processor"))?;
            let processor =
                unsafe { device.CreateVideoProcessor(&enumerator, 0) }.map_err(e("making a video processor"))?;
            self.processor = Some((input, slot.size, enumerator, processor));
        }
        let Some((_, _, enumerator, processor)) = self.processor.clone() else { unreachable!() };

        let input_view = |texture: &ID3D11Texture2D, slice: u32| {
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
                },
            };
            let mut view: Option<ID3D11VideoProcessorInputView> = None;
            unsafe { device.CreateVideoProcessorInputView(texture, &enumerator, &desc, Some(&mut view)) }.map(|()| view)
        };
        // a source the processor can't read (not a shader resource, say) is copied first
        let (view, region) = match input_view(source, subresource) {
            Ok(Some(view)) => (view, region),
            _ => {
                let size = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
                let fits = self.staging.as_ref().is_some_and(|s| {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    unsafe { s.GetDesc(&mut d) };
                    (d.Width, d.Height) == size && d.Format == desc.Format
                });
                if !fits {
                    let staging_desc = D3D11_TEXTURE2D_DESC {
                        Width: size.0,
                        Height: size.1,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: desc.Format,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                        CPUAccessFlags: 0,
                        MiscFlags: 0,
                    };
                    let mut texture = None;
                    unsafe { gpu.device.CreateTexture2D(&staging_desc, None, Some(&mut texture)) }
                        .map_err(e("making a copy for the video processor"))?;
                    self.staging = texture;
                }
                let Some(staging) = self.staging.clone() else { unreachable!() };
                let area = D3D11_BOX {
                    left: region.left as u32,
                    top: region.top as u32,
                    front: 0,
                    right: region.right as u32,
                    bottom: region.bottom as u32,
                    back: 1,
                };
                unsafe { gpu.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, source, subresource, Some(&area)) };
                // the processor was made for the source's size: make it again for the copy's
                self.processor = None;
                return self.blt(
                    gpu,
                    &staging,
                    0,
                    RECT { left: 0, top: 0, right: size.0 as i32, bottom: size.1 as i32 },
                    slot,
                    pixel,
                );
            }
        };

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe { device.CreateVideoProcessorOutputView(&slot.texture, &enumerator, &output_desc, Some(&mut output)) }
            .map_err(e("drawing into a shared texture"))?;
        let output = output.ok_or_else(|| Error::Failed("drawing into a shared texture".into()))?;

        let size = ((region.right - region.left) as u32, (region.bottom - region.top) as u32);
        let target = fit(size, slot.size);
        let whole = RECT { left: 0, top: 0, right: slot.size.0 as i32, bottom: slot.size.1 as i32 };
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } },
        };
        let source_space = colour_space(desc.Format != DXGI_FORMAT_NV12);
        let output_space = colour_space(pixel != PixelFormat::Nv12Video);
        unsafe {
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace(&processor, 0, &source_space);
            context.VideoProcessorSetOutputColorSpace(&processor, &output_space);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&region));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&target));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&whole));
            context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(view)),
            ..Default::default()
        };
        let streams = [stream];
        let done = unsafe { context.VideoProcessorBlt(&processor, &output, 0, &streams) };
        let [mut stream] = streams;
        unsafe { std::mem::ManuallyDrop::drop(&mut stream.pInputSurface) };
        done.map_err(e("scaling a picture"))?;
        Ok(Rect {
            x: target.left as f64,
            y: target.top as f64,
            width: (target.right - target.left) as f64,
            height: (target.bottom - target.top) as f64,
        })
    }
}

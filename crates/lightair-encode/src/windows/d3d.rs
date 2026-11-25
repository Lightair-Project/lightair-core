//! Direct3D 11 for the video encoder: its device, the shared textures the compositor draws
//! into, the shared fence that says a picture is finished, and the GPU conversion from BGRA to
//! NV12 for encoders that only take NV12.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Texture2D,
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_NONE_P709_X601, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020, DXGI_FORMAT,
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIDevice, IDXGIResource1,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, PCWSTR};

use super::mf::{Vendor, platform};
use crate::Error;
use crate::encoder::{InputFormat, VideoSettings};
use crate::format::Color;

/// A hardware Direct3D 11 device that can run video work, safe to use from several threads
/// (Media Foundation's encoders need both).
pub(crate) fn create_device() -> Result<ID3D11Device, Error> {
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(platform("Direct3D 11 wouldn't make a device for the encoder"))?;
    device.ok_or(Error::Unsupported("no Direct3D 11 device".into()))
}

/// Lets the device be used from several threads at once (the encoder's and the caller's).
pub(crate) fn protect(device: &ID3D11Device) -> Result<(), Error> {
    let multithread: ID3D11Multithread =
        device.cast().map_err(platform("the Direct3D 11 device can't be shared between threads"))?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok(())
}

/// The maker of the GPU behind `device`.
pub(crate) fn vendor(device: &ID3D11Device) -> Option<Vendor> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(Vendor::from_id(desc.VendorId))
}

pub(crate) fn dxgi_format(input: InputFormat) -> DXGI_FORMAT {
    match input {
        InputFormat::Bgra => DXGI_FORMAT_B8G8R8A8_UNORM,
        InputFormat::Nv12 => DXGI_FORMAT_NV12,
        InputFormat::P010 => DXGI_FORMAT_P010,
    }
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    shared: bool,
) -> Result<ID3D11Texture2D, Error> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: if shared {
            (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32
        } else {
            0
        },
    };
    let mut out = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut out)) }
        .map_err(platform("Direct3D 11 wouldn't make the encoder's input texture"))?;
    out.ok_or(Error::Unsupported("no texture".into()))
}

/// One picture the compositor draws into: a texture on the encoder's device, shared with the
/// compositor's device through an NT handle.
pub(crate) struct Slot {
    pub(crate) texture: ID3D11Texture2D,
    pub(crate) handle: HANDLE,
    /// Drawn into or inside the encoder: not to be handed out.
    pub(crate) busy: AtomicBool,
    /// The video processor's view of it, made on first conversion.
    pub(crate) input_view: parking_lot::Mutex<Option<ID3D11VideoProcessorInputView>>,
}

/// The encoder's input textures.
pub(crate) struct Pool {
    pub(crate) slots: Vec<Slot>,
}

impl Pool {
    pub(crate) fn new(device: &ID3D11Device, settings: &VideoSettings, size: u32) -> Result<Pool, Error> {
        let format = dxgi_format(settings.input);
        let mut slots = Vec::with_capacity(size as usize);
        for _ in 0..size.max(2) {
            let texture = texture(device, settings.width, settings.height, format, true)?;
            let resource: IDXGIResource1 =
                texture.cast().map_err(platform("the encoder's input texture can't be shared"))?;
            let handle = unsafe {
                resource.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR::null(),
                )
            }
            .map_err(platform("the encoder's input texture can't be shared"))?;
            slots.push(Slot {
                texture,
                handle,
                busy: AtomicBool::new(false),
                input_view: parking_lot::Mutex::new(None),
            });
        }
        Ok(Pool { slots })
    }

    /// A free slot, now marked busy.
    pub(crate) fn acquire(&self) -> Option<u32> {
        self.slots
            .iter()
            .position(|s| s.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
            .map(|i| i as u32)
    }

    pub(crate) fn release(&self, index: u32) {
        if let Some(slot) = self.slots.get(index as usize) {
            slot.busy.store(false, Ordering::Release);
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        for slot in &self.slots {
            let _ = unsafe { CloseHandle(slot.handle) };
        }
    }
}

/// The fence the compositor signals when a picture in a slot is finished, shared with it
/// through an NT handle.
pub(crate) struct Fence {
    fence: ID3D11Fence,
    pub(crate) handle: HANDLE,
    event: HANDLE,
}

impl Fence {
    pub(crate) fn new(device: &ID3D11Device) -> Result<Fence, Error> {
        let what = "Direct3D 11 wouldn't make the encoder's shared fence";
        let device5: ID3D11Device5 = device.cast().map_err(platform(what))?;
        let mut fence: Option<ID3D11Fence> = None;
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }.map_err(platform(what))?;
        let fence = fence.ok_or(Error::Unsupported("no fence".into()))?;
        let handle =
            unsafe { fence.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()) }.map_err(platform(what))?;
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.map_err(platform(what))?;
        Ok(Fence { fence, handle, event })
    }

    /// Queues a signal of `value` on `context` (the fence's device's) and sends the queued work
    /// to the GPU.
    pub(crate) fn signal(&self, context: &ID3D11DeviceContext4, value: u64) -> Result<(), Error> {
        unsafe {
            context.Signal(&self.fence, value).map_err(platform("Direct3D 11 wouldn't signal the encoder's fence"))?;
            context.Flush();
        }
        Ok(())
    }

    /// Waits (on this thread) until the fence reaches `value`; `false` after `timeout_ms`.
    pub(crate) fn wait(&self, value: u64, timeout_ms: u32) -> bool {
        unsafe {
            if value == 0 || self.fence.GetCompletedValue() >= value {
                return true;
            }
            if self.fence.SetEventOnCompletion(value, self.event).is_err() {
                return false;
            }
            WaitForSingleObject(self.event, timeout_ms) == WAIT_OBJECT_0
        }
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}

/// BGRA to NV12 on the GPU's video processor, into a ring of the converter's own textures.
pub(crate) struct Converter {
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// NV12 textures, their output views, and whether one is inside the encoder.
    targets: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView, AtomicBool)>,
}

impl Converter {
    pub(crate) fn new(device: &ID3D11Device, settings: &VideoSettings, size: u32) -> Result<Converter, Error> {
        let what = "the GPU's video processor can't convert BGRA to NV12";
        let video: ID3D11VideoDevice = device.cast().map_err(platform(what))?;
        let immediate = unsafe { device.GetImmediateContext() }.map_err(platform(what))?;
        let context: ID3D11VideoContext1 = immediate.cast().map_err(platform(what))?;
        let rate = DXGI_RATIONAL { Numerator: settings.fps_num.max(1), Denominator: settings.fps_den.max(1) };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: settings.width,
            InputHeight: settings.height,
            OutputFrameRate: rate,
            OutputWidth: settings.width,
            OutputHeight: settings.height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content) }.map_err(platform(what))?;
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0) }.map_err(platform(what))?;
        let output_space = output_color_space(&settings.color);
        unsafe {
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            context.VideoProcessorSetOutputColorSpace1(&processor, output_space);
        }
        let mut targets = Vec::with_capacity(size as usize);
        for _ in 0..size.max(2) {
            let texture = texture(device, settings.width, settings.height, DXGI_FORMAT_NV12, false)?;
            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut view = None;
            unsafe { video.CreateVideoProcessorOutputView(&texture, &enumerator, &desc, Some(&mut view)) }
                .map_err(platform(what))?;
            let view = view.ok_or(Error::Unsupported(what.into()))?;
            targets.push((texture, view, AtomicBool::new(false)));
        }
        Ok(Converter { video, context, enumerator, processor, targets })
    }

    /// A view the video processor reads `texture` through.
    pub(crate) fn input_view(&self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView, Error> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut view = None;
        unsafe { self.video.CreateVideoProcessorInputView(texture, &self.enumerator, &desc, Some(&mut view)) }
            .map_err(platform("the video processor can't read the encoder's input texture"))?;
        view.ok_or(Error::Unsupported("no input view".into()))
    }

    /// Converts the picture behind `input` into a free NV12 texture; returns its index (to
    /// [`Converter::release`] once the encoder has the picture) and the texture.
    pub(crate) fn convert(&self, input: &ID3D11VideoProcessorInputView) -> Result<(u32, ID3D11Texture2D), Error> {
        let Some(index) = self
            .targets
            .iter()
            .position(|t| t.2.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok())
        else {
            return Err(Error::Unsupported("every conversion texture is inside the encoder".into()));
        };
        let (texture, view, busy) = &self.targets[index];
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(input.clone())),
            ..Default::default()
        };
        let mut streams = [stream];
        let result = unsafe { self.context.VideoProcessorBlt(&self.processor, view, 0, &streams) };
        // the stream's reference to the input view is ours to drop
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        if let Err(e) = result {
            busy.store(false, Ordering::Release);
            return Err(platform("the video processor couldn't convert a picture")(e));
        }
        Ok((index as u32, texture.clone()))
    }

    pub(crate) fn release(&self, index: u32) {
        if let Some(t) = self.targets.get(index as usize) {
            t.2.store(false, Ordering::Release);
        }
    }
}

/// The YCbCr colour space the GPU's video processor writes for a stream's colour (BT.601's
/// matrix for matrix codes 5 and 6, BT.2020's for 9, else BT.709's).
pub(crate) fn output_color_space(color: &Color) -> windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE {
    match (color.matrix, color.full_range) {
        (9, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
        (5 | 6, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_NONE_P709_X601,
        (5 | 6, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
        (_, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
        _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    }
}

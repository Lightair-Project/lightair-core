//! Pictures copied on the GPU between textures of any size: a plain copy when the sizes and
//! formats match, else the GPU's video processor, which scales BGRA and converts it to NV12 in
//! one pass. The encoder copies a host's pictures into its input slots with it; a host uses it
//! for its own copies (a smaller picture to read back, NV12 for a camera).

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D, D3D11CreateDevice,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_RATIONAL,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::core::Interface;

use super::d3d::{output_color_space, protect};
use super::mf::platform;
use crate::Error;
use crate::format::Color;

/// A Direct3D 11 device for encoders and [`Blit`]s on the same graphics adapter as `like`
/// (`None`: the system's default adapter): with video support, safe to use from several
/// threads. One device serves any number of encoders; a texture shared from `like`'s adapter
/// opens on it by its NT handle.
pub fn video_device(like: Option<&ID3D11Device>) -> Result<ID3D11Device, Error> {
    let what = "Direct3D 11 wouldn't make a device for the encoders";
    let adapter: Option<IDXGIAdapter> = match like {
        Some(d) => {
            let dxgi: IDXGIDevice = d.cast().map_err(platform(what))?;
            Some(unsafe { dxgi.GetAdapter() }.map_err(platform(what))?)
        }
        None => None,
    };
    let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            adapter.as_ref(),
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(platform(what))?;
    let device = device.ok_or(Error::Unsupported("no Direct3D 11 device".into()))?;
    protect(&device)?;
    Ok(device)
}

/// The video processor for one input size, output size, output format and colour.
struct Processor {
    key: (u32, u32, u32, u32, DXGI_FORMAT, i32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

/// Copies pictures between textures on one device, scaling and converting on the GPU.
pub struct Blit {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    /// Made for the last sizes and output; made again when they change.
    processor: Option<Processor>,
}

// Direct3D 11 devices made multithread-protected are free-threaded; a `Blit` is used by one
// thread at a time (it takes `&mut self`).
unsafe impl Send for Blit {}

impl Blit {
    /// A blit on `device` (made with video support: [`video_device`]).
    pub fn new(device: &ID3D11Device) -> Result<Blit, Error> {
        let what = "the GPU's video processor isn't there";
        let video: ID3D11VideoDevice = device.cast().map_err(platform(what))?;
        let context = unsafe { device.GetImmediateContext() }.map_err(platform(what))?;
        let video_context: ID3D11VideoContext1 = context.cast().map_err(platform(what))?;
        Ok(Blit { device: device.clone(), context, video, video_context, processor: None })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Draws `src` (BGRA) into `dst` (BGRA or NV12), stretched to fill it; NV12 in `color`'s
    /// matrix and range. Both are on the blit's device, `dst` made to be drawn into (a render
    /// target) unless it matches `src` exactly. The work is queued on the GPU, not waited for.
    pub fn copy(&mut self, src: &ID3D11Texture2D, dst: &ID3D11Texture2D, color: &Color) -> Result<(), Error> {
        let (s, d) = (describe(src), describe(dst));
        if (s.Width, s.Height, s.Format) == (d.Width, d.Height, d.Format) {
            unsafe { self.context.CopyResource(dst, src) };
            return Ok(());
        }
        let space = if d.Format == DXGI_FORMAT_B8G8R8A8_UNORM {
            DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
        } else {
            output_color_space(color)
        };
        let key = (s.Width, s.Height, d.Width, d.Height, d.Format, space.0);
        if self.processor.as_ref().is_none_or(|p| p.key != key) {
            self.processor = None;
            self.processor = Some(self.processor_for(key)?);
        }
        let Some(p) = &self.processor else { return Err(Error::Unsupported("no video processor".into())) };
        let what = "the GPU's video processor couldn't copy a picture";
        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };
        let mut input = None;
        unsafe { self.video.CreateVideoProcessorInputView(src, &p.enumerator, &input_desc, Some(&mut input)) }
            .map_err(platform(what))?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output = None;
        unsafe { self.video.CreateVideoProcessorOutputView(dst, &p.enumerator, &output_desc, Some(&mut output)) }
            .map_err(platform(what))?;
        let output = output.ok_or(Error::Unsupported(what.into()))?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(input),
            ..Default::default()
        };
        let mut streams = [stream];
        let result = unsafe { self.video_context.VideoProcessorBlt(&p.processor, &output, 0, &streams) };
        // the stream's reference to the input view is ours to drop
        unsafe { std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        result.map_err(platform(what))
    }

    fn processor_for(&self, key: (u32, u32, u32, u32, DXGI_FORMAT, i32)) -> Result<Processor, Error> {
        let what = "the GPU's video processor can't copy this picture";
        let (in_w, in_h, out_w, out_h, _, space) = key;
        // the rate is only advice to the driver
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: in_w,
            InputHeight: in_h,
            OutputFrameRate: rate,
            OutputWidth: out_w,
            OutputHeight: out_h,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { self.video.CreateVideoProcessorEnumerator(&content) }.map_err(platform(what))?;
        let processor = unsafe { self.video.CreateVideoProcessor(&enumerator, 0) }.map_err(platform(what))?;
        let c = &self.video_context;
        unsafe {
            c.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            c.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            c.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            c.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_TYPE(space));
        }
        Ok(Processor { key, enumerator, processor })
    }
}

fn describe(texture: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    desc
}

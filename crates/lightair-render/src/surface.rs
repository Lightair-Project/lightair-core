//! macOS: the compositor draws the player's frames straight into an IOSurface (a Metal texture
//! wgpu renders to), and VideoToolbox turns each into the NV12 picture GPUI's `surface()`
//! element shows: no pixel goes to the CPU. (Elsewhere, and for the export, frames are read
//! back.)

use std::ptr::{self, NonNull};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetIOSurface, CVPixelBufferPool,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVReturnSuccess,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
use objc2_video_toolbox::VTPixelTransferSession;

/// A finished frame: NV12 (full range, BT.601), as GPUI's `surface()` draws it.
#[derive(Clone)]
pub struct Frame(pub CFRetained<CVPixelBuffer>);

// CoreVideo's buffers are reference counted thread-safely; a frame isn't written once made.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl std::ops::Deref for Frame {
    type Target = CVPixelBuffer;
    fn deref(&self) -> &CVPixelBuffer {
        &self.0
    }
}

/// Where the compositor draws: a BGRA IOSurface as a wgpu texture, and what makes frames of it.
pub struct Target {
    bgra: CFRetained<CVPixelBuffer>,
    pub texture: wgpu::Texture,
    pool: CFRetained<CVPixelBufferPool>,
    transfer: CFRetained<VTPixelTransferSession>,
}

// The pixel buffers and the session are only used by the renderer that owns them, one thread
// at a time.
unsafe impl Send for Target {}

fn bgra_buffer(w: u32, h: u32) -> Option<CFRetained<CVPixelBuffer>> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let keys: [&CFString; 2] = unsafe { [kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey] };
    let values: [&CFType; 2] = [empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    let rc = unsafe {
        CVPixelBufferCreate(
            None,
            w as usize,
            h as usize,
            kCVPixelFormatType_32BGRA,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    (rc == kCVReturnSuccess && !out.is_null()).then(|| unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

fn nv12_pool(w: u32, h: u32) -> Option<CFRetained<CVPixelBufferPool>> {
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let (format, width, height) = (
        CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange as i32),
        CFNumber::new_i32(w as i32),
        CFNumber::new_i32(h as i32),
    );
    let keys: [&CFString; 5] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let values: [&CFType; 5] =
        [format.as_ref(), width.as_ref(), height.as_ref(), empty.as_ref(), CFBoolean::new(true).as_ref()];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut out: *mut CVPixelBufferPool = ptr::null_mut();
    let rc = unsafe { CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut out)) };
    (rc == kCVReturnSuccess && !out.is_null()).then(|| unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) })
}

impl Target {
    /// A `size` target on `device` (Metal only; `None` otherwise or on failure).
    pub fn new(device: &wgpu::Device, size: (u32, u32)) -> Option<Target> {
        let (w, h) = size;
        let bgra = bgra_buffer(w, h)?;
        let iosurface = CVPixelBufferGetIOSurface(Some(&bgra))?;
        let raw: Retained<ProtocolObject<dyn MTLTexture>> = {
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
            let mtl = hal.raw_device();
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    w as usize,
                    h as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            mtl.newTextureWithDescriptor_iosurface_plane(&desc, &iosurface, 0)?
        };
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                wgpu::TextureFormat::Bgra8Unorm,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
                None,
            )
        };
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("lightair-surface"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        let pool = nv12_pool(w, h)?;
        let mut session: *mut VTPixelTransferSession = ptr::null_mut();
        let rc = unsafe { VTPixelTransferSession::create(None, NonNull::from(&mut session)) };
        if rc != 0 || session.is_null() {
            return None;
        }
        let transfer: CFRetained<VTPixelTransferSession> =
            unsafe { CFRetained::from_raw(NonNull::new_unchecked(session)) };
        // into BT.601, as GPUI's surface shader decodes (VideoToolbox picks BT.709 otherwise)
        unsafe {
            objc2_video_toolbox::VTSessionSetProperty(
                transfer.as_ref(),
                objc2_video_toolbox::kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
                Some(kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref()),
            )
        };
        Some(Target { bgra, texture, pool, transfer })
    }

    /// The picture last drawn (the GPU done with it) as a frame for the window.
    pub fn frame(&self) -> Option<Frame> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let rc = unsafe { CVPixelBufferPool::create_pixel_buffer(None, &self.pool, NonNull::from(&mut out)) };
        if rc != kCVReturnSuccess || out.is_null() {
            return None;
        }
        let frame: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new_unchecked(out)) };
        // GPUI's surface shader decodes BT.601
        unsafe {
            frame.set_attachment(
                kCVImageBufferYCbCrMatrixKey,
                kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref(),
                CVAttachmentMode::ShouldPropagate,
            )
        };
        (unsafe { self.transfer.transfer_image(&self.bgra, &frame) } == 0).then_some(Frame(frame))
    }
}

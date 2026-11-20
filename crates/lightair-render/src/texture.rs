//! macOS and iOS: decoded frames as IOSurface-backed pixel buffers (`CVPixelBuffer`, BGRA), drawn
//! by the compositor as Metal textures made over the same memory. The frame a hardware decoder
//! wrote is drawn as it is: no copy to the CPU, no upload. A media backend that can hand frames
//! over this way does it through [`crate::media::VideoReader::next_picture`]; everything else
//! keeps reading RGBA bytes.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

/// A decoded frame in an IOSurface-backed BGRA pixel buffer.
#[derive(Clone)]
pub struct PixelBuffer(CFRetained<CVPixelBuffer>);

// CoreVideo's buffers are reference counted thread-safely, and a decoded frame isn't written
// again once it's handed over.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl PixelBuffer {
    /// Takes over a `CVPixelBufferRef` and the one reference its caller gave up (a platform's
    /// decoder hands frames across as raw pointers). `None` for a null pointer, or a buffer
    /// that isn't BGRA in an IOSurface (it's released).
    ///
    /// # Safety
    /// `raw` must be a `CVPixelBufferRef` the caller owns one reference to, given up here.
    pub unsafe fn from_raw(raw: *mut c_void) -> Option<PixelBuffer> {
        let buffer: CFRetained<CVPixelBuffer> = unsafe { CFRetained::from_raw(NonNull::new(raw.cast())?) };
        let bgra = CVPixelBufferGetPixelFormatType(&buffer) == kCVPixelFormatType_32BGRA;
        (bgra && CVPixelBufferGetIOSurface(Some(&buffer)).is_some()).then_some(PixelBuffer(buffer))
    }

    pub fn size(&self) -> (u32, u32) {
        (CVPixelBufferGetWidth(&self.0) as u32, CVPixelBufferGetHeight(&self.0) as u32)
    }

    /// A texture over the same memory (Metal only; `None` otherwise or on failure). Sampled, it
    /// reads as RGBA, as an uploaded frame does. The buffer must outlive the GPU's use of it.
    pub(crate) fn texture(&self, device: &wgpu::Device) -> Option<wgpu::Texture> {
        let (w, h) = self.size();
        let iosurface = CVPixelBufferGetIOSurface(Some(&self.0))?;
        let raw: Retained<ProtocolObject<dyn MTLTexture>> = {
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    w as usize,
                    h as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::ShaderRead);
            hal.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, &iosurface, 0)?
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
        Some(unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("lightair-decoded"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::TextureUses::RESOURCE,
            )
        })
    }

    /// Its pixels as RGBA bytes, for what reads frames on the CPU (thumbnails, a take's own
    /// renderer).
    pub fn rgba(&self) -> Option<Vec<u8>> {
        let (w, h) = self.size();
        if unsafe { CVPixelBufferLockBaseAddress(&self.0, CVPixelBufferLockFlags::ReadOnly) } != 0 {
            return None;
        }
        let base = CVPixelBufferGetBaseAddress(&self.0) as *const u8;
        let stride = CVPixelBufferGetBytesPerRow(&self.0);
        let out = (!base.is_null()).then(|| {
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for y in 0..h as usize {
                let row = unsafe { std::slice::from_raw_parts(base.add(y * stride), w as usize * 4) };
                for p in row.as_chunks::<4>().0 {
                    out.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
                }
            }
            out
        });
        unsafe { CVPixelBufferUnlockBaseAddress(&self.0, CVPixelBufferLockFlags::ReadOnly) };
        out
    }
}

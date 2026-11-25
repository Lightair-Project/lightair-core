//! iOS (and macOS, for tests): an encoder's `CVPixelBufferPool` as render targets
//! Each frame takes a BGRA IOSurface buffer from the pool, draws
//! into a Metal texture over the same memory, and hands the buffer to the encoder.

// the free functions are clearer here than objc2's renamed methods
#![allow(deprecated)]

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferPool, CVPixelBufferPoolCreatePixelBuffer, kCVPixelFormatType_32BGRA,
};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

pub struct PoolTarget {
    pool: CFRetained<CVPixelBufferPool>,
}

// CoreVideo pools are thread-safe.
unsafe impl Send for PoolTarget {}
unsafe impl Sync for PoolTarget {}

impl PoolTarget {
    /// Takes over a `CVPixelBufferPoolRef` and the reference its caller gave up.
    ///
    /// # Safety
    /// `raw` must be a `CVPixelBufferPoolRef` the caller owns one reference to.
    pub unsafe fn from_raw(raw: u64) -> Option<PoolTarget> {
        let pool = unsafe { CFRetained::from_raw(NonNull::new(raw as usize as *mut CVPixelBufferPool)?) };
        Some(PoolTarget { pool })
    }

    /// A buffer from the pool and a render target over it; `None` when the pool is empty (the
    /// encoder holds every buffer) or the buffer isn't BGRA in an IOSurface.
    pub fn next(&self, device: &wgpu::Device) -> Option<(CFRetained<CVPixelBuffer>, wgpu::Texture)> {
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        let r = unsafe { CVPixelBufferPoolCreatePixelBuffer(None, &self.pool, NonNull::from(&mut out)) };
        if r != 0 {
            return None;
        }
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(out)?) };
        let texture = render_target(device, &buffer)?;
        Some((buffer, texture))
    }
}

/// A BGRA render target over a pixel buffer's IOSurface.
pub fn render_target(device: &wgpu::Device, buffer: &CVPixelBuffer) -> Option<wgpu::Texture> {
    if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
        return None;
    }
    let (w, h) = (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32);
    let iosurface = CVPixelBufferGetIOSurface(Some(buffer))?;
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
        desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
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
                label: Some("encoder buffer"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            },
            wgpu::TextureUses::COLOR_TARGET,
        )
    })
}

/// A pixel buffer kept by the engine too (a camera frame it samples itself).
#[derive(Clone)]
pub struct Kept(pub CFRetained<CVPixelBuffer>);

// CoreVideo's buffers are reference counted thread-safely; a pushed frame isn't written again.
unsafe impl Send for Kept {}
unsafe impl Sync for Kept {}

impl Kept {
    /// One more reference to the buffer at `raw` (`None` for null).
    ///
    /// # Safety
    /// `raw` must be a valid `CVPixelBufferRef`.
    pub unsafe fn retain(raw: *mut c_void) -> Option<Kept> {
        let ptr = NonNull::new(raw.cast::<CVPixelBuffer>())?;
        Some(Kept(unsafe { CFRetained::retain(ptr) }))
    }
}

/// A BGRA pixel buffer's IOSurface as a texture to sample (the same memory, no copy). The
/// buffer must outlive the GPU's use of it.
pub fn sampled(device: &wgpu::Device, buffer: &CVPixelBuffer) -> Option<wgpu::Texture> {
    if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
        return None;
    }
    let (w, h) = (CVPixelBufferGetWidth(buffer) as u32, CVPixelBufferGetHeight(buffer) as u32);
    let iosurface = CVPixelBufferGetIOSurface(Some(buffer))?;
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
                label: Some("camera frame"),
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

/// Gives a buffer's reference to a puller (it releases it).
pub fn into_raw(buffer: CFRetained<CVPixelBuffer>) -> usize {
    CFRetained::into_raw(buffer).as_ptr() as *mut c_void as usize
}

/// Tests: a pool of BGRA IOSurface buffers, as VideoToolbox's would be, given as a raw +1 pointer.
#[cfg(test)]
pub fn test_pool(width: u32, height: u32) -> u64 {
    use objc2_core_foundation::{CFDictionary, CFNumber, CFString, CFType};
    use objc2_core_video::{
        CVPixelBufferPoolCreate, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
        kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    };
    let empty: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[], &[]);
    let keys: [&CFString; 5] = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
            kCVPixelBufferMetalCompatibilityKey,
        ]
    };
    let (fmt, w, h, one) = (
        CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32),
        CFNumber::new_i32(width as i32),
        CFNumber::new_i32(height as i32),
        CFNumber::new_i32(1),
    );
    let values: [&CFType; 5] = [&fmt, &w, &h, empty.as_opaque(), &one];
    let attrs = CFDictionary::from_slices(&keys, &values);
    let mut pool: *mut CVPixelBufferPool = std::ptr::null_mut();
    let r = unsafe { CVPixelBufferPoolCreate(None, None, Some(attrs.as_opaque()), NonNull::from(&mut pool)) };
    assert_eq!(r, 0);
    pool as usize as u64
}

//! Native surfaces the engine presents into: the preview (a
//! `CAMetalLayer` on iOS, a `SurfaceView`'s `ANativeWindow` on Android) and, on Android, a
//! MediaCodec encoder's input surface.

use super::Gpu;

pub struct WindowSurface {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// Android: the `ANativeWindow*` this holds a reference to.
    #[cfg(target_os = "android")]
    window: usize,
}

// wgpu surfaces are Send + Sync; the window pointer is only released.
unsafe impl Send for WindowSurface {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The preview: never blocks the render thread for long.
    Preview,
    /// An encoder's input: every frame matters.
    Encoder,
}

impl WindowSurface {
    /// A surface on `handle`: a `CAMetalLayer*` (Apple) or an `ANativeWindow*` whose reference is
    /// given up to this (Android, released on drop). `None` if the GPU can't present there.
    ///
    /// # Safety
    /// `handle` must stay valid until the surface is dropped (Apple: the caller keeps the layer).
    pub unsafe fn new(gpu: &Gpu, handle: u64, size: (u32, u32), kind: Kind) -> Option<WindowSurface> {
        if handle == 0 {
            return None;
        }
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let target = wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(handle as usize as *mut std::ffi::c_void);
        #[cfg(target_os = "android")]
        let target = {
            use raw_window_handle::{AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle};
            let ptr = std::ptr::NonNull::new(handle as usize as *mut std::ffi::c_void)?;
            wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Android(AndroidDisplayHandle::new())),
                raw_window_handle: RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(ptr)),
            }
        };
        #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "android")))]
        {
            let _ = (gpu, size, kind);
            return None;
        }
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
        {
            let surface = match unsafe { gpu.shared.instance.create_surface_unsafe(target) } {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(%e, ?kind, "no surface");
                    #[cfg(target_os = "android")]
                    release_window(handle);
                    return None;
                }
            };
            let caps = surface.get_capabilities(&gpu.shared.adapter);
            let Some(&first) = caps.formats.first() else {
                tracing::warn!(?kind, "the surface offers no format");
                #[cfg(target_os = "android")]
                release_window(handle);
                return None;
            };
            // an sRGB format where there is one (the blit then writes linear light as is)
            let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(first);
            let present_mode = match kind {
                Kind::Preview => wgpu::PresentMode::AutoNoVsync,
                Kind::Encoder => wgpu::PresentMode::Fifo,
            };
            let config = wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width: size.0.max(1),
                height: size.1.max(1),
                present_mode,
                desired_maximum_frame_latency: 2,
                alpha_mode: caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
                view_formats: vec![],
                color_space: wgpu::SurfaceColorSpace::Auto,
            };
            let ws = WindowSurface {
                surface,
                config,
                #[cfg(target_os = "android")]
                window: handle as usize,
            };
            ws.configure(gpu).then_some(ws)
        }
    }

    fn configure(&self, gpu: &Gpu) -> bool {
        let scope = gpu.device().push_error_scope(wgpu::ErrorFilter::Validation);
        self.surface.configure(gpu.device(), &self.config);
        pollster::block_on(scope.pop()).is_none()
    }

    pub fn resize(&mut self, gpu: &Gpu, size: (u32, u32)) {
        self.config.width = size.0.max(1);
        self.config.height = size.1.max(1);
        let _ = self.configure(gpu);
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// The next texture to draw into, or `None` when there's none ready now (skip this frame).
    pub fn texture(&self, gpu: &Gpu) -> Option<wgpu::SurfaceTexture> {
        match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => Some(t),
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                let _ = self.configure(gpu);
                None
            }
            _ => None,
        }
    }
}

#[cfg(target_os = "android")]
pub fn release_window(handle: u64) {
    if handle != 0 {
        unsafe { ndk_sys::ANativeWindow_release(handle as usize as *mut ndk_sys::ANativeWindow) };
    }
}

impl Drop for WindowSurface {
    fn drop(&mut self) {
        #[cfg(target_os = "android")]
        release_window(self.window as u64);
    }
}

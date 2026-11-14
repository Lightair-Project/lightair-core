//! Devices as video sources.

/// Lets an iPhone or iPad connected by USB appear as a video device (its screen), as QuickTime
/// does (macOS: CoreMediaIO's screen capture devices; no permission involved). Call once, early.
/// Elsewhere, nothing to do.
pub fn allow_ios_screens() {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct Address {
            selector: u32,
            scope: u32,
            element: u32,
        }
        #[link(name = "CoreMediaIO", kind = "framework")]
        unsafe extern "C" {
            fn CMIOObjectSetPropertyData(
                object: u32,
                address: *const Address,
                qualifier_size: u32,
                qualifier: *const std::ffi::c_void,
                size: u32,
                data: *const std::ffi::c_void,
            ) -> i32;
        }
        // kCMIOHardwarePropertyAllowScreenCaptureDevices ('yes '), global scope ('glob'),
        // the main element, on the system object (1)
        let address = Address { selector: 0x7965_7320, scope: 0x676C_6F62, element: 0 };
        let allow: u32 = 1;
        let status = unsafe {
            CMIOObjectSetPropertyData(1, &address, 0, std::ptr::null(), 4, &allow as *const u32 as *const std::ffi::c_void)
        };
        if status != 0 {
            tracing::warn!(status, "iPhone and iPad screens won't show as devices");
        }
    }
}

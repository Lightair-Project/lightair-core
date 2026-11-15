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

/// Whether a camera is a virtual one (a camera made by an app: OBS's, a meeting app's…) rather
/// than hardware. macOS: its transport type (`kIOAudioDeviceTransportTypeVirtual`, as
/// AVFoundation reports it); elsewhere, not known (false). `uid` is the device's unique id.
pub fn camera_is_virtual(uid: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2::msg_send;
        use objc2::rc::Retained;
        use objc2::runtime::{AnyClass, AnyObject};
        use objc2_foundation::NSString;
        #[link(name = "AVFoundation", kind = "framework")]
        unsafe extern "C" {}
        let Some(class) = AnyClass::get(c"AVCaptureDevice") else { return false };
        let id = NSString::from_str(uid);
        unsafe {
            let device: Option<Retained<AnyObject>> = msg_send![class, deviceWithUniqueID: &*id];
            // 'virt'
            device.is_some_and(|d| {
                let transport: i32 = msg_send![&*d, transportType];
                transport == 0x7669_7274
            })
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = uid;
        false
    }
}

//! Files as the OS's file manager shows them: free space, reveal, move to the Trash (Recycle
//! Bin).

use std::path::Path;

/// Free space (for the user) on the volume holding `path`, or its nearest existing parent.
pub fn free_bytes(path: &Path) -> Option<u64> {
    space(path).map(|(free, _)| free)
}

/// Free space (for the user) and the size of the volume holding `path`, or its nearest
/// existing parent: (free, total) in bytes.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let existing = path.ancestors().find(|p| p.exists())?;
    imp::space(existing)
}

/// Shows `path` selected in Finder (Explorer).
pub fn reveal(path: &Path) -> std::io::Result<()> {
    imp::reveal(path)
}

/// Moves `path` to the Trash (Recycle Bin), where it can be put back.
pub fn trash(path: &Path) -> std::io::Result<()> {
    imp::trash(path)
}

#[cfg(unix)]
mod unix {
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn space(path: &Path) -> Option<(u64, u64)> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
        (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0)
            .then_some((st.f_bavail as u64 * st.f_frsize as u64, st.f_blocks as u64 * st.f_frsize as u64))
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::io;
    use std::path::Path;

    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::NSString;

    pub use super::unix::space;

    pub fn reveal(path: &Path) -> io::Result<()> {
        let status = std::process::Command::new("/usr/bin/open").arg("-R").arg(path).status()?;
        if status.success() { Ok(()) } else { Err(io::Error::other("Finder could not show it")) }
    }

    pub fn trash(path: &Path) -> io::Result<()> {
        let missing = || io::Error::other("Foundation is missing");
        let fm_class = AnyClass::get(c"NSFileManager").ok_or_else(missing)?;
        let url_class = AnyClass::get(c"NSURL").ok_or_else(missing)?;
        let s = NSString::from_str(&path.to_string_lossy());
        unsafe {
            let url: Option<Retained<AnyObject>> = msg_send![url_class, fileURLWithPath: &*s];
            let url = url.ok_or_else(missing)?;
            let fm: Retained<AnyObject> = msg_send![fm_class, defaultManager];
            let ok: Bool = msg_send![
                &*fm,
                trashItemAtURL: &*url,
                resultingItemURL: std::ptr::null_mut::<*mut AnyObject>(),
                error: std::ptr::null_mut::<*mut AnyObject>()
            ];
            if ok.as_bool() { Ok(()) } else { Err(io::Error::other("could not move it to the Trash")) }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW,
    };

    fn wide(path: &Path, nulls: usize) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(std::iter::repeat_n(0, nulls)).collect()
    }

    pub fn space(path: &Path) -> Option<(u64, u64)> {
        let p = wide(path, 1);
        let (mut free, mut total) = (0u64, 0u64);
        (unsafe { GetDiskFreeSpaceExW(p.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) } != 0)
            .then_some((free, total))
    }

    pub fn reveal(path: &Path) -> io::Result<()> {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path.as_os_str());
        // Explorer's exit code means nothing
        std::process::Command::new("explorer.exe").arg(arg).spawn().map(|_| ())
    }

    pub fn trash(path: &Path) -> io::Result<()> {
        // a list of paths, ended by an empty one
        let from = wide(path, 2);
        let mut op = SHFILEOPSTRUCTW {
            wFunc: FO_DELETE,
            pFrom: from.as_ptr(),
            fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
            ..unsafe { std::mem::zeroed() }
        };
        match unsafe { SHFileOperationW(&mut op) } {
            0 if op.fAnyOperationsAborted == 0 => Ok(()),
            code => Err(io::Error::other(format!("could not move it to the Recycle Bin ({code})"))),
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::io;
    use std::path::Path;

    pub use super::unix::space;

    pub fn reveal(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    pub fn trash(_: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn free_space_of_a_new_folder_is_its_volumes() {
        let dir = std::env::temp_dir().join("lightair-platform-free").join("not-made-yet");
        assert!(super::free_bytes(&dir).is_some_and(|b| b > 0));
        let (free, total) = super::space(&dir).unwrap();
        assert!(total >= free && total > 0, "free {free} of {total}");
    }

    // (not on macOS: it would leave the file in the user's Trash)
    #[cfg(windows)]
    #[test]
    fn trash_takes_a_file_away() {
        let dir = std::env::temp_dir().join(format!("lightair-trash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("take me.txt");
        std::fs::write(&file, b"x").unwrap();
        super::trash(&file).unwrap();
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! A game's "Disable fullscreen optimizations" compatibility flag: reading it, and taking it off
//! for the current user so Windows converts the game's exclusive fullscreen to the flip model
//! (which Windows Graphics Capture sees) at its next start.
//!
//! The flag is the `DISABLEDXMAXIMIZEDWINDOWEDMODE` layer in the value named after the game's
//! full path under `AppCompatFlags\Layers`: the current user's (`HKEY_CURRENT_USER`, changed
//! here without admin rights) and every user's (`HKEY_LOCAL_MACHINE`, only ever read: Lightair
//! says where it is instead). A value holds its layers separated by spaces, after a `~` (and on
//! some systems a `$`) marking how they were set, for example
//! `~ DISABLEDXMAXIMIZEDWINDOWEDMODE HIGHDPIAWARE`.
//!
//! What a change does is worked out first ([`plan`], from [`without_fso_off`], which is pure);
//! [`apply`] then writes exactly that, only if the value is still what was read. Nothing here
//! changes the registry unless [`apply`] is called.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ,
    RRF_SUBKEY_WOW6464KEY, RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::core::PCWSTR;

use super::{from_wide, wide};
use crate::Error;

/// Where the layers are, under the current user's and the machine's keys.
pub const LAYERS_KEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

/// The layer "Disable fullscreen optimizations" sets.
pub const FSO_OFF: &str = "DISABLEDXMAXIMIZEDWINDOWEDMODE";

/// Whether a layers value turns fullscreen optimisations off.
pub fn has_fso_off(value: &str) -> bool {
    value.split_whitespace().any(|layer| layer.eq_ignore_ascii_case(FSO_OFF))
}

/// The layers value without the fullscreen optimisations flag: `None` when it doesn't have the
/// flag (nothing to change), `Some(None)` when nothing but its markers would be left (the value
/// goes), `Some(Some(new))` otherwise, every other layer and marker kept in order.
pub fn without_fso_off(value: &str) -> Option<Option<String>> {
    if !has_fso_off(value) {
        return None;
    }
    let kept: Vec<&str> = value.split_whitespace().filter(|layer| !layer.eq_ignore_ascii_case(FSO_OFF)).collect();
    let markers_only = kept.iter().all(|layer| *layer == "~" || *layer == "$");
    Some(if markers_only { None } else { Some(kept.join(" ")) })
}

/// A game's layers values, where it has them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Flags {
    /// The current user's (`HKEY_CURRENT_USER`).
    pub user: Option<String>,
    /// Every user's (`HKEY_LOCAL_MACHINE`).
    pub machine: Option<String>,
}

impl Flags {
    /// Fullscreen optimisations are off for this game for the current user (Lightair can turn
    /// them back on).
    pub fn off_for_user(&self) -> bool {
        self.user.as_deref().is_some_and(has_fso_off)
    }

    /// Off for every user (only someone with admin rights can turn them back on).
    pub fn off_for_machine(&self) -> bool {
        self.machine.as_deref().is_some_and(has_fso_off)
    }
}

/// A string value under `LAYERS_KEY`, named `name`.
fn read_value(root: HKEY, name: &str) -> Option<String> {
    let key = wide(LAYERS_KEY);
    let name = wide(name);
    let flags = if root == HKEY_LOCAL_MACHINE { RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY } else { RRF_RT_REG_SZ };
    let mut bytes = 0u32;
    let sized =
        unsafe { RegGetValueW(root, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr()), flags, None, None, Some(&mut bytes)) };
    if sized != ERROR_SUCCESS || bytes == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (bytes as usize).div_ceil(2) + 1];
    let mut bytes = (buffer.len() * 2) as u32;
    let read = unsafe {
        RegGetValueW(
            root,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            flags,
            None,
            Some(buffer.as_mut_ptr() as *mut _),
            Some(&mut bytes),
        )
    };
    (read == ERROR_SUCCESS).then(|| from_wide(&buffer))
}

/// The layers values Windows has for the game at `exe` (its full path).
pub fn read(exe: &str) -> Flags {
    Flags { user: read_value(HKEY_CURRENT_USER, exe), machine: read_value(HKEY_LOCAL_MACHINE, exe) }
}

/// Where the every-user flag is, for the Inspector to name.
pub fn machine_location(exe: &str) -> String {
    format!(r"HKEY_LOCAL_MACHINE\{LAYERS_KEY}, value {exe}")
}

/// Taking the flag off one game for the current user, worked out but not done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The game's full path (the value's name).
    pub exe: String,
    /// The value now.
    pub before: String,
    /// The value after; `None` when the value is deleted.
    pub after: Option<String>,
}

impl Change {
    /// What it does, in words.
    pub fn describe(&self) -> String {
        match &self.after {
            Some(after) => {
                format!(r"HKEY_CURRENT_USER\{LAYERS_KEY}, {}: “{}” becomes “{after}”", self.exe, self.before)
            }
            None => format!(r"HKEY_CURRENT_USER\{LAYERS_KEY}, {}: “{}” is removed", self.exe, self.before),
        }
    }
}

/// What turning fullscreen optimisations back on for the game at `exe` would change for the
/// current user; `None` when they aren't off for the current user.
pub fn plan(exe: &str) -> Option<Change> {
    let before = read_value(HKEY_CURRENT_USER, exe)?;
    let after = without_fso_off(&before)?;
    Some(Change { exe: exe.to_string(), before, after })
}

/// Makes `change`, if the value is still what it was when the change was planned (else
/// [`Error::Failed`], and nothing is written). The game sees it at its next start.
pub fn apply(change: &Change) -> Result<(), Error> {
    let key_name = wide(LAYERS_KEY);
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key_name.as_ptr()), None, KEY_QUERY_VALUE | KEY_SET_VALUE, &mut key)
    };
    if opened != ERROR_SUCCESS {
        return Err(if opened == ERROR_FILE_NOT_FOUND {
            Error::NotFound("the compatibility layers key".into())
        } else {
            Error::Failed(format!("opening the compatibility layers key: error {}", opened.0))
        });
    }
    let done = (|| {
        if read_value(HKEY_CURRENT_USER, &change.exe).as_deref() != Some(change.before.as_str()) {
            return Err(Error::Failed("the game's compatibility settings changed since they were read".into()));
        }
        let name = wide(&change.exe);
        let written = match &change.after {
            Some(after) => {
                let data = wide(after);
                let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
                unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }
            }
            None => unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) },
        };
        if written == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(Error::Failed(format!("changing the game's compatibility settings: error {}", written.0)))
        }
    })();
    let _ = unsafe { RegCloseKey(key) };
    done
}

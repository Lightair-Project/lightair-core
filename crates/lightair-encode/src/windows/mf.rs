//! Media Foundation plumbing the video and sound encoders share: starting the runtime, finding
//! encoder transforms, media types, codec properties and samples.

use std::ptr;

use windows::Win32::Media::MediaFoundation::{
    ICodecAPI, IMFActivate, IMFAttributes, IMFMediaBuffer, IMFSample, IMFTransform, MF_VERSION, MFCreateMemoryBuffer,
    MFCreateSample, MFSTARTUP_FULL, MFShutdown, MFStartup, MFT_ENUM_FLAG, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, HRESULT, PWSTR};

use crate::Error;

/// Media Foundation started for as long as this lives (the runtime counts starts and stops).
pub(crate) struct Runtime;

impl Runtime {
    /// Joins the calling thread to the multithreaded apartment (a thread already in another
    /// apartment keeps it: Media Foundation's objects work from any) and starts Media
    /// Foundation.
    pub(crate) fn start() -> Result<Runtime, Error> {
        join_apartment();
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(platform("Media Foundation wouldn't start"))?;
        Ok(Runtime)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

/// Joins the calling thread to the multithreaded apartment unless it is in one already.
pub(crate) fn join_apartment() {
    // S_FALSE (already joined) and RPC_E_CHANGED_MODE (a single-threaded apartment) both leave
    // the thread able to use Media Foundation
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// An `Error::Platform` from a failed call, with its HRESULT.
pub(crate) fn platform(what: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |e| Error::Platform { what, status: e.code().0 }
}

pub(crate) fn status(what: &'static str, hr: HRESULT) -> Error {
    Error::Platform { what, status: hr.0 }
}

/// Two 32-bit values packed as Media Foundation's size and ratio attributes hold them.
pub(crate) fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// A GPU maker as Media Foundation's hardware transforms name them (`VEN_10DE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    /// Another maker's PCI vendor ID.
    Other(u32),
}

impl Vendor {
    pub fn from_id(id: u32) -> Vendor {
        match id {
            0x10DE => Vendor::Nvidia,
            0x1002 | 0x1022 => Vendor::Amd,
            0x8086 => Vendor::Intel,
            n => Vendor::Other(n),
        }
    }

    /// From a transform's `VEN_xxxx` vendor attribute.
    pub fn from_attribute(text: &str) -> Option<Vendor> {
        let hex = text.trim().strip_prefix("VEN_").or_else(|| text.trim().strip_prefix("ven_"))?;
        u32::from_str_radix(hex.get(..4)?, 16).ok().map(Vendor::from_id)
    }

    pub fn id(self) -> u32 {
        match self {
            Vendor::Nvidia => 0x10DE,
            Vendor::Amd => 0x1002,
            Vendor::Intel => 0x8086,
            Vendor::Other(n) => n,
        }
    }
}

/// One encoder transform found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    /// What the driver calls it ("NVIDIA H.264 Encoder MFT", "Intel® Quick Sync Video H.264
    /// Encoder MFT").
    pub name: String,
    /// `None` for a software transform.
    pub vendor: Option<Vendor>,
}

fn string_attribute(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut text = PWSTR::null();
    let mut len = 0u32;
    unsafe { attributes.GetAllocatedString(key, &mut text, &mut len) }.ok()?;
    let out = unsafe { text.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
    out
}

/// The transforms of `category` that turn `input` into `output`, best first, with what they
/// are.
pub(crate) fn enumerate(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<MFT_REGISTER_TYPE_INFO>,
    output: Option<MFT_REGISTER_TYPE_INFO>,
) -> Result<Vec<(IMFActivate, EncoderInfo)>, Error> {
    let mut array: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            input.as_ref().map(|i| i as *const _),
            output.as_ref().map(|o| o as *const _),
            &mut array,
            &mut count,
        )
    }
    .map_err(platform("Media Foundation couldn't list its encoders"))?;
    if array.is_null() {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        // each element's reference moves out of the array, which is then freed
        let Some(activate) = (unsafe { ptr::read(array.add(i)) }) else {
            continue;
        };
        let name = string_attribute(&activate, &MFT_FRIENDLY_NAME_Attribute).unwrap_or_default();
        let vendor = string_attribute(&activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)
            .as_deref()
            .and_then(Vendor::from_attribute);
        out.push((activate, EncoderInfo { name, vendor }));
    }
    unsafe { CoTaskMemFree(Some(array as *const _)) };
    Ok(out)
}

/// Sets one of the encoder's codec properties; `false` when the encoder doesn't take it.
pub(crate) fn set_u32(codec: &ICodecAPI, key: &GUID, value: u32) -> bool {
    let value = VARIANT::from(value);
    unsafe { codec.SetValue(key, &value) }.is_ok()
}

pub(crate) fn set_bool(codec: &ICodecAPI, key: &GUID, value: bool) -> bool {
    let value = VARIANT::from(value);
    unsafe { codec.SetValue(key, &value) }.is_ok()
}

/// A sample holding `bytes` in a new memory buffer, at `time_hns` for `duration_hns`.
pub(crate) fn memory_sample(bytes: &[u8], time_hns: i64, duration_hns: i64) -> Result<IMFSample, Error> {
    let what = "Media Foundation couldn't make a sample";
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len().max(1) as u32).map_err(platform(what))?;
        let mut at: *mut u8 = ptr::null_mut();
        buffer.Lock(&mut at, None, None).map_err(platform(what))?;
        ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        buffer.Unlock().map_err(platform(what))?;
        buffer.SetCurrentLength(bytes.len() as u32).map_err(platform(what))?;
        let sample = MFCreateSample().map_err(platform(what))?;
        sample.AddBuffer(&buffer).map_err(platform(what))?;
        sample.SetSampleTime(time_hns).map_err(platform(what))?;
        sample.SetSampleDuration(duration_hns).map_err(platform(what))?;
        Ok(sample)
    }
}

/// An empty sample with a memory buffer of `size` bytes, for transforms that want the caller
/// to give them somewhere to write.
pub(crate) fn output_sample(size: u32) -> Result<IMFSample, Error> {
    let what = "Media Foundation couldn't make an output sample";
    unsafe {
        let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(size.max(4096)).map_err(platform(what))?;
        let sample = MFCreateSample().map_err(platform(what))?;
        sample.AddBuffer(&buffer).map_err(platform(what))?;
        Ok(sample)
    }
}

/// A sample's bytes, all its buffers joined.
pub(crate) fn sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, Error> {
    let what = "Media Foundation couldn't read an encoded sample";
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(platform(what))?;
        let mut at: *mut u8 = ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut at, None, Some(&mut len)).map_err(platform(what))?;
        let out = if at.is_null() { Vec::new() } else { std::slice::from_raw_parts(at, len as usize).to_vec() };
        buffer.Unlock().map_err(platform(what))?;
        Ok(out)
    }
}

/// A transform's input and output stream IDs (0 and 0 when it numbers them itself).
pub(crate) fn stream_ids(transform: &IMFTransform) -> (u32, u32) {
    let mut input = [0u32];
    let mut output = [0u32];
    match unsafe { transform.GetStreamIDs(&mut input, &mut output) } {
        Ok(()) => (input[0], output[0]),
        // E_NOTIMPL: fixed streams numbered from 0
        Err(_) => (0, 0),
    }
}

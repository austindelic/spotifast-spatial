//! Resolve the existing device-name setting to the UID AVFoundation accepts.

use std::ffi::c_void;
use std::ptr::{NonNull, null, null_mut};

use objc2::rc::Retained;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectPropertyAddress,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_foundation::NSString;

use super::Result;

pub(super) struct Route {
    pub id: u32,
    pub name: String,
    /// None follows the system default instead of pinning its current UID.
    pub uid: Option<Retained<NSString>>,
}

impl Route {
    /// Default output changes are followed by AVFoundation without replacing
    /// the renderer and losing its spatialization session.
    pub(super) fn requires_reopen(&self, next: &Self) -> bool {
        self.uid != next.uid
    }
}

pub(super) fn resolve(preferred: Option<&str>) -> Result<Route> {
    if let Some(preferred) = preferred.map(str::trim).filter(|name| !name.is_empty()) {
        for id in ids(
            kAudioObjectSystemObject as u32,
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        )? {
            if ids(
                id,
                kAudioDevicePropertyStreams,
                kAudioObjectPropertyScopeOutput,
            )
            .is_ok_and(|streams| !streams.is_empty())
                && string(id, kAudioObjectPropertyName).is_ok_and(|name| name == preferred)
            {
                return Ok(Route {
                    id,
                    name: preferred.into(),
                    uid: Some(NSString::from_str(&string(
                        id,
                        kAudioDevicePropertyDeviceUID,
                    )?)),
                });
            }
        }
    }
    let devices = ids(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )?;
    let id = devices
        .first()
        .copied()
        .filter(|id| *id != 0)
        .ok_or_else(|| crate::sink::NO_DEVICE.to_string())?;
    Ok(Route {
        id,
        name: string(id, kAudioObjectPropertyName)?,
        uid: None,
    })
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn status(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("Cannot read the audio output (Core Audio {code})"))
    }
}

/// Only called for properties whose documented representation is u32 or u32[].
fn ids(object: u32, selector: u32, scope: u32) -> Result<Vec<u32>> {
    let mut address = address(selector, scope);
    let mut bytes = 0;
    // SAFETY: Valid address/output pointers, no qualifier data.
    status(unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&mut address),
            0,
            null(),
            NonNull::from(&mut bytes),
        )
    })?;
    if !bytes.is_multiple_of(size_of::<u32>() as u32) {
        return Err("Invalid Core Audio device list".into());
    }
    let mut values = vec![0; bytes as usize / size_of::<u32>()];
    if values.is_empty() {
        return Ok(values);
    }
    let allocated = bytes;
    // SAFETY: The array is aligned and sized for the property. Core Audio must
    // not write beyond the supplied byte count if devices change mid-query.
    status(unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut address),
            0,
            null(),
            NonNull::from(&mut bytes),
            NonNull::new(values.as_mut_ptr().cast::<c_void>()).expect("nonempty array"),
        )
    })?;
    if bytes > allocated || !bytes.is_multiple_of(size_of::<u32>() as u32) {
        return Err("Audio devices changed while being read".into());
    }
    values.truncate(bytes as usize / size_of::<u32>());
    Ok(values)
}

/// Core Audio transfers ownership of the CFString for name and UID properties.
fn string(object: u32, selector: u32) -> Result<String> {
    let mut address = address(selector, kAudioObjectPropertyScopeGlobal);
    let mut raw: *mut CFString = null_mut();
    let mut bytes = size_of_val(&raw) as u32;
    // SAFETY: These selectors return one CFStringRef into this pointer slot.
    status(unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut address),
            0,
            null(),
            NonNull::from(&mut bytes),
            NonNull::from(&mut raw).cast(),
        )
    })?;
    let raw = NonNull::new(raw).ok_or("Audio output has no name or UID")?;
    // SAFETY: The property transfers a retained CFString reference to us.
    let string = unsafe { CFRetained::from_raw(raw) };
    Ok(string.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_device_changes_keep_the_renderer_but_named_device_changes_reopen() {
        use objc2_foundation::NSString;
        let speakers = Route {
            id: 1,
            name: "speakers".into(),
            uid: None,
        };
        let airpods = Route {
            id: 2,
            name: "AirPods".into(),
            uid: None,
        };
        let named = Route {
            id: 2,
            name: "AirPods".into(),
            uid: Some(NSString::from_str("airpods")),
        };
        assert!(!speakers.requires_reopen(&airpods));
        assert!(!airpods.requires_reopen(&speakers));
        assert!(airpods.requires_reopen(&named));
        assert!(named.requires_reopen(&speakers));
        assert!(!named.requires_reopen(&named));
    }
}

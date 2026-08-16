//! Windows audio endpoint enumeration.
//!
//! Capture endpoints and render endpoints are presented in one flat list, with
//! render entries tagged `[LOOPBACK]` — those are how system audio (and hence
//! Elite Dangerous) is monitored. The descriptor type itself is portable so the
//! UI and configuration code compile anywhere; only the enumeration is gated.

/// What a device gives us when opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A microphone or line input.
    Capture,
    /// An output endpoint, opened in loopback so we hear what it plays.
    RenderLoopback,
}

impl DeviceKind {
    pub fn is_loopback(self) -> bool {
        self == DeviceKind::RenderLoopback
    }

    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Capture => "input",
            DeviceKind::RenderLoopback => "loopback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// Endpoint id — stable across reboots, and what gets persisted to config.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
}

impl AudioDevice {
    /// One line for the device picker.
    pub fn display_name(&self) -> String {
        let mut s = self.name.clone();
        if self.kind.is_loopback() {
            s.push_str(" [LOOPBACK]");
        }
        if self.is_default {
            s.push_str(" (default)");
        }
        s
    }
}

/// Pick the device matching `id`, falling back to the default render endpoint
/// (i.e. system audio) when `id` is empty or no longer present.
pub fn select<'a>(devices: &'a [AudioDevice], id: &str) -> Option<&'a AudioDevice> {
    if !id.is_empty() {
        if let Some(d) = devices.iter().find(|d| d.id == id) {
            return Some(d);
        }
        log::warn!("configured device {id} is not present; falling back to the default output");
    }
    devices
        .iter()
        .find(|d| d.kind.is_loopback() && d.is_default)
        .or_else(|| devices.iter().find(|d| d.kind.is_loopback()))
        .or_else(|| devices.first())
}

#[cfg(windows)]
mod imp {
    use super::{AudioDevice, DeviceKind};
    use anyhow::{Context, Result};
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eCapture,
        eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoCreateInstance,
        CoInitializeEx, STGM_READ,
    };
    use windows::core::PCWSTR;

    /// Initialize COM for a thread that may also host a window.
    ///
    /// Deliberately a *single-threaded* apartment. Window creation calls
    /// `OleInitialize`, which requires STA on the same thread — putting the UI
    /// thread into an MTA (as enumerating devices used to) makes that fail with
    /// `RPC_E_CHANGED_MODE` and panics the moment a window is opened.
    ///
    /// Endpoint enumeration is happy in either apartment, so STA costs nothing.
    /// The capture thread is separate and uses [`ensure_com_mta`].
    pub fn ensure_com() {
        unsafe {
            // The HRESULT is informational: S_FALSE means already initialized,
            // RPC_E_CHANGED_MODE means another component already chose the
            // apartment. Neither stops us using the interfaces.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
    }

    /// Initialize COM for the capture thread, which wants a multi-threaded
    /// apartment and never creates a window.
    pub fn ensure_com_mta() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    fn friendly_name(device: &IMMDevice) -> Result<String> {
        unsafe {
            let store = device
                .OpenPropertyStore(STGM_READ)
                .context("opening endpoint property store")?;
            let value = store
                .GetValue(&PKEY_Device_FriendlyName)
                .context("reading endpoint friendly name")?;
            Ok(value.to_string())
        }
    }

    fn endpoint_id(device: &IMMDevice) -> Result<String> {
        unsafe {
            let id = device.GetId().context("reading endpoint id")?;
            let s = id.to_string().context("endpoint id was not valid UTF-16")?;
            windows::Win32::System::Com::CoTaskMemFree(Some(id.0 as *const _));
            Ok(s)
        }
    }

    pub fn enumerate() -> Result<Vec<AudioDevice>> {
        ensure_com();
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("creating the audio endpoint enumerator")?;

            let mut devices = Vec::new();
            for (flow, kind) in [
                (eRender, DeviceKind::RenderLoopback),
                (eCapture, DeviceKind::Capture),
            ] {
                // A missing default endpoint is normal (no microphone at all),
                // so this is not an error.
                let default_id = enumerator
                    .GetDefaultAudioEndpoint(flow, eConsole)
                    .ok()
                    .and_then(|d| endpoint_id(&d).ok());

                let collection = enumerator
                    .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
                    .with_context(|| format!("enumerating {} endpoints", kind.label()))?;

                for i in 0..collection.GetCount().unwrap_or(0) {
                    let Ok(device) = collection.Item(i) else {
                        continue;
                    };
                    let Ok(id) = endpoint_id(&device) else {
                        continue;
                    };
                    let name = friendly_name(&device)
                        .unwrap_or_else(|_| format!("Unknown {}", kind.label()));
                    let is_default = default_id.as_deref() == Some(id.as_str());
                    devices.push(AudioDevice {
                        id,
                        name,
                        kind,
                        is_default,
                    });
                }
            }
            Ok(devices)
        }
    }

    /// Re-open an endpoint by id for capture.
    ///
    /// Assumes the calling thread has already initialized COM — it is the
    /// capture thread, which wants an MTA and must not be pushed into an STA
    /// from here.
    pub fn open(id: &str) -> Result<IMMDevice> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("creating the audio endpoint enumerator")?;
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            enumerator
                .GetDevice(PCWSTR(wide.as_ptr()))
                .with_context(|| format!("opening audio endpoint {id}"))
        }
    }
}

#[cfg(windows)]
pub use imp::{ensure_com, ensure_com_mta, enumerate, open};

#[cfg(not(windows))]
mod imp {
    use super::AudioDevice;
    use anyhow::Result;

    /// No endpoints off Windows. The synthetic sources and file input still
    /// work, which is the point of keeping the rest of the crate portable.
    pub fn enumerate() -> Result<Vec<AudioDevice>> {
        Ok(Vec::new())
    }

    pub fn ensure_com() {}
    pub fn ensure_com_mta() {}
}

#[cfg(not(windows))]
pub use imp::{ensure_com, ensure_com_mta, enumerate};

#[cfg(test)]
mod tests {
    use super::*;

    fn devices() -> Vec<AudioDevice> {
        vec![
            AudioDevice {
                id: "mic".into(),
                name: "Microphone".into(),
                kind: DeviceKind::Capture,
                is_default: true,
            },
            AudioDevice {
                id: "spk".into(),
                name: "Speakers (Realtek)".into(),
                kind: DeviceKind::RenderLoopback,
                is_default: true,
            },
            AudioDevice {
                id: "hdmi".into(),
                name: "HDMI Output".into(),
                kind: DeviceKind::RenderLoopback,
                is_default: false,
            },
        ]
    }

    #[test]
    fn loopback_devices_are_tagged_in_the_picker() {
        let d = devices();
        assert_eq!(
            d[1].display_name(),
            "Speakers (Realtek) [LOOPBACK] (default)"
        );
        assert_eq!(d[2].display_name(), "HDMI Output [LOOPBACK]");
        assert_eq!(d[0].display_name(), "Microphone (default)");
    }

    #[test]
    fn an_empty_id_selects_the_default_output() {
        // System audio is the point of the tool, so the fallback is loopback,
        // not the default microphone.
        let d = devices();
        assert_eq!(select(&d, "").unwrap().id, "spk");
    }

    #[test]
    fn a_configured_id_is_honoured() {
        let d = devices();
        assert_eq!(select(&d, "hdmi").unwrap().id, "hdmi");
        assert_eq!(select(&d, "mic").unwrap().id, "mic");
    }

    #[test]
    fn a_missing_device_falls_back_rather_than_failing() {
        let d = devices();
        assert_eq!(select(&d, "unplugged-usb-interface").unwrap().id, "spk");
    }

    #[test]
    fn falls_back_to_any_loopback_when_none_is_default() {
        let d: Vec<AudioDevice> = devices()
            .into_iter()
            .map(|mut x| {
                x.is_default = false;
                x
            })
            .collect();
        assert!(select(&d, "").unwrap().kind.is_loopback());
    }

    #[test]
    fn selecting_from_an_empty_list_yields_nothing() {
        assert!(select(&[], "").is_none());
        assert!(select(&[], "anything").is_none());
    }

    #[test]
    fn capture_only_machines_still_select_something() {
        let only_capture = vec![AudioDevice {
            id: "mic".into(),
            name: "Microphone".into(),
            kind: DeviceKind::Capture,
            is_default: true,
        }];
        assert_eq!(select(&only_capture, "").unwrap().id, "mic");
    }
}

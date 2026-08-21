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
/// Choose an endpoint to listen to. **Loopback only.**
///
/// There is deliberately no fallback to "whatever device exists". This tool
/// listens to what the game plays, which is a render endpoint opened in
/// loopback; a microphone is never a correct answer to that question. The
/// fallback used to be `devices.first()`, and on a machine whose headphones had
/// been unplugged that resolved to a microphone — so the tool opened it, wrote
/// it to disk as captures, and recorded the room instead of the game. Returning
/// `None` and doing nothing is the only acceptable behaviour when there is
/// nothing to listen to.
pub fn select<'a>(devices: &'a [AudioDevice], id: &str) -> Option<&'a AudioDevice> {
    if !id.is_empty() {
        match devices.iter().find(|d| d.id == id) {
            // An explicitly configured device still has to be one we can
            // legitimately listen to.
            Some(d) if d.kind.is_loopback() => return Some(d),
            Some(d) => log::warn!(
                "configured device {} is a {} endpoint, not an output; ignoring it",
                d.display_name(),
                d.kind.label()
            ),
            None => log::warn!(
                "configured device {id} is not present; falling back to the default output"
            ),
        }
    }
    devices
        .iter()
        .find(|d| d.kind.is_loopback() && d.is_default)
        .or_else(|| devices.iter().find(|d| d.kind.is_loopback()))
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
    fn a_configured_id_is_honoured_if_it_is_an_output() {
        let d = devices();
        assert_eq!(select(&d, "hdmi").unwrap().id, "hdmi");
        // A microphone named explicitly is still refused, and the default output
        // is used instead. Configuring one is far more likely to be a mistake
        // than a decision, and the cost of being wrong is recording the room.
        assert!(
            select(&d, "mic").is_some_and(|s| s.kind.is_loopback()),
            "a configured capture endpoint must not be selected"
        );
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

    /// The behaviour that replaced `capture_only_machines_still_select_something`.
    ///
    /// That test asserted we would fall back to any endpoint at all, and it was
    /// wrong in a way that only showed up in the field: with the headphones
    /// unplugged there was no output endpoint, the fallback resolved to a
    /// microphone, and the tool opened it and began writing the room to disk as
    /// signal captures. There is no acceptable fallback here — with nothing to
    /// listen to, the answer is to listen to nothing.
    #[test]
    fn a_machine_with_no_output_endpoint_selects_nothing() {
        let only_capture = vec![AudioDevice {
            id: "mic".into(),
            name: "Microphone".into(),
            kind: DeviceKind::Capture,
            is_default: true,
        }];
        assert!(
            select(&only_capture, "").is_none(),
            "a microphone is never a substitute for the game's output"
        );
        assert!(
            select(&only_capture, "mic").is_none(),
            "not even when it is named explicitly"
        );
        assert!(select(&[], "").is_none(), "and no devices means no device");
    }
}

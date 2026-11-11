//! Windows audio endpoint enumeration.
//!
//! Capture endpoints and render endpoints are presented in one flat list, with
//! render entries tagged `[LOOPBACK]` — those are how system audio (and hence
//! Elite Dangerous) is monitored. The descriptor type itself is portable so the
//! UI and configuration code compile anywhere; only the enumeration is gated.
//!
//! On Windows 11 the list also carries **programs**: every process currently
//! playing audio, tagged `[PROGRAM]`. Listening to one program hears only that
//! program — no notification chimes, no voice chat, no browser — which is the
//! thing a loopback of the whole output cannot do.

/// What a device gives us when opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// A microphone or line input.
    Capture,
    /// An output endpoint, opened in loopback so we hear what it plays.
    RenderLoopback,
    /// One program's audio, and nothing else's. Windows 11 and later.
    ProcessLoopback,
}

impl DeviceKind {
    pub fn is_loopback(self) -> bool {
        matches!(
            self,
            DeviceKind::RenderLoopback | DeviceKind::ProcessLoopback
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            DeviceKind::Capture => "input",
            DeviceKind::RenderLoopback => "loopback",
            DeviceKind::ProcessLoopback => "program",
        }
    }
}

/// Prefix of a device id that names a program rather than an endpoint.
///
/// A program is remembered by its executable's name, never its process id: the
/// id changes every time the game starts, and the point of remembering the
/// choice is that it still means the same thing next session.
pub const PROGRAM_PREFIX: &str = "program:";

/// The device id for a program, from its executable's file name.
pub fn program_id(exe_name: &str) -> String {
    format!("{PROGRAM_PREFIX}{}", exe_name.to_ascii_lowercase())
}

/// The executable a program device id names, if it names one.
pub fn program_of(id: &str) -> Option<&str> {
    id.strip_prefix(PROGRAM_PREFIX)
        .filter(|exe| !exe.is_empty())
}

/// The game's executable, the same for Odyssey, Horizons and Legacy.
pub const ELITE_EXE: &str = "EliteDangerous64.exe";

/// What to call a program in the interface.
///
/// Elite always gets the same name, rather than whatever its version resource
/// says, so the entry reads the same whether it was found playing audio or
/// offered before the game has started. Anything else uses its own description,
/// or failing that its file name.
pub fn program_label(exe: &str, described: Option<String>) -> String {
    if exe.eq_ignore_ascii_case(ELITE_EXE) {
        return "Elite Dangerous".to_string();
    }
    described.unwrap_or_else(|| {
        exe.strip_suffix(".exe")
            .or_else(|| exe.strip_suffix(".EXE"))
            .unwrap_or(exe)
            .to_string()
    })
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
        match self.kind {
            DeviceKind::RenderLoopback => s.push_str(" [LOOPBACK]"),
            DeviceKind::ProcessLoopback => s.push_str(" [PROGRAM]"),
            DeviceKind::Capture => {}
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
///
/// A **program** chosen by name is waited for, never swapped for the default
/// output. Choosing one means "hear this and nothing else"; quietly falling back
/// to the whole output while it is not running would record exactly the sounds
/// the choice was made to shut out, with nothing on screen to say so.
pub fn select<'a>(devices: &'a [AudioDevice], id: &str) -> Option<&'a AudioDevice> {
    if program_of(id).is_some() {
        return devices
            .iter()
            .find(|d| d.kind == DeviceKind::ProcessLoopback && d.id == id);
    }
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
    // The default is the whole output, never a program: a program is only ever
    // listened to because someone chose it.
    devices
        .iter()
        .find(|d| d.kind == DeviceKind::RenderLoopback && d.is_default)
        .or_else(|| {
            devices
                .iter()
                .find(|d| d.kind == DeviceKind::RenderLoopback)
        })
}

/// What capture should open for a configured id.
///
/// A **program resolves only while its process is running**, found by process
/// rather than by the list. The list is the wrong evidence in both directions:
/// it shows programs by their audio sessions, so a game running with no output
/// to play to is missing from it — and it always offers Elite, so a game that is
/// not running at all is present in it. Trusting the list made the reconnect
/// attach, fail and retry every two seconds for as long as the game was closed,
/// two error lines and a status flicker each time. Asking for the process makes
/// the wait quiet until there is something to attach to.
pub fn resolve(devices: &[AudioDevice], id: &str) -> Option<AudioDevice> {
    if let Some(exe) = program_of(id) {
        if !program_capture_supported() {
            return None;
        }
        let (_, name) = find_program(exe)?;
        return Some(AudioDevice {
            id: id.to_string(),
            name,
            kind: DeviceKind::ProcessLoopback,
            is_default: false,
        });
    }
    select(devices, id).cloned()
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
            if program_capture_supported() {
                let mut programs = audio_programs(&enumerator);
                // Elite is always offered, running or not and playing or not.
                // Listing only programs with an audio session left it missing in
                // exactly the case this exists for: with the Bluetooth headset
                // not yet connected there is no output, so no sessions, so no
                // Elite to choose. Chosen ahead of time, it is attached by
                // process the moment it runs.
                if !programs
                    .iter()
                    .any(|p| p.exe.eq_ignore_ascii_case(super::ELITE_EXE))
                {
                    programs.insert(
                        0,
                        AudioProgram {
                            exe: super::ELITE_EXE.to_string(),
                            name: super::program_label(super::ELITE_EXE, None),
                        },
                    );
                }
                devices.extend(programs.into_iter().map(|p| AudioDevice {
                    id: super::program_id(&p.exe),
                    name: p.name,
                    kind: DeviceKind::ProcessLoopback,
                    is_default: false,
                }));
            }
            Ok(devices)
        }
    }

    /// Whether this Windows can capture one program's audio.
    ///
    /// Process loopback arrived in build 20348 (Windows Server 2022); on the
    /// desktop that means Windows 11. Earlier builds reject the activation, so
    /// programs are simply not offered there rather than offered and failing.
    pub fn program_capture_supported() -> bool {
        static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *SUPPORTED.get_or_init(|| {
            let build = windows_build();
            log::info!("Windows build {build:?}");
            build.is_some_and(|b| b >= 20348)
        })
    }

    fn windows_build() -> Option<u32> {
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
        use windows::core::HSTRING;
        let mut buf = [0u16; 64];
        let mut bytes = std::mem::size_of_val(&buf) as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                &HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
                &HSTRING::from("CurrentBuildNumber"),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        String::from_utf16_lossy(&buf[..len])
            .trim_end_matches('\0')
            .trim()
            .parse()
            .ok()
    }

    /// A process playing audio, as the picker shows it.
    pub struct AudioProgram {
        /// The executable's file name, e.g. `EliteDangerous64.exe`.
        pub exe: String,
        /// What it calls itself, e.g. "Elite Dangerous".
        pub name: String,
    }

    /// Every process with an audio session on any active output.
    ///
    /// Sessions rather than a process list: only something that has opened an
    /// audio stream can be heard, so offering every running process would mostly
    /// offer silence. Expired sessions — a program that has closed its stream —
    /// and the Windows system-sounds session are left out, and so is this
    /// process, which listening to would only ever hear itself.
    fn audio_programs(enumerator: &IMMDeviceEnumerator) -> Vec<AudioProgram> {
        use windows::Win32::Foundation::S_OK;
        use windows::Win32::Media::Audio::{
            AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2,
        };
        use windows::core::Interface;

        let own = std::process::id();
        let mut seen = std::collections::HashSet::new();
        let mut programs = Vec::new();
        let Ok(collection) =
            (unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) })
        else {
            return programs;
        };
        for i in 0..unsafe { collection.GetCount() }.unwrap_or(0) {
            let Ok(device) = (unsafe { collection.Item(i) }) else {
                continue;
            };
            let endpoint = friendly_name(&device).unwrap_or_else(|_| format!("output {i}"));
            let manager =
                match unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) } {
                    Ok(m) => m,
                    Err(e) => {
                        log::debug!("{endpoint}: no session manager ({e})");
                        continue;
                    }
                };
            let sessions = match unsafe { manager.GetSessionEnumerator() } {
                Ok(s) => s,
                Err(e) => {
                    log::debug!("{endpoint}: cannot list its sessions ({e})");
                    continue;
                }
            };
            log::debug!(
                "{endpoint}: {} audio sessions",
                unsafe { sessions.GetCount() }.unwrap_or(-1)
            );
            for j in 0..unsafe { sessions.GetCount() }.unwrap_or(0) {
                let Ok(control) = (unsafe { sessions.GetSession(j) }) else {
                    continue;
                };
                if unsafe { control.GetState() }.is_ok_and(|st| st == AudioSessionStateExpired) {
                    continue;
                }
                let Ok(control) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                if unsafe { control.IsSystemSoundsSession() } == S_OK {
                    log::debug!("  system sounds: skipped");
                    continue;
                }
                let pid = match unsafe { control.GetProcessId() } {
                    Ok(pid) => pid,
                    Err(e) => {
                        log::debug!("  a session shared by several processes: skipped ({e})");
                        continue;
                    }
                };
                if pid == 0 || pid == own || !seen.insert(pid) {
                    log::debug!("  pid {pid}: skipped (none, ours, or already seen)");
                    continue;
                }
                let Some(path) = process_path(pid) else {
                    log::debug!("  pid {pid}: its executable cannot be read");
                    continue;
                };
                log::debug!("  pid {pid}: {}", path.display());
                let Some(exe) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                let name = super::program_label(&exe, file_description(&path));
                programs.push(AudioProgram { exe, name });
            }
        }
        // One entry per program, not per process: a browser has several, and
        // capturing any one of them takes its whole process tree.
        let mut by_exe = std::collections::HashSet::new();
        programs.retain(|p| by_exe.insert(p.exe.to_ascii_lowercase()));
        programs.sort_by_key(|p| p.name.to_lowercase());
        programs
    }

    /// A running process with this executable name, playing audio or not, with
    /// the name it calls itself.
    ///
    /// Found from the process list, not from audio sessions. A session exists
    /// only while the program has an output to play to — with the Bluetooth
    /// headset not yet connected, Elite has none — and waiting for one would
    /// leave ED-Compass idle exactly when the game is already running. A capture
    /// attached by process records silence until sound flows, then follows the
    /// program's audio to whichever output it ends up on.
    pub fn find_program(exe: &str) -> Option<(u32, String)> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        let own = std::process::id();
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            while more {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if entry.th32ProcessID != own && name.eq_ignore_ascii_case(exe) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
            let pid = found?;
            let described = process_path(pid).and_then(|path| file_description(&path));
            Some((pid, super::program_label(exe, described)))
        }
    }

    fn process_path(pid: u32) -> Option<std::path::PathBuf> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        };
        use windows::core::PWSTR;
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(handle);
            ok.ok()?;
            Some(std::path::PathBuf::from(String::from_utf16_lossy(
                &buf[..len as usize],
            )))
        }
    }

    /// The executable's own name for itself, from its version resource.
    fn file_description(path: &std::path::Path) -> Option<String> {
        use std::ffi::c_void;
        use windows::Win32::Storage::FileSystem::{
            GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
        };
        use windows::core::{HSTRING, w};
        unsafe {
            let wide = HSTRING::from(path.as_os_str());
            let size = GetFileVersionInfoSizeW(&wide, None);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            GetFileVersionInfoW(&wide, None, size, data.as_mut_ptr().cast()).ok()?;

            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len = 0u32;
            if !VerQueryValueW(
                data.as_ptr().cast(),
                w!("\\VarFileInfo\\Translation"),
                &mut ptr,
                &mut len,
            )
            .as_bool()
                || len < 4
            {
                return None;
            }
            let pair = std::slice::from_raw_parts(ptr as *const u16, 2);
            let key = HSTRING::from(format!(
                "\\StringFileInfo\\{:04x}{:04x}\\FileDescription",
                pair[0], pair[1]
            ));
            if !VerQueryValueW(data.as_ptr().cast(), &key, &mut ptr, &mut len).as_bool() || len == 0
            {
                return None;
            }
            let text = std::slice::from_raw_parts(ptr as *const u16, len as usize);
            let s = String::from_utf16_lossy(text)
                .trim_end_matches('\0')
                .trim()
                .to_string();
            (!s.is_empty()).then_some(s)
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
pub use imp::{
    ensure_com, ensure_com_mta, enumerate, find_program, open, program_capture_supported,
};

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

    pub fn program_capture_supported() -> bool {
        false
    }

    pub fn find_program(_exe: &str) -> Option<(u32, String)> {
        None
    }
}

#[cfg(not(windows))]
pub use imp::{ensure_com, ensure_com_mta, enumerate, find_program, program_capture_supported};

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

    fn elite() -> AudioDevice {
        AudioDevice {
            id: program_id("EliteDangerous64.exe"),
            name: "Elite Dangerous".into(),
            kind: DeviceKind::ProcessLoopback,
            is_default: false,
        }
    }

    #[test]
    fn a_program_is_remembered_by_its_executable_not_its_process() {
        let id = program_id("EliteDangerous64.exe");
        assert_eq!(
            id, "program:elitedangerous64.exe",
            "case-insensitive, as Windows file names are"
        );
        assert_eq!(program_of(&id), Some("elitedangerous64.exe"));
        assert_eq!(
            program_of("{0.0.0.00000000}.{4b362600}"),
            None,
            "an endpoint id is not a program"
        );
        assert_eq!(
            program_of("program:"),
            None,
            "and an empty name is not one either"
        );
        assert_eq!(elite().display_name(), "Elite Dangerous [PROGRAM]");
    }

    #[test]
    fn elite_has_one_name_whether_found_playing_or_offered_in_advance() {
        // Offered before the game starts it has no version resource to read, so
        // naming it from that resource when it is found would make the same
        // entry change its name the moment Elite starts.
        assert_eq!(program_label(ELITE_EXE, None), "Elite Dangerous");
        assert_eq!(
            program_label("elitedangerous64.exe", Some("EliteDangerous64".into())),
            "Elite Dangerous"
        );
        assert_eq!(
            program_label("Discord.exe", Some("Discord".into())),
            "Discord"
        );
        assert_eq!(
            program_label("obs64.exe", None),
            "obs64",
            "a file name without its extension"
        );
    }

    #[test]
    fn a_chosen_program_is_waited_for_never_swapped_for_the_whole_output() {
        // Choosing a program means "hear this and nothing else". Falling back to
        // the default output while it is not running would record exactly the
        // sounds the choice was made to exclude.
        let mut d = devices();
        let id = program_id("EliteDangerous64.exe");
        assert_eq!(select(&d, &id), None, "not running: wait, do not fall back");
        d.push(elite());
        assert_eq!(
            select(&d, &id).map(|x| x.name.as_str()),
            Some("Elite Dangerous")
        );
    }

    #[test]
    fn the_default_is_never_a_program() {
        // A program is only ever listened to because someone chose it, even when
        // it is listed first and no output is marked default.
        let mut d = vec![elite()];
        d.extend(devices().into_iter().map(|mut x| {
            x.is_default = false;
            x
        }));
        let picked = select(&d, "").unwrap();
        assert_eq!(picked.kind, DeviceKind::RenderLoopback, "picked {picked:?}");
        assert_eq!(
            select(&d, "unplugged").unwrap().kind,
            DeviceKind::RenderLoopback
        );
    }

    #[test]
    fn a_listed_program_that_is_not_running_does_not_resolve() {
        // Elite is always in the list, running or not. Resolving it from the
        // list made the reconnect attach to a process that did not exist, fail,
        // and try again every two seconds while the game was closed.
        let mut d = devices();
        d.push(elite());
        assert!(
            select(&d, &program_id(ELITE_EXE)).is_some(),
            "it is in the list"
        );
        if find_program(ELITE_EXE).is_none() {
            assert_eq!(
                resolve(&d, &program_id(ELITE_EXE)),
                None,
                "but with no process there is nothing to attach to"
            );
        }
    }

    #[test]
    fn an_unrunning_program_resolves_to_nothing_where_programs_are_unsupported() {
        // Off Windows 11 there is no process to find, and no fallback either.
        if !program_capture_supported() {
            assert_eq!(
                resolve(&devices(), &program_id("EliteDangerous64.exe")),
                None
            );
        }
        assert_eq!(
            resolve(&devices(), "hdmi").map(|x| x.id),
            Some("hdmi".to_string())
        );
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

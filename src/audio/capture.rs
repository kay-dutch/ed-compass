//! WASAPI capture, including loopback for system audio.
//!
//! Runs on its own thread and does the minimum: negotiate the format, pull
//! packets, convert to `f32`, and send. No analysis happens here.
//!
//! Two distinct things can break the timeline, and they need distinct handling:
//!
//! * **A discontinuity while packets are flowing** — the device dropped data.
//!   Detected from the stream position reported alongside each packet, which
//!   gives an exact frame count for the hole.
//! * **An idle loopback endpoint** — nothing is playing, so WASAPI delivers no
//!   packets at all and the stream position simply stops advancing. Detected
//!   from wall-clock silence between packets.
//!
//! Both are reported as `Gap`, because downstream the distinction does not
//! matter: what matters is that the analysis clock keeps advancing, so a
//! 109.5-second period stays measurable across a quiet stretch.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use crossbeam_channel::Sender;

use crate::audio::StreamFormat;

/// How long an idle endpoint must stay quiet before we start synthesizing
/// silence to keep the timeline advancing.
#[cfg(windows)]
const IDLE_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(250);

/// Wait timeout, which also bounds how long `stop()` takes to take effect.
#[cfg(windows)]
const WAIT_TIMEOUT_MS: u32 = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum CaptureMessage {
    /// Sent once when the stream opens, and again if the format changes.
    Format(StreamFormat),
    /// Interleaved `f32` frames.
    Audio(Vec<f32>),
    /// The timeline advanced without audio. `idle` distinguishes "nothing was
    /// playing" from "the device dropped data".
    Gap {
        frames: usize,
        idle: bool,
    },
    /// Capture has stopped and will send nothing further.
    Error(String),
    Stopped,
}

/// Owns the capture thread. Dropping it stops capture.
#[derive(Debug)]
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureHandle {
    /// A handle with nothing behind it, for when there is no device to capture
    /// from yet. Lets the application start and show why rather than refusing to
    /// open at all.
    pub fn idle() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
            thread: None,
        }
    }

    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Relaxed)
    }

    /// Signal the thread and wait for it. Takes up to `WAIT_TIMEOUT_MS`.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{Context, Result, bail};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
        AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync,
        IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
        IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
        IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
    };
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        BLOB, CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IAgileObject, IAgileObject_Impl,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject,
    };
    use windows::Win32::System::Variant::VT_BLOB;
    use windows::core::{GUID, HRESULT, IUnknown, Interface, implement};

    use crate::audio::SampleFormat;
    use crate::audio::device::{self, AudioDevice, DeviceKind};
    use crate::audio::format::{MASK_STEREO, classify, convert_to_f32};

    // Defined locally rather than pulled from a `windows` feature, so this file
    // does not depend on which crate version moved them where.
    const WAVE_FORMAT_PCM: u16 = 1;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
        GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

    /// Read a `WAVEFORMATEX` (possibly extensible) into our own descriptor.
    ///
    /// # Safety
    /// `wfx` must point at a valid `WAVEFORMATEX`, and at a full
    /// `WAVEFORMATEXTENSIBLE` when its tag says so.
    unsafe fn describe_format(wfx: *const WAVEFORMATEX) -> Result<StreamFormat> {
        let base = unsafe { &*wfx };
        let channels = base.nChannels as usize;
        if channels == 0 {
            bail!("endpoint reported zero channels");
        }
        let container = base.nBlockAlign as usize / channels;

        let (is_float, mask) = if base.wFormatTag == WAVE_FORMAT_EXTENSIBLE {
            let ext = unsafe { &*(wfx as *const WAVEFORMATEXTENSIBLE) };
            let sub = ext.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                (true, ext.dwChannelMask)
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                (false, ext.dwChannelMask)
            } else {
                bail!("endpoint uses an unsupported extensible subformat {sub:?}");
            }
        } else {
            match base.wFormatTag {
                WAVE_FORMAT_IEEE_FLOAT => (true, 0),
                WAVE_FORMAT_PCM => (false, 0),
                tag => bail!("endpoint uses an unsupported format tag {tag}"),
            }
        };

        let sample_format = classify(container, is_float).with_context(|| {
            format!(
                "endpoint uses an unsupported sample layout: {} bytes per sample, float={is_float}",
                container
            )
        })?;

        Ok(StreamFormat::new(
            base.nSamplesPerSec,
            channels,
            mask,
            sample_format,
        ))
    }

    struct Stream {
        client: IAudioClient,
        capture: IAudioCaptureClient,
        event: Option<HANDLE>,
        format: StreamFormat,
        bytes_per_frame: usize,
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
                if let Some(h) = self.event.take() {
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    /// Open a capture stream, preferring event-driven mode and falling back to
    /// polling. Loopback plus `EVENTCALLBACK` is not honoured by every driver,
    /// and an `IAudioClient` cannot be re-initialized after a failure, so the
    /// fallback activates a fresh client.
    unsafe fn open_stream(device: &IMMDevice, loopback: bool) -> Result<Stream> {
        let mut last_error = None;
        for use_event in [true, false] {
            match unsafe { try_open(device, loopback, use_event) } {
                Ok(stream) => {
                    if !use_event {
                        log::warn!(
                            "endpoint refused event-driven capture; falling back to polling"
                        );
                    }
                    return Ok(stream);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not open the endpoint")))
    }

    unsafe fn try_open(device: &IMMDevice, loopback: bool, use_event: bool) -> Result<Stream> {
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.context("activating the audio client")?;

        let wfx = unsafe { client.GetMixFormat() }.context("reading the endpoint mix format")?;
        let format = unsafe { describe_format(wfx) };

        let mut flags = 0u32;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        if use_event {
            flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        }

        // 0 lets the audio engine choose its own period, which is what shared
        // mode wants.
        let init = unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, wfx, None) };
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _)) };
        init.context("initializing the audio client")?;
        let format = format?;

        let event = if use_event {
            let handle = unsafe { CreateEventW(None, false, false, None) }
                .context("creating a wait event")?;
            unsafe { client.SetEventHandle(handle) }.context("attaching the wait event")?;
            Some(handle)
        } else {
            None
        };

        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("acquiring the capture service")?;
        unsafe { client.Start() }.context("starting the stream")?;

        let bytes_per_frame = format.sample_format.bytes_per_sample() * format.channels;
        Ok(Stream {
            client,
            capture,
            event,
            format,
            bytes_per_frame,
        })
    }

    pub fn start(device: &AudioDevice, tx: Sender<CaptureMessage>) -> Result<CaptureHandle> {
        let stop = Arc::new(AtomicBool::new(false));
        let id = device.id.clone();
        let name = device.name.clone();
        let loopback = device.kind.is_loopback();
        let program = match device.kind {
            DeviceKind::ProcessLoopback => Some(
                device::program_of(&device.id)
                    .ok_or_else(|| anyhow::anyhow!("{} names no program", device.id))?
                    .to_string(),
            ),
            _ => None,
        };
        let thread_stop = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("wasapi-capture".into())
            .spawn(move || {
                // The capture thread wants a multi-threaded apartment; the UI
                // thread must stay single-threaded so window creation works.
                device::ensure_com_mta();
                let result = match &program {
                    Some(exe) => run_program(exe, &name, &tx, &thread_stop),
                    None => run(&id, loopback, &tx, &thread_stop),
                };
                if let Err(e) = result {
                    log::error!("capture on {name} failed: {e:#}");
                    let _ = tx.send(CaptureMessage::Error(format!("{e:#}")));
                }
                let _ = tx.send(CaptureMessage::Stopped);
                thread_stop.store(true, Ordering::Relaxed);
            })
            .context("spawning the capture thread")?;

        Ok(CaptureHandle {
            stop,
            thread: Some(thread),
        })
    }

    fn run(id: &str, loopback: bool, tx: &Sender<CaptureMessage>, stop: &AtomicBool) -> Result<()> {
        let device = device::open(id)?;
        let stream = unsafe { open_stream(&device, loopback) }?;
        log::info!(
            "capture started on {id} ({}) — {}, {} mode, ring frame size {} bytes",
            if loopback { "loopback" } else { "input" },
            stream.format.describe(),
            if stream.event.is_some() {
                "event"
            } else {
                "polling"
            },
            stream.bytes_per_frame,
        );
        pump(&stream, id, true, tx, stop, || Ok(()))
    }

    /// Capture one program's audio, and nothing else's.
    fn run_program(
        exe: &str,
        name: &str,
        tx: &Sender<CaptureMessage>,
        stop: &AtomicBool,
    ) -> Result<()> {
        let (pid, _) = device::find_program(exe).ok_or_else(|| {
            anyhow::anyhow!("{name} is not running — it is picked up as soon as it starts")
        })?;
        let process = Watched::open(pid)?;
        // The rate the output mixes at, so the engine's conversion is a format
        // change and not a resample. With no output at all — the headset not yet
        // connected — 48 kHz is what Elite renders.
        let rate = default_mix_rate().unwrap_or(48_000);
        let stream = unsafe { open_program_stream(pid, rate) }?;
        log::info!(
            "capture started on {name} (process {pid}, program loopback) — {}, {} mode",
            stream.format.describe(),
            if stream.event.is_some() {
                "event"
            } else {
                "polling"
            },
        );
        // A program's stream has no device clock behind it, so its packet
        // positions are not evidence of dropped audio and are not used to fill
        // gaps. Idle stretches are still filled from wall time.
        pump(&stream, name, false, tx, stop, || {
            if process.exited() {
                bail!("{name} has closed");
            }
            Ok(())
        })
    }

    /// Drain a capture stream until told to stop, or until `alive` reports
    /// that the source has gone.
    fn pump(
        stream: &Stream,
        what: &str,
        trust_position: bool,
        tx: &Sender<CaptureMessage>,
        stop: &AtomicBool,
        mut alive: impl FnMut() -> Result<()>,
    ) -> Result<()> {
        /// How often a program is checked for having exited. Cheap, but there
        /// is no reason to ask a hundred times a second.
        const ALIVE_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
        let format = stream.format.clone();
        let sample_rate = format.sample_rate as f64;
        let mut last_alive_check = std::time::Instant::now();

        if tx.send(CaptureMessage::Format(format.clone())).is_err() {
            return Ok(()); // nobody is listening any more
        }

        // Position of the frame we expect next, in the device's own stream
        // clock. Used only to size discontinuities.
        let mut expected_position: Option<u64> = None;
        let mut last_packet_at = std::time::Instant::now();
        let mut samples = Vec::new();
        let mut discontinuities = 0u64;
        let mut idle_gaps = 0u64;

        while !stop.load(Ordering::Relaxed) {
            if last_alive_check.elapsed() >= ALIVE_EVERY {
                alive()?;
                last_alive_check = std::time::Instant::now();
            }
            unsafe {
                match stream.event {
                    Some(handle) => {
                        let r = WaitForSingleObject(handle, WAIT_TIMEOUT_MS);
                        if r != WAIT_OBJECT_0 && r.0 != 0x102 {
                            // Neither signalled nor timed out: the handle is bad.
                            bail!("waiting on the capture event failed ({r:?})");
                        }
                    }
                    None => std::thread::sleep(std::time::Duration::from_millis(10)),
                }
            }

            let mut got_audio = false;
            // Bounded, so a misbehaving endpoint cannot hold this thread inside
            // the drain loop indefinitely.
            //
            // The exit condition is the endpoint reporting no more packets, and
            // that trusts the endpoint to eventually say so. A virtual or
            // half-removed device that keeps claiming data is available spins
            // here at full speed with the outer wait never reached — one core
            // pinned, in a thread running at audio priority, which is enough to
            // make a whole machine feel locked up. Shared-mode packets are ~10 ms,
            // so this ceiling is around two seconds of audio in one pass: far
            // more than a healthy device ever queues, far less than forever.
            const MAX_PACKETS_PER_WAKE: usize = 200;
            let mut drained = 0usize;
            loop {
                if drained >= MAX_PACKETS_PER_WAKE {
                    log::warn!(
                        "endpoint queued more than {MAX_PACKETS_PER_WAKE} packets in one wake; \
                         yielding to avoid spinning"
                    );
                    break;
                }
                drained += 1;
                let available = unsafe { stream.capture.GetNextPacketSize() }
                    .context("querying the next packet size")?;
                if available == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                let mut position: u64 = 0;
                unsafe {
                    stream
                        .capture
                        .GetBuffer(
                            &mut data,
                            &mut frames,
                            &mut flags,
                            Some(&mut position),
                            None,
                        )
                        .context("reading a capture packet")?;
                }

                if frames > 0 {
                    // A hole inside a flowing stream: fill it exactly.
                    if trust_position
                        && let Some(expected) = expected_position
                        && position > expected
                    {
                        let missing = (position - expected) as usize;
                        discontinuities += 1;
                        log::warn!(
                            "device discontinuity: {missing} frames ({:.3} s) missing",
                            missing as f64 / sample_rate
                        );
                        let _ = tx.send(CaptureMessage::Gap {
                            frames: missing,
                            idle: false,
                        });
                    }
                    expected_position = Some(position + frames as u64);

                    samples.clear();
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                        // The buffer contents are undefined when this is set;
                        // reusing them would inject noise.
                        samples.resize(frames as usize * format.channels, 0.0);
                    } else {
                        let bytes = frames as usize * stream.bytes_per_frame;
                        let raw = unsafe { std::slice::from_raw_parts(data, bytes) };
                        convert_to_f32(raw, format.sample_format, &mut samples);
                    }
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        log::debug!("packet flagged as discontinuous");
                    }
                    got_audio = true;
                }

                unsafe {
                    stream
                        .capture
                        .ReleaseBuffer(frames)
                        .context("releasing a capture packet")?;
                }

                if frames > 0 && tx.send(CaptureMessage::Audio(samples.clone())).is_err() {
                    return Ok(());
                }
            }

            let now = std::time::Instant::now();
            if got_audio {
                last_packet_at = now;
            } else {
                // An idle loopback endpoint delivers nothing at all. Advance the
                // timeline ourselves so a quiet stretch does not silently splice
                // the audio either side of it together.
                let quiet = now.duration_since(last_packet_at);
                if quiet >= IDLE_THRESHOLD {
                    let frames = (quiet.as_secs_f64() * sample_rate) as usize;
                    if frames > 0 {
                        idle_gaps += 1;
                        if tx.send(CaptureMessage::Gap { frames, idle: true }).is_err() {
                            return Ok(());
                        }
                        last_packet_at = now;
                        // The device clock also stopped, so do not treat the
                        // jump as a dropout when packets resume.
                        expected_position = None;
                    }
                }
            }
        }

        log::info!(
            "capture stopped on {what} — {discontinuities} discontinuities, {idle_gaps} idle gaps"
        );
        Ok(())
    }

    /// A process handle that can say whether the process has exited.
    struct Watched(HANDLE);

    impl Watched {
        fn open(pid: u32) -> Result<Self> {
            let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }
                .context("watching the program for exit")?;
            Ok(Self(handle))
        }

        fn exited(&self) -> bool {
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
        }
    }

    impl Drop for Watched {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    /// The default output's mix rate, if there is a default output at all.
    fn default_mix_rate() -> Option<u32> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            let wfx = client.GetMixFormat().ok()?;
            let rate = (*wfx).nSamplesPerSec;
            CoTaskMemFree(Some(wfx as *const _));
            Some(rate)
        }
    }

    /// Open a program capture, preferring event-driven mode, as [`open_stream`]
    /// does for an endpoint and for the same reason: a client that failed to
    /// initialize cannot be retried, so the fallback activates a fresh one.
    unsafe fn open_program_stream(pid: u32, rate: u32) -> Result<Stream> {
        let mut last_error = None;
        for use_event in [true, false] {
            match unsafe { try_open_program(pid, rate, use_event) } {
                Ok(stream) => {
                    if !use_event {
                        log::warn!("program capture refused event-driven mode; polling");
                    }
                    return Ok(stream);
                }
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not open the program capture")))
    }

    /// Stereo float at `rate`. A program capture has no mix format of its own to
    /// ask for (`GetMixFormat` is not implemented for it), so the format is
    /// stated and the engine converts to it. Stereo because that is what every
    /// program can be converted to; a surround mix is folded down, so the
    /// bearing in this mode is the stereo estimate.
    unsafe fn try_open_program(pid: u32, rate: u32, use_event: bool) -> Result<Stream> {
        let client = unsafe { activate_program(pid) }?;
        let channels: u16 = 2;
        let block = channels * 4;
        let wfx = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let mut flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if use_event {
            flags |= AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        }
        // 200 ms of buffer. There is no device period to defer to, and the drain
        // loop empties it every wake.
        unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &wfx, None) }
            .context("initializing the program capture")?;

        let event = if use_event {
            let handle = unsafe { CreateEventW(None, false, false, None) }
                .context("creating a wait event")?;
            unsafe { client.SetEventHandle(handle) }.context("attaching the wait event")?;
            Some(handle)
        } else {
            None
        };
        let capture: IAudioCaptureClient =
            unsafe { client.GetService() }.context("acquiring the capture service")?;
        unsafe { client.Start() }.context("starting the program capture")?;
        Ok(Stream {
            client,
            capture,
            event,
            format: StreamFormat::new(rate, channels as usize, MASK_STEREO, SampleFormat::F32),
            bytes_per_frame: block as usize,
        })
    }

    /// Signals an event when the asynchronous activation finishes.
    ///
    /// Agile, because the activation calls back on a thread of its own choosing
    /// and refuses a handler that cannot be called from any apartment.
    #[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
    struct Activation {
        done: HANDLE,
    }

    impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
        fn ActivateCompleted(
            &self,
            _operation: windows_core::Ref<IActivateAudioInterfaceAsyncOperation>,
        ) -> windows_core::Result<()> {
            unsafe {
                let _ = SetEvent(self.done);
            }
            Ok(())
        }
    }

    impl IAgileObject_Impl for Activation_Impl {}

    /// An audio client that hears only `pid` and the processes it started.
    unsafe fn activate_program(pid: u32) -> Result<IAudioClient> {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // The parameters travel as a blob inside a PROPVARIANT, pointing at
        // `params` on the stack — so the PROPVARIANT must never be dropped.
        //
        // windows-rs gives `PROPVARIANT` a destructor that calls
        // `PropVariantClear`, defined in its hand-written extensions rather than
        // beside the generated struct, where it is easy to miss. For a blob that
        // frees `pBlobData` with the COM allocator: here, a stack address. It
        // corrupted the heap and killed the process (0xc0000374) the moment a
        // program was chosen. `ManuallyDrop` keeps the destructor from running;
        // there is nothing to leak, since nothing here was allocated.
        let mut variant = std::mem::ManuallyDrop::new(PROPVARIANT::default());
        unsafe {
            let inner = &mut *variant.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob = BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: &params as *const _ as *mut u8,
            };
        }

        let done = unsafe { CreateEventW(None, true, false, None) }
            .context("creating the activation event")?;
        let result = (|| -> Result<IAudioClient> {
            let handler: IActivateAudioInterfaceCompletionHandler = Activation { done }.into();
            let operation = unsafe {
                ActivateAudioInterfaceAsync(
                    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                    &IAudioClient::IID,
                    Some(&*variant),
                    &handler,
                )
            }
            .context("requesting program capture")?;
            if unsafe { WaitForSingleObject(done, 5_000) } != WAIT_OBJECT_0 {
                bail!("program capture did not activate within 5 s");
            }
            let mut status = HRESULT(0);
            let mut activated: Option<IUnknown> = None;
            unsafe { operation.GetActivateResult(&mut status, &mut activated) }
                .context("reading the activation result")?;
            status.ok().context("program capture was refused")?;
            activated
                .ok_or_else(|| anyhow::anyhow!("program capture returned no client"))?
                .cast::<IAudioClient>()
                .context("program capture returned no audio client")
        })();
        unsafe {
            let _ = CloseHandle(done);
        }
        // Keep the blob's target alive until the activation is finished with it.
        let _ = &params;
        result
    }
}

#[cfg(windows)]
pub use imp::start;

#[cfg(not(windows))]
pub fn start(
    _device: &crate::audio::device::AudioDevice,
    _tx: Sender<CaptureMessage>,
) -> anyhow::Result<CaptureHandle> {
    anyhow::bail!("live capture requires Windows; use --test-landscape or --input on this platform")
}

/// Drive the pipeline from a synthetic source using the same message protocol
/// as live capture, so the rest of the application cannot tell the difference.
pub fn start_synthetic(
    mut source: crate::audio::synthetic::SyntheticSource,
    tx: Sender<CaptureMessage>,
) -> CaptureHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);

    let thread = std::thread::Builder::new()
        .name("synthetic-capture".into())
        .spawn(move || {
            let format = source.format().clone();
            if tx.send(CaptureMessage::Format(format.clone())).is_err() {
                return;
            }
            // 50 ms blocks, roughly what a shared-mode endpoint delivers.
            let block = (format.sample_rate as f32 * 0.05) as usize;
            let period = std::time::Duration::from_millis(50);
            let mut next = std::time::Instant::now();
            let mut buf = Vec::new();

            while !thread_stop.load(Ordering::Relaxed) {
                buf.clear();
                source.render(block, &mut buf);
                if tx.send(CaptureMessage::Audio(buf.clone())).is_err() {
                    break;
                }
                next += period;
                let now = std::time::Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    // Fell behind; resynchronize rather than spiral.
                    next = now;
                }
            }
            let _ = tx.send(CaptureMessage::Stopped);
            thread_stop.store(true, Ordering::Relaxed);
        })
        .expect("spawning the synthetic capture thread");

    CaptureHandle {
        stop,
        thread: Some(thread),
    }
}

/// Drive the pipeline from a decoded file.
///
/// `realtime` paces playback at wall-clock speed, which is what the GUI wants.
/// Headless offline analysis leaves it off and runs as fast as the CPU allows.
pub fn start_file(
    mut source: crate::audio::file_input::FileSource,
    tx: Sender<CaptureMessage>,
    realtime: bool,
) -> CaptureHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);

    let thread = std::thread::Builder::new()
        .name("file-capture".into())
        .spawn(move || {
            let format = source.format().clone();
            if tx.send(CaptureMessage::Format(format.clone())).is_err() {
                return;
            }
            let block = (format.sample_rate as f32 * 0.05).max(1.0) as usize;
            let period = std::time::Duration::from_millis(50);
            let mut next = std::time::Instant::now();
            let mut buf = Vec::new();

            while !thread_stop.load(Ordering::Relaxed) {
                buf.clear();
                if source.render(block, &mut buf) == 0 {
                    break; // end of a non-looping file
                }
                if tx.send(CaptureMessage::Audio(buf.clone())).is_err() {
                    break;
                }
                if realtime {
                    next += period;
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            }
            let _ = tx.send(CaptureMessage::Stopped);
            thread_stop.store(true, Ordering::Relaxed);
        })
        .expect("spawning the file capture thread");

    CaptureHandle {
        stop,
        thread: Some(thread),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SampleFormat;
    use crate::audio::format::MASK_STEREO;
    use crate::audio::synthetic::{SyntheticSource, TestSignal};

    fn format() -> StreamFormat {
        StreamFormat::new(8_000, 2, MASK_STEREO, SampleFormat::F32)
    }

    #[test]
    fn synthetic_capture_announces_its_format_first() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let handle = start_synthetic(
            SyntheticSource::new(TestSignal::Sine { hz: 440.0 }, format(), 0.0),
            tx,
        );
        match rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap() {
            CaptureMessage::Format(f) => assert_eq!(f, format()),
            other => panic!("expected the format first, got {other:?}"),
        }
        handle.stop();
    }

    #[test]
    fn synthetic_capture_delivers_audio_then_stops_cleanly() {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let handle = start_synthetic(SyntheticSource::new(TestSignal::Noise, format(), 0.0), tx);

        let mut audio_blocks = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while audio_blocks < 3 && std::time::Instant::now() < deadline {
            if let Ok(CaptureMessage::Audio(block)) =
                rx.recv_timeout(std::time::Duration::from_millis(500))
            {
                assert_eq!(block.len() % 2, 0, "blocks must be whole frames");
                assert!(!block.is_empty());
                audio_blocks += 1;
            }
        }
        assert!(audio_blocks >= 3, "only received {audio_blocks} blocks");

        assert!(handle.is_running());
        handle.stop();

        // Drain to the terminator.
        let mut saw_stopped = false;
        while let Ok(msg) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
            if msg == CaptureMessage::Stopped {
                saw_stopped = true;
                break;
            }
        }
        assert!(saw_stopped, "capture must signal that it has finished");
    }

    #[test]
    fn dropping_the_handle_stops_capture() {
        let (tx, rx) = crossbeam_channel::bounded(8);
        {
            let _handle =
                start_synthetic(SyntheticSource::new(TestSignal::Silence, format(), 0.0), tx);
            let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
        } // dropped here, which joins the thread

        // A bounded channel with a stalled reader must not wedge shutdown.
        while rx.try_recv().is_ok() {}
        assert!(rx.try_recv().is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn live_capture_is_refused_with_a_useful_message_off_windows() {
        let (tx, _rx) = crossbeam_channel::bounded(1);
        let device = crate::audio::device::AudioDevice {
            id: "x".into(),
            name: "x".into(),
            kind: crate::audio::device::DeviceKind::RenderLoopback,
            is_default: true,
        };
        let err = start(&device, tx).unwrap_err().to_string();
        assert!(err.contains("--test-landscape"), "unhelpful message: {err}");
    }
}

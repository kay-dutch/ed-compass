//! Checking for, and installing, a newer release.
//!
//! Modelled on the ArtCraft launcher's self-update, which solved the parts that
//! are easy to get wrong:
//!
//! * **Only an offer.** A check runs at startup and the banner says a version is
//!   available; nothing is downloaded until the button is pressed. An update that
//!   installed itself would close the application in the middle of a session.
//! * **Verified before it runs.** The installer is checked against the SHA-256
//!   GitHub publishes for each release asset, or failing that the release's own
//!   `SHA256SUMS.txt`. A release offering neither is refused rather than trusted.
//! * **Swapped by a helper, after exit.** Windows locks a running exe, and this
//!   process also holds the single-instance lock, so the new copy cannot be put in
//!   place while it runs. A hidden helper waits for this process to exit, runs the
//!   installer, and starts the new version.
//! * **Only an installed copy updates itself.** Anything else — the portable zip,
//!   or a build run from `target\release` — is offered the download page instead,
//!   so a development build can never be overwritten by a release.
//! * **No update loops.** The version just installed is recorded, and not offered
//!   again for a day, in case a release's binary reports a different version from
//!   its tag.
//!
//! [`PRETEND_VAR`] makes this copy believe it is an older version, so the whole
//! path can be exercised against the current release.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// Makes this copy report a different version, to try an update against the
/// current release. The helper drops it, so it lasts one run.
pub const PRETEND_VAR: &str = "ED_COMPASS_PRETEND_VERSION";

/// How long a just-installed version is exempt from being offered again.
const JUST_UPDATED_FOR: Duration = Duration::from_secs(24 * 3600);

/// How often a long session asks again.
const RECHECK_EVERY: Duration = Duration::from_secs(24 * 3600);

/// Largest answer accepted from the releases API, and largest installer.
/// Generous bounds that only exist so a misbehaving server cannot fill the disk.
const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;
const MAX_INSTALLER_BYTES: u64 = 256 * 1024 * 1024;

/// A release version: major, minor, patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parse `v0.5.0`, `0.5.0` or `0.5`. A pre-release suffix is ignored, since
/// GitHub's "latest release" never points at a pre-release.
pub fn parse_version(s: &str) -> Option<Version> {
    let core = s
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Version(major, minor, patch))
}

/// The version this copy counts as (see [`PRETEND_VAR`]).
pub fn current_version() -> Version {
    std::env::var(PRETEND_VAR)
        .ok()
        .and_then(|v| parse_version(&v))
        .or_else(|| parse_version(env!("CARGO_PKG_VERSION")))
        .expect("the crate version is a valid version")
}

/// One published release.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
    /// The release page on GitHub, for "What's new" and manual downloads.
    pub page: String,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Lower-case hex, when GitHub publishes a digest for the file.
    pub sha256: Option<String>,
}

impl Release {
    /// The installer, preferring the one named for this exact version.
    pub fn installer(&self) -> Option<&Asset> {
        let exact = format!("ed-compass-setup-{}.exe", self.version).to_ascii_lowercase();
        self.assets
            .iter()
            .find(|a| a.name.to_ascii_lowercase() == exact)
            .or_else(|| {
                self.assets.iter().find(|a| {
                    let n = a.name.to_ascii_lowercase();
                    n.starts_with("ed-compass-setup-") && n.ends_with(".exe")
                })
            })
    }

    fn checksums(&self) -> Option<&Asset> {
        self.assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case("SHA256SUMS.txt"))
    }
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// `sha256:<hex>`, published by GitHub for each asset.
    #[serde(default)]
    digest: Option<String>,
}

/// Read the answer from `GET /repos/{owner}/{repo}/releases/latest`.
pub fn parse_release(json: &str) -> Result<Release> {
    let api: ApiRelease = serde_json::from_str(json).context("reading the release")?;
    if api.draft || api.prerelease {
        bail!("{} is not a final release", api.tag_name);
    }
    let version = parse_version(&api.tag_name)
        .ok_or_else(|| anyhow!("{} is not a version number", api.tag_name))?;
    Ok(Release {
        tag: api.tag_name,
        version,
        page: api.html_url,
        assets: api
            .assets
            .into_iter()
            .map(|a| Asset {
                sha256: a
                    .digest
                    .as_deref()
                    .and_then(|d| d.strip_prefix("sha256:"))
                    .and_then(valid_sha256),
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
            })
            .collect(),
    })
}

/// `hash  filename` lines, as the release workflow writes them.
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.trim().splitn(2, char::is_whitespace);
            let hash = valid_sha256(parts.next()?)?;
            let name = parts.next()?.trim().trim_start_matches('*').to_string();
            (!name.is_empty()).then_some((hash, name))
        })
        .collect()
}

fn valid_sha256(hex: &str) -> Option<String> {
    let hex = hex.trim();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// Where the releases are published, from the crate's own metadata so a renamed
/// repository is followed by changing one line.
fn latest_release_url() -> String {
    let repo = env!("CARGO_PKG_REPOSITORY")
        .trim_end_matches('/')
        .trim_start_matches("https://github.com/");
    format!("https://api.github.com/repos/{repo}/releases/latest")
}

/// How this copy was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// By the installer, so it can update itself in place. Holds the exe.
    Installed(PathBuf),
    /// The portable zip, or a development build: offered the download page.
    Manual,
}

/// Recognise an installed copy by the folder the installer recorded for it.
pub fn detect_install() -> Install {
    let Ok(exe) = std::env::current_exe() else {
        return Install::Manual;
    };
    let Some(dir) = exe.parent() else {
        return Install::Manual;
    };
    match platform::installed_dir() {
        Some(recorded) if same_dir(&recorded, dir) => Install::Installed(exe),
        _ => Install::Manual,
    }
}

/// The folder the installer recorded, if ED-Compass has ever been installed.
pub fn recorded_install_dir() -> Option<PathBuf> {
    platform::installed_dir()
}

/// Windows paths compare without regard to case or a trailing separator.
fn same_dir(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let p = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .trim_start_matches(r"\\?\")
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

/// The marker recording which version was just installed, and when.
fn marker(data_dir: &Path) -> PathBuf {
    data_dir.join("update.txt")
}

fn record_update(data_dir: &Path, tag: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Err(e) = std::fs::write(marker(data_dir), format!("{tag}\n{now}\n")) {
        log::warn!("could not record the update: {e}");
    }
}

/// Whether `tag` is the version this copy just updated itself to.
fn just_updated_to(data_dir: &Path, tag: &str, now_secs: u64) -> bool {
    let Ok(text) = std::fs::read_to_string(marker(data_dir)) else {
        return false;
    };
    let mut lines = text.lines();
    let (Some(recorded), Some(at)) = (
        lines.next(),
        lines.next().and_then(|a| a.parse::<u64>().ok()),
    ) else {
        return false;
    };
    recorded == tag
        && now_secs
            .checked_sub(at)
            .is_some_and(|age| age < JUST_UPDATED_FOR.as_secs())
}

/// Whether `release` should be offered to a copy at version `current`.
pub fn worth_offering(release: &Release, current: Version, data_dir: &Path, now_secs: u64) -> bool {
    release.version > current && !just_updated_to(data_dir, &release.tag, now_secs)
}

/// Where an update stands, for the banner.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Downloading { done: u64, total: u64 },
    Verifying,
    Installing,
}

#[derive(Default)]
struct Shared {
    offer: Option<Release>,
    phase: Option<Phase>,
    error: Option<String>,
    /// The helper is scheduled: this process must exit for it to proceed.
    exit_now: bool,
    checking: bool,
}

/// The update state the interface reads every frame.
///
/// The work happens on background threads; the interface only ever polls, which
/// suits a window that repaints continuously anyway.
pub struct Updater {
    shared: Arc<Mutex<Shared>>,
    install: Install,
    data_dir: PathBuf,
    enabled: bool,
    last_check: Option<Instant>,
    /// A version the pilot dismissed. A newer one is offered again.
    dismissed: Option<Version>,
}

impl Updater {
    pub fn new(data_dir: PathBuf, enabled: bool) -> Self {
        Self {
            shared: Arc::default(),
            install: detect_install(),
            data_dir,
            enabled,
            last_check: None,
            dismissed: None,
        }
    }

    /// Start a check when one is due. Cheap; call every frame.
    pub fn tick(&mut self) {
        if !self.enabled || self.lock().checking || self.lock().phase.is_some() {
            return;
        }
        if self.last_check.is_some_and(|t| t.elapsed() < RECHECK_EVERY) {
            return;
        }
        self.last_check = Some(Instant::now());
        self.lock().checking = true;
        let shared = Arc::clone(&self.shared);
        let data_dir = self.data_dir.clone();
        let spawned = std::thread::Builder::new()
            .name("update-check".into())
            .spawn(move || {
                let found = check(&data_dir);
                let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
                s.checking = false;
                match found {
                    Ok(Some(release)) => {
                        log::info!("ED-Compass {} is available", release.version);
                        s.offer = Some(release);
                    }
                    Ok(None) => log::info!("ED-Compass is up to date"),
                    // Offline is ordinary, and not worth a banner.
                    Err(e) => log::info!("could not check for updates: {e:#}"),
                }
            });
        if spawned.is_err() {
            self.lock().checking = false;
        }
    }

    /// The release being offered, unless dismissed.
    pub fn offer(&self) -> Option<Release> {
        let s = self.lock();
        s.offer
            .clone()
            .filter(|r| Some(r.version) != self.dismissed)
    }

    pub fn phase(&self) -> Option<Phase> {
        self.lock().phase.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }

    /// Whether this copy installs updates itself, or is pointed at the download.
    pub fn can_install(&self) -> bool {
        matches!(self.install, Install::Installed(_))
    }

    pub fn dismiss(&mut self) {
        let offered = {
            let mut s = self.lock();
            s.error = None;
            s.offer.as_ref().map(|r| r.version)
        };
        self.dismissed = offered;
    }

    /// The helper has been scheduled; the application must now close.
    pub fn exit_now(&self) -> bool {
        self.lock().exit_now
    }

    /// Download, verify and hand over to the installer.
    pub fn start_update(&self) {
        let Install::Installed(exe) = self.install.clone() else {
            return;
        };
        let release = {
            let mut s = self.lock();
            if s.phase.is_some() {
                return;
            }
            let Some(release) = s.offer.clone() else {
                return;
            };
            s.error = None;
            s.phase = Some(Phase::Downloading { done: 0, total: 0 });
            release
        };
        let shared = Arc::clone(&self.shared);
        let data_dir = self.data_dir.clone();
        let spawned = std::thread::Builder::new()
            .name("update-install".into())
            .spawn(move || {
                let result = install(&release, &exe, &data_dir, &shared);
                let mut s = shared.lock().unwrap_or_else(|p| p.into_inner());
                match result {
                    Ok(()) => s.exit_now = true,
                    Err(e) => {
                        log::error!("update to {} failed: {e:#}", release.version);
                        s.phase = None;
                        s.error = Some(format!("{e:#}"));
                    }
                }
            });
        if let Err(e) = spawned {
            let mut s = self.lock();
            s.phase = None;
            s.error = Some(format!("could not start the update: {e}"));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Ask GitHub for the latest release; `Some` if it is worth offering.
pub fn check(data_dir: &Path) -> Result<Option<Release>> {
    let mut body = Vec::new();
    platform::get(
        &latest_release_url(),
        "application/vnd.github+json",
        MAX_JSON_BYTES,
        &mut |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        },
        &mut |_, _| {},
    )?;
    let release = parse_release(std::str::from_utf8(&body).context("the release is not text")?)?;
    Ok(worth_offering(&release, current_version(), data_dir, now_secs()).then_some(release))
}

/// The published SHA-256 for the installer: GitHub's own digest, else the
/// release's checksum list. Neither is an error, not a reason to skip checking.
fn expected_sha256(release: &Release, asset: &Asset) -> Result<String> {
    if let Some(h) = &asset.sha256 {
        return Ok(h.clone());
    }
    let sums = release.checksums().ok_or_else(|| {
        anyhow!(
            "{} publishes no checksum for {}, so it cannot be verified and was not installed",
            release.tag,
            asset.name
        )
    })?;
    let mut text = Vec::new();
    platform::get(
        &sums.url,
        "text/plain",
        MAX_JSON_BYTES,
        &mut |c| {
            text.extend_from_slice(c);
            Ok(())
        },
        &mut |_, _| {},
    )
    .context("fetching the release's checksums")?;
    parse_sha256sums(&String::from_utf8_lossy(&text))
        .into_iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(&asset.name))
        .map(|(hash, _)| hash)
        .ok_or_else(|| anyhow!("the checksum list has no entry for {}", asset.name))
}

fn set_phase(shared: &Mutex<Shared>, phase: Phase) {
    shared.lock().unwrap_or_else(|p| p.into_inner()).phase = Some(phase);
}

/// Download the release's installer into `dir` and check it against its
/// published SHA-256. Returns the verified file; a file that fails is deleted.
pub fn download_verified(
    release: &Release,
    dir: &Path,
    phase: &mut dyn FnMut(Phase),
) -> Result<PathBuf> {
    let asset = release
        .installer()
        .ok_or_else(|| anyhow!("{} has no installer", release.tag))?;
    let expected = expected_sha256(release, asset)?;

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(&asset.name);
    let mut file =
        std::fs::File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    let mut hasher = platform::Sha256::new()?;
    let cap = if asset.size > 0 {
        asset.size.min(MAX_INSTALLER_BYTES)
    } else {
        MAX_INSTALLER_BYTES
    };
    let downloaded = platform::get(
        &asset.url,
        "application/octet-stream",
        cap,
        &mut |chunk| {
            use std::io::Write;
            hasher.update(chunk)?;
            file.write_all(chunk).context("writing the installer")
        },
        &mut |done, total| {
            phase(Phase::Downloading {
                done,
                total: total.unwrap_or(asset.size),
            })
        },
    );
    drop(file);
    if let Err(e) = downloaded {
        let _ = std::fs::remove_file(&path);
        return Err(e.context(format!("downloading {}", asset.name)));
    }

    phase(Phase::Verifying);
    let actual = hasher.finish()?;
    if actual != expected {
        let _ = std::fs::remove_file(&path);
        bail!(
            "{} failed verification (expected {expected}, got {actual}); it was deleted, not installed",
            asset.name
        );
    }
    log::info!("{} verified ({actual})", asset.name);
    Ok(path)
}

fn install(release: &Release, exe: &Path, data_dir: &Path, shared: &Mutex<Shared>) -> Result<()> {
    let dir = std::env::temp_dir().join("ed-compass-update");
    let path = download_verified(release, &dir, &mut |p| set_phase(shared, p))?;
    set_phase(shared, Phase::Installing);
    record_update(data_dir, &release.tag);
    platform::spawn_helper(std::process::id(), &path, exe, &dir.join("install.log"))
}

/// A PowerShell single-quoted literal.
#[cfg_attr(not(windows), allow(dead_code))]
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// The helper: wait for this process to exit, install silently, start the new copy.
///
/// The relaunch is done here rather than by the installer, whose own "launch"
/// step is skipped when it runs silently. If the installer fails or its UAC
/// prompt is declined, the old copy starts again — the safe outcome.
#[cfg_attr(not(windows), allow(dead_code))]
fn helper_script(pid: u32, installer: &Path, exe: &Path, log: &Path) -> String {
    format!(
        "Wait-Process -Id {pid} -Timeout 60 -ErrorAction SilentlyContinue; \
         Start-Process -FilePath {installer} -ArgumentList '/SILENT','/SUPPRESSMSGBOXES','/NORESTART','/SP-',('/LOG=\"' + {log} + '\"') -Wait; \
         Remove-Item -LiteralPath {installer} -ErrorAction SilentlyContinue; \
         Start-Process -FilePath {exe}",
        installer = ps_quote(installer),
        log = ps_quote(log),
        exe = ps_quote(exe),
    )
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, anyhow, bail};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::Networking::WinHttp::*;
    use windows::Win32::Security::Cryptography::{
        BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE, BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS,
        BCRYPT_SHA256_ALGORITHM, BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash,
        BCryptFinishHash, BCryptHashData, BCryptOpenAlgorithmProvider,
    };
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY,
        RegGetValueW,
    };
    use windows::core::{HSTRING, PCWSTR};

    /// The installer's uninstall key: `AppId` from `installer/ed-compass.iss`,
    /// with Inno Setup's `_is1` suffix.
    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{AF32677B-610B-4F3F-BA87-3BA79EA4768E}_is1";

    /// The folder the installer recorded, for a per-user install or then an
    /// all-users one.
    pub fn installed_dir() -> Option<PathBuf> {
        [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE]
            .into_iter()
            .find_map(|root| read_string(root, UNINSTALL_KEY, "InstallLocation"))
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| !p.as_os_str().is_empty())
    }

    fn read_string(root: HKEY, key: &str, value: &str) -> Option<String> {
        let (key, value) = (HSTRING::from(key), HSTRING::from(value));
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                root,
                &key,
                &value,
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let len = (bytes as usize / 2).min(buf.len());
        let s = String::from_utf16_lossy(&buf[..len]);
        Some(s.trim_end_matches('\0').to_string())
    }

    /// A WinHTTP handle, closed on drop.
    struct Handle(*mut c_void);

    impl Handle {
        fn new(raw: *mut c_void, what: &str) -> Result<Self> {
            if raw.is_null() {
                Err(windows::core::Error::from_thread()).with_context(|| what.to_string())
            } else {
                Ok(Self(raw))
            }
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }

    /// GET an `https` URL, following redirects, streaming the body into `sink`.
    ///
    /// WinHTTP rather than a Rust TLS stack: it is part of Windows, so there is
    /// nothing to ship or keep patched, and it honours the system proxy and the
    /// machine's own certificate store — which matters on a corporate network.
    pub fn get(
        url: &str,
        accept: &str,
        max_bytes: u64,
        sink: &mut dyn FnMut(&[u8]) -> Result<()>,
        progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<()> {
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| anyhow!("refusing a non-https URL: {url}"))?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };

        let agent = HSTRING::from(format!("ed-compass/{}", env!("CARGO_PKG_VERSION")));
        let session = Handle::new(
            unsafe {
                WinHttpOpen(
                    &agent,
                    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                )
            },
            "opening an HTTP session",
        )?;
        // Resolve, connect, send, receive. A stalled download fails rather than
        // hanging the banner forever.
        unsafe {
            let _ = WinHttpSetTimeouts(session.0, 15_000, 15_000, 30_000, 30_000);
        }
        let connect = Handle::new(
            unsafe {
                WinHttpConnect(
                    session.0,
                    &HSTRING::from(host),
                    INTERNET_DEFAULT_HTTPS_PORT,
                    0,
                )
            },
            "connecting",
        )?;
        let request = Handle::new(
            unsafe {
                WinHttpOpenRequest(
                    connect.0,
                    &HSTRING::from("GET"),
                    &HSTRING::from(path),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    WINHTTP_FLAG_SECURE,
                )
            },
            "opening the request",
        )?;
        let headers: Vec<u16> = format!("Accept: {accept}\r\n").encode_utf16().collect();
        unsafe {
            WinHttpAddRequestHeaders(request.0, &headers, WINHTTP_ADDREQ_FLAG_ADD)
                .context("adding request headers")?;
            WinHttpSendRequest(request.0, None, None, 0, 0, 0)
                .with_context(|| format!("requesting {host}"))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .with_context(|| format!("waiting for {host}"))?;
        }

        let status = query_number(&request, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        if status != 200 {
            bail!("{host} answered HTTP {status}");
        }
        let total = query_number(&request, WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from);
        if total.is_some_and(|t| t > max_bytes) {
            bail!(
                "the download is larger than expected ({} bytes)",
                total.unwrap_or(0)
            );
        }

        let mut buf = vec![0u8; 64 * 1024];
        let mut done = 0u64;
        progress(0, total);
        loop {
            let mut read = 0u32;
            unsafe {
                WinHttpReadData(
                    request.0,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut read,
                )
                .context("reading the response")?;
            }
            if read == 0 {
                break;
            }
            done += read as u64;
            if done > max_bytes {
                bail!("the download is larger than expected");
            }
            sink(&buf[..read as usize])?;
            progress(done, total);
        }
        if total.is_some_and(|t| t != done) {
            bail!(
                "the download ended early ({done} of {} bytes)",
                total.unwrap_or(0)
            );
        }
        Ok(())
    }

    fn query_number(request: &Handle, info: u32) -> Option<u32> {
        let mut value = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                info | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut value as *mut u32).cast()),
                &mut len,
                std::ptr::null_mut(),
            )
        }
        .ok()
        .map(|()| value)
    }

    /// SHA-256 from Windows' own cryptography provider.
    pub struct Sha256 {
        alg: BCRYPT_ALG_HANDLE,
        hash: BCRYPT_HASH_HANDLE,
    }

    impl Sha256 {
        pub fn new() -> Result<Self> {
            let mut alg = BCRYPT_ALG_HANDLE::default();
            let mut hash = BCRYPT_HASH_HANDLE::default();
            unsafe {
                BCryptOpenAlgorithmProvider(
                    &mut alg,
                    BCRYPT_SHA256_ALGORITHM,
                    PCWSTR::null(),
                    BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
                )
                .ok()
                .context("opening SHA-256")?;
                if let Err(e) = BCryptCreateHash(alg, &mut hash, None, None, 0).ok() {
                    let _ = BCryptCloseAlgorithmProvider(alg, 0);
                    return Err(anyhow::Error::from(e).context("creating a SHA-256 hash"));
                }
            }
            Ok(Self { alg, hash })
        }

        pub fn update(&mut self, data: &[u8]) -> Result<()> {
            unsafe { BCryptHashData(self.hash, data, 0) }
                .ok()
                .context("hashing")
        }

        /// Lower-case hex.
        pub fn finish(self) -> Result<String> {
            let mut out = [0u8; 32];
            unsafe { BCryptFinishHash(self.hash, &mut out, 0) }
                .ok()
                .context("finishing the hash")?;
            Ok(out.iter().map(|b| format!("{b:02x}")).collect())
        }
    }

    impl Drop for Sha256 {
        fn drop(&mut self) {
            unsafe {
                let _ = BCryptDestroyHash(self.hash);
                let _ = BCryptCloseAlgorithmProvider(self.alg, 0);
            }
        }
    }

    /// Start the helper, hidden and detached, so it outlives this process.
    pub fn spawn_helper(pid: u32, installer: &Path, exe: &Path, log: &Path) -> Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let script = super::helper_script(pid, installer, exe, log);
        std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-WindowStyle",
                "Hidden",
                "-Command",
                &script,
            ])
            // The new copy reports its real version; a pretend one was for this run.
            .env_remove(super::PRETEND_VAR)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("starting the update helper")?;
        Ok(())
    }
}

/// Everywhere else this is a Windows tool built for development; it never updates.
#[cfg(not(windows))]
mod platform {
    use std::path::{Path, PathBuf};

    use anyhow::{Result, bail};

    pub fn installed_dir() -> Option<PathBuf> {
        None
    }

    pub fn get(
        _url: &str,
        _accept: &str,
        _max_bytes: u64,
        _sink: &mut dyn FnMut(&[u8]) -> Result<()>,
        _progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<()> {
        bail!("checking for updates is only built on Windows")
    }

    pub struct Sha256;

    impl Sha256 {
        pub fn new() -> Result<Self> {
            bail!("hashing is only built on Windows")
        }
        pub fn update(&mut self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        pub fn finish(self) -> Result<String> {
            bail!("hashing is only built on Windows")
        }
    }

    pub fn spawn_helper(_pid: u32, _installer: &Path, _exe: &Path, _log: &Path) -> Result<()> {
        bail!("updating is only built on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATEST: &str = r#"{
        "tag_name": "v0.5.1",
        "html_url": "https://github.com/kay-dutch/ed-compass/releases/tag/v0.5.1",
        "draft": false,
        "prerelease": false,
        "assets": [
            {"name": "ed-compass-0.5.1-win64.zip", "browser_download_url": "https://example/zip", "size": 7000000,
             "digest": "sha256:708092bc4e56a571651b438752584399e51822da70ba7a2b3f00e3447a828736"},
            {"name": "ED-Compass-Setup-0.5.1.exe", "browser_download_url": "https://example/setup", "size": 7300000,
             "digest": "sha256:F5A65615C16815DCB9FE001EEDF8CF57E285461955A19D46648DFA592D44181B"},
            {"name": "SHA256SUMS.txt", "browser_download_url": "https://example/sums", "size": 200}
        ]
    }"#;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("ed-compass-update-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(parse_version("v0.5.0"), Some(Version(0, 5, 0)));
        assert_eq!(parse_version("0.10.2"), Some(Version(0, 10, 2)));
        assert_eq!(parse_version("1.2"), Some(Version(1, 2, 0)));
        assert_eq!(parse_version("v0.6.0-beta.1"), Some(Version(0, 6, 0)));
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        // Numeric, not lexical: 0.10 is newer than 0.9.
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(current_version().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn the_release_answer_is_read_with_its_digests() {
        let r = parse_release(LATEST).unwrap();
        assert_eq!(r.version, Version(0, 5, 1));
        let setup = r.installer().expect("an installer");
        assert_eq!(setup.name, "ED-Compass-Setup-0.5.1.exe");
        assert_eq!(
            setup.sha256.as_deref(),
            Some("f5a65615c16815dcb9fe001eedf8cf57e285461955a19d46648dfa592d44181b"),
            "GitHub's digest, normalised to lower case"
        );
        assert_eq!(
            r.checksums().map(|a| a.name.as_str()),
            Some("SHA256SUMS.txt")
        );
    }

    #[test]
    fn drafts_and_prereleases_are_never_offered() {
        let draft = LATEST.replace(r#""draft": false"#, r#""draft": true"#);
        assert!(parse_release(&draft).is_err());
        let pre = LATEST.replace(r#""prerelease": false"#, r#""prerelease": true"#);
        assert!(parse_release(&pre).is_err());
    }

    #[test]
    fn the_checksum_list_is_read_as_the_workflow_writes_it() {
        let text = "f5a65615c16815dcb9fe001eedf8cf57e285461955a19d46648dfa592d44181b  ED-Compass-Setup-0.5.1.exe\r\n\
                    708092bc4e56a571651b438752584399e51822da70ba7a2b3f00e3447a828736 *ed-compass-0.5.1-win64.zip\n\
                    not-a-hash  junk.txt\n";
        let sums = parse_sha256sums(text);
        assert_eq!(sums.len(), 2, "the malformed line is skipped: {sums:?}");
        assert_eq!(sums[0].1, "ED-Compass-Setup-0.5.1.exe");
        assert_eq!(
            sums[1].1, "ed-compass-0.5.1-win64.zip",
            "a binary-mode star is not part of the name"
        );
    }

    #[test]
    fn only_a_newer_version_is_offered() {
        let dir = temp_dir("newer");
        let r = parse_release(LATEST).unwrap();
        assert!(worth_offering(&r, Version(0, 5, 0), &dir, 1_000));
        assert!(
            !worth_offering(&r, Version(0, 5, 1), &dir, 1_000),
            "not the same version"
        );
        assert!(
            !worth_offering(&r, Version(0, 6, 0), &dir, 1_000),
            "and never a downgrade"
        );
    }

    #[test]
    fn a_version_just_installed_is_not_offered_again_for_a_day() {
        // A release whose binary reports a different version from its tag would
        // otherwise loop: update, restart, "update available", update…
        let dir = temp_dir("loop");
        let r = parse_release(LATEST).unwrap();
        std::fs::write(marker(&dir), "v0.5.1\n1000\n").unwrap();
        assert!(!worth_offering(&r, Version(0, 5, 0), &dir, 1_000 + 60));
        assert!(
            worth_offering(
                &r,
                Version(0, 5, 0),
                &dir,
                1_000 + JUST_UPDATED_FOR.as_secs() + 1
            ),
            "after a day it is offered again"
        );
        std::fs::write(marker(&dir), "garbage").unwrap();
        assert!(
            worth_offering(&r, Version(0, 5, 0), &dir, 1_000),
            "a corrupt marker is ignored"
        );
    }

    #[test]
    fn the_helper_waits_installs_silently_and_relaunches() {
        let script = helper_script(
            4242,
            Path::new(
                r"C:\Users\O'Brien\AppData\Local\Temp\ed-compass-update\ED-Compass-Setup-0.5.1.exe",
            ),
            Path::new(r"C:\Users\O'Brien\AppData\Local\Programs\ED Compass\ed-compass.exe"),
            Path::new(r"C:\Users\O'Brien\AppData\Local\Temp\ed-compass-update\install.log"),
        );
        let wait = script
            .find("Wait-Process -Id 4242")
            .expect("waits for this process");
        let setup = script
            .find("ED-Compass-Setup-0.5.1.exe")
            .expect("runs the installer");
        let relaunch = script
            .rfind("ed-compass.exe'")
            .expect("starts the app again");
        assert!(wait < setup && setup < relaunch, "in that order: {script}");
        assert!(
            script.contains("'/SILENT'") && script.contains("-Wait"),
            "silently, to completion"
        );
        assert!(
            script.contains("O''Brien"),
            "an apostrophe in a path is escaped: {script}"
        );
    }

    #[test]
    fn a_copy_outside_the_recorded_folder_never_installs() {
        // Off Windows there is no recorded folder at all; on Windows the test
        // binary runs from target\, which no installer ever recorded.
        assert_eq!(detect_install(), Install::Manual);
    }
}

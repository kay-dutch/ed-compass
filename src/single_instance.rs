//! One instance at a time.
//!
//! Two copies running against the same endpoint is not merely wasteful — it
//! loses data. Each holds its own detectors and its own capture writer, and each
//! enforces `disk_budget_mb` over the *same* folder, so they evict each other's
//! recordings. They also double the CPU and capture the same audio twice.
//!
//! A named mutex is used rather than a lock file because the operating system
//! releases it when the process dies, however it dies. A lock file left behind
//! by a crash would block every future start until deleted by hand.

/// Held for the lifetime of the process. Dropping it releases the claim.
#[derive(Debug)]
pub struct InstanceLock {
    #[cfg(windows)]
    handle: Option<windows::Win32::Foundation::HANDLE>,
}

/// Why a claim failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    /// Another instance holds the lock.
    AlreadyRunning,
    /// The lock could not be created at all; carry on rather than refuse to run.
    Unavailable(String),
}

impl std::fmt::Display for ClaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaimError::AlreadyRunning => write!(
                f,
                "another ED Compass instance is already running. Two instances \
                 capture the same audio twice and delete each other's recordings, \
                 so this one will exit."
            ),
            ClaimError::Unavailable(why) => write!(f, "could not check for other instances: {why}"),
        }
    }
}

/// Name of the mutex. Session-scoped, so two different users may each run one.
#[cfg(windows)]
const LOCK_NAME: &str = "Local\\ed-compass-single-instance";

#[cfg(windows)]
pub fn claim() -> Result<InstanceLock, ClaimError> {
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = LOCK_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe { CreateMutexW(None, true, PCWSTR(wide.as_ptr())) }
        .map_err(|e| ClaimError::Unavailable(e.to_string()))?;

    // `CreateMutexW` succeeds either way; the distinction is in the last error.
    // SAFETY: reading the calling thread's last-error value, immediately after
    // the call that set it. No pointers or handles are dereferenced.
    let already = unsafe { windows::Win32::Foundation::GetLastError() };
    if already == ERROR_ALREADY_EXISTS {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Err(ClaimError::AlreadyRunning);
    }
    Ok(InstanceLock {
        handle: Some(handle),
    })
}

#[cfg(windows)]
impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
}

#[cfg(not(windows))]
pub fn claim() -> Result<InstanceLock, ClaimError> {
    // Only Windows can run the live capture, so the collision this guards
    // against cannot happen elsewhere.
    Ok(InstanceLock {})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_claim_succeeds() {
        let lock = claim();
        assert!(lock.is_ok(), "a first claim must succeed: {lock:?}");
    }

    #[test]
    fn a_second_claim_is_refused_while_the_first_is_held() {
        let first = claim().expect("first claim");
        let second = claim();

        // Compared with `matches!` rather than `assert_eq!`: on Windows the Ok
        // variant holds a raw HANDLE and is not `PartialEq`, so the equality
        // form did not compile there — which is to say this assertion had never
        // once run on the only platform it describes.
        #[cfg(windows)]
        assert!(
            matches!(second, Err(ClaimError::AlreadyRunning)),
            "a second instance must be refused, got {second:?}"
        );
        #[cfg(not(windows))]
        assert!(
            second.is_ok(),
            "no live capture off Windows, so no collision"
        );

        // Keep the first alive until here; the claim is only meaningful while
        // it is held.
        let _ = &first;
    }

    #[test]
    fn releasing_allows_a_later_claim() {
        {
            let _held = claim().expect("claim");
        } // released here
        assert!(claim().is_ok(), "the lock must be reusable after release");
    }

    #[test]
    fn the_refusal_explains_the_consequence() {
        let message = ClaimError::AlreadyRunning.to_string();
        assert!(
            message.contains("delete each other's recordings"),
            "the message must say why it matters, not just that it happened: {message}"
        );
    }
}

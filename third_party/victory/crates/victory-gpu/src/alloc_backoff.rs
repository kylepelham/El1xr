//! After the driver refuses memory with `E_OUTOFMEMORY`, the next upload
//! buffers wait a short while instead of asking again. Without this, every
//! small upload of a burst asked the driver once more, and each refusal
//! was logged.
//!
//! Taken from codec_d's fork (`cool_fox`, `ab75ac3`), adapted to this
//! copy's upload path. Only out-of-memory refusals arm the backoff; other
//! errors are still reported where they happen.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::object::HRESULT;

/// How long a refusal holds back the next upload buffer, in milliseconds.
pub(crate) const RETRY_MS: u64 = 500;

/// `E_OUTOFMEMORY` (0x8007000E) as the HRESULT the driver returns.
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;

/// The millisecond (since first use) before which uploads are held back.
static RETRY_AT: AtomicU64 = AtomicU64::new(0);

static START: OnceLock<Instant> = OnceLock::new();

fn now_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Whether `code` is a refusal of memory, which holds uploads back.
pub(crate) fn arms(code: HRESULT) -> bool {
    code == E_OUTOFMEMORY
}

/// The time up to which uploads wait after a refusal at `now`.
pub(crate) fn until(now: u64) -> u64 {
    now.saturating_add(RETRY_MS)
}

/// Whether an upload at `now` waits for a refusal that holds until `retry_at`.
pub(crate) fn held(retry_at: u64, now: u64) -> bool {
    now < retry_at
}

/// Notes a failed allocation; an out-of-memory one holds uploads back.
pub(crate) fn note(error: &windows::core::Error) {
    if arms(error.code().0) {
        RETRY_AT.fetch_max(until(now_ms()), Ordering::Relaxed);
    }
}

/// Whether uploads are held back now.
pub(crate) fn refused() -> bool {
    held(RETRY_AT.load(Ordering::Relaxed), now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_out_of_memory_arms_the_backoff() {
        assert!(arms(E_OUTOFMEMORY));
        assert!(!arms(0));
        assert!(!arms(0x8007_0057_u32 as i32));
    }

    #[test]
    fn a_refusal_holds_uploads_for_the_retry_interval_only() {
        let retry_at = until(1_000);
        assert_eq!(retry_at, 1_000 + RETRY_MS);
        assert!(held(retry_at, 1_000));
        assert!(held(retry_at, retry_at - 1));
        assert!(!held(retry_at, retry_at));
        assert!(!held(0, 1_000));
    }

    #[test]
    fn the_retry_time_does_not_overflow() {
        assert_eq!(until(u64::MAX), u64::MAX);
    }
}

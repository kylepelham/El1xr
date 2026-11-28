//! (hangsafe/worldstream, #26) `VICTORY_DSTORAGE_FILE_REF=1`: a read the
//! title enqueues holds a reference on its file object until the read is
//! done (let go before its status entry can say so), so the title may
//! release its `IDStorageFile` right after `EnqueueRequest`, as DirectStorage
//! allows. Unset: reads trust the title to keep its files open until they
//! complete, as before (a file released early is freed under a running
//! read: a crash or garbage in a streamed texture).
//!
//! Source: darkworldhm (after fxceless victory-fork #621 by Cuebert).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{
    BCPACK, File, Instance, MEMORY_SOURCE, PAGE_DESTINATION, Request, ZLIB, add_ref_instance,
    release,
};

/// A read's reference on its file object, let go when dropped.
#[derive(Default)]
pub(super) struct FileHold(Option<*mut Instance<File>>);

// SAFETY: the object counts its references atomically.
unsafe impl Send for FileHold {}

impl FileHold {
    /// The reference `request` holds on its file: one with the switch on
    /// and a file source, none otherwise.
    pub(super) fn new(request: &Request) -> Self {
        #[cfg(test)]
        let on = || on() || FORCE.load(Relaxed);
        if !on() {
            return Self::default();
        }
        Self::of(request)
    }

    /// A reference on `request`'s file object, if it reads one.
    fn of(request: &Request) -> Self {
        const SUPPORTED: u64 = ZLIB | BCPACK | PAGE_DESTINATION | MEMORY_SOURCE;
        let reads_a_file = request.options & !SUPPORTED == 0
            && request.options & MEMORY_SOURCE == 0
            && !request.source.is_null();
        if !reads_a_file {
            return Self::default();
        }
        let file = request.source.cast::<Instance<File>>().cast_mut();
        add_ref_instance(file);
        HELD.fetch_add(1, Relaxed);
        Self(Some(file))
    }
}

impl Drop for FileHold {
    fn drop(&mut self) {
        if let Some(file) = self.0.take() {
            if release(file) == 0 {
                let n = LAST.fetch_add(1, Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!(
                        n,
                        held = HELD.load(Relaxed),
                        "dstorage: files the title had released, kept open by their reads until done (VICTORY_DSTORAGE_FILE_REF)"
                    );
                }
            }
        }
    }
}

/// Tests: the switch on, for every read enqueued from now on.
#[cfg(test)]
pub(super) static FORCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// References taken, and reads that held the last one.
static HELD: AtomicU64 = AtomicU64::new(0);
static LAST: AtomicU64 = AtomicU64::new(0);

/// The switch (`VICTORY_DSTORAGE_FILE_REF`; on only when `1`).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_DSTORAGE_FILE_REF").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "VICTORY_DSTORAGE_FILE_REF=1: reads hold their file open until they are done"
            );
        }
        on
    })
}

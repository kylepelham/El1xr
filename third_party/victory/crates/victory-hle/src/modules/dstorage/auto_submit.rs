//! (worldstream, #26) `VICTORY_DSTORAGE_AUTO_SUBMIT=title`: a queue whose
//! description sets [`super::AUTO_SUBMIT_DISABLED`] (the title sets it on
//! every queue) never submits itself once half full; its reads start at the
//! title's Submit, as the console runs them. Unset: every queue submits
//! itself at half (reads into physical pages and the status entries behind
//! them still wait for the Submit), as before.
//!
//! Why: a read the title enqueued is not the title's to start yet: until
//! its Submit, the destination may still be read by the card or written by
//! the title (a reused buffer), so starting it early can overwrite memory
//! in use (a stale or torn texture, a one-frame flicker). It also shows
//! whether the early start buys loading time (boot_s and the
//! `dstorage: last` lines' waiting/busy_workers, on against off).
//!
//! A queue that does not submit itself and fills up is submitted here, so
//! the title is never left waiting for a slot it cannot free.
//!
//! Source: darkworldhm (DSTORAGE_QUEUE_DESC1 options, after fxceless
//! victory-fork #575 by Cuebert).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether a queue made with `options` submits itself once half full.
pub(super) fn submits_itself(options: u64) -> bool {
    decide(honoured(), options)
}

/// [`submits_itself`] with the switch given.
fn decide(honour: bool, options: u64) -> bool {
    !(honour && options & super::AUTO_SUBMIT_DISABLED != 0)
}

/// The switch: `title` honours the title's option.
fn honoured() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_DSTORAGE_AUTO_SUBMIT").ok().as_deref());
        if on {
            tracing::info!(
                "VICTORY_DSTORAGE_AUTO_SUBMIT=title: queues the title made with auto-submit disabled start their reads at its Submit"
            );
        }
        on
    })
}

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("title"))
}

/// Commands enqueued on queues that do not submit themselves.
static LEFT: AtomicU64 = AtomicU64::new(0);

/// A command was enqueued on a queue that does not submit itself.
pub(super) fn left_for_the_title() {
    let n = LEFT.fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() && n >= 1024 {
        tracing::info!(
            n,
            "dstorage: commands left for the title's Submit on queues that do not submit themselves (VICTORY_DSTORAGE_AUTO_SUBMIT=title)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_title_honours_the_option() {
        assert!(parse(Some("title")));
        assert!(parse(Some(" Title ")));
        assert!(!parse(Some("1")));
        assert!(!parse(Some("")));
        assert!(!parse(None));
    }

    #[test]
    fn a_queue_without_auto_submit_waits_for_the_submit_only_when_honoured() {
        let off = super::super::AUTO_SUBMIT_DISABLED;
        assert!(decide(false, off), "unset: submits itself, as before");
        assert!(!decide(true, off));
        assert!(
            decide(true, 0),
            "a queue that asks for auto-submit keeps it"
        );
        assert!(decide(true, 2), "other option bits leave it on");
    }
}

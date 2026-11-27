//! A texture a draw reads through a fresh descriptor table for the first
//! time (no host copy yet) is not made and uploaded on the thread that
//! records the draw (`VICTORY_TABLE_DEFER`, on; `0` turns it off): it is
//! handed to the texture prepare threads (`crate::texture::prepare`, ahead of
//! the texture pass's queue) and the table's slot reads nothing (a null view)
//! until they made it ready. The next draw that binds it finds it ready and
//! records its upload as before, from the staging buffer the thread made.
//!
//! During world streaming the recording threads spent 200-1000 ms a frame
//! (all threads) making fresh tables, much of it making host copies and
//! staging buffers of textures read for the first time.
//!
//! Never for a draw of the title's decoration bake (what it reads must be
//! there now: shortened bakes left faces black), for dispatches, for views
//! other than shader resource views of textures the title fills, without
//! prepare threads, or while the card has no room for new host copies.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::descriptor::View;
use crate::object::This;

thread_local! {
    /// While a draw binds (`State::bind`): whether its tables' textures
    /// may wait (not a bake draw, not a dispatch).
    static MAY_WAIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Since the last line: table slots left null for a texture being made
/// ready, and of them the textures handed out now.
static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The switch (`VICTORY_TABLE_DEFER`; on unless `0`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_TABLE_DEFER").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_none_or(|v| v != "0")
}

/// The bytes a frame may hand out, as the texture pass's
/// (`VICTORY_TEXTURE_STAGE_MB`, 192 unless set).
fn budget() -> u64 {
    static MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MB.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_STAGE_MB")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(192)
    }) << 20
}

/// A draw starts binding: `bake` a draw of the title's decoration bake,
/// `compute` a dispatch.
pub(crate) fn begin(bake: bool, compute: bool) {
    MAY_WAIT.set(may_wait(
        on(),
        crate::texture::prepare::threads(),
        bake,
        compute,
    ));
}

fn may_wait(on: bool, threads: usize, bake: bool, compute: bool) -> bool {
    on && threads > 0 && !bake && !compute
}

/// What a texture's state says about leaving its table slot null now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Texture {
    /// A shader resource view the draw's shader reads.
    read: bool,
    title_fills: bool,
    host_copy_made: bool,
    host_refused: bool,
    room: bool,
}

/// Whether the texture is one to hand out instead of making it now.
fn candidate(t: Texture) -> bool {
    t.read && t.title_fills && !t.host_copy_made && !t.host_refused && t.room
}

/// Whether table slot `title_slot` (the title's slot of the table entry),
/// read as `kind` through `view`, is left null for this draw because its
/// texture is being made ready on the prepare threads. False: make it and
/// make it readable now, as before.
pub(crate) fn wait(
    view: &View,
    kind: Option<victory_shader::ResourceType>,
    title_slot: Option<u32>,
) -> bool {
    if !MAY_WAIT.get() {
        return false;
    }
    let Some(slot) = title_slot else {
        return false;
    };
    let read = kind == Some(victory_shader::ResourceType::Srv);
    if !read {
        return false;
    }
    let (resource, first) = match view {
        View::Shader(resource, _) => (*resource as This, 0),
        View::Placed(false, _, desc) => match crate::descriptor::host::placed_texture(view) {
            Some(View::Shader(resource, _)) => (resource as This, crate::resource::first_mip(desc)),
            _ => return false,
        },
        _ => return false,
    };
    let t = Texture {
        read,
        title_fills: crate::resource::title_fills(resource),
        host_copy_made: crate::resource::host_copy_made(resource),
        host_refused: crate::resource::host_refused(resource),
        room: crate::resource::room_lately(),
    };
    if !candidate(t) {
        return false;
    }
    use crate::texture::prepare::Look;
    let waits = match crate::texture::prepare::look_urgent(
        slot,
        resource,
        first,
        budget(),
        crate::frame::current(),
    ) {
        Look::Go | Look::Full => false,
        Look::Wait => true,
        Look::Handed(_) => {
            STATS[1].fetch_add(1, Relaxed);
            true
        }
    };
    if waits {
        STATS[0].fetch_add(1, Relaxed);
        line();
    }
    waits
}

/// About every five seconds while slots are left null: how many.
fn line() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let next = NEXT.load(Relaxed);
    if now < next
        || NEXT
            .compare_exchange(next, now + 5000, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let [null, handed] = [0, 1].map(|i| STATS[i].swap(0, Relaxed));
    tracing::info!(
        null,
        handed,
        "table slots left null while their texture's first upload is made ready on the prepare threads (VICTORY_TABLE_DEFER=0: made on the recording thread)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_on_unless_zero() {
        assert!(switch(None));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
    }

    /// Bakes, dispatches, the switch off and no prepare threads: never.
    #[test]
    fn only_plain_draws_with_prepare_threads_may_wait() {
        assert!(may_wait(true, 4, false, false));
        assert!(!may_wait(true, 4, true, false));
        assert!(!may_wait(true, 4, false, true));
        assert!(!may_wait(false, 4, false, false));
        assert!(!may_wait(true, 0, false, false));
    }

    /// Only a read texture the title fills, never given a host copy, not
    /// refused, while the card has room.
    #[test]
    fn only_never_made_title_textures_with_room_are_handed_out() {
        let t = Texture {
            read: true,
            title_fills: true,
            host_copy_made: false,
            host_refused: false,
            room: true,
        };
        assert!(candidate(t));
        assert!(!candidate(Texture { read: false, ..t }));
        assert!(!candidate(Texture {
            title_fills: false,
            ..t
        }));
        assert!(!candidate(Texture {
            host_copy_made: true,
            ..t
        }));
        assert!(!candidate(Texture {
            host_refused: true,
            ..t
        }));
        assert!(!candidate(Texture { room: false, ..t }));
    }

    /// Outside a draw's binding nothing waits.
    #[test]
    fn nothing_waits_outside_a_binding() {
        MAY_WAIT.set(false);
        let view = View::Shader(1, None);
        assert!(!wait(
            &view,
            Some(victory_shader::ResourceType::Srv),
            Some(3)
        ));
    }
}

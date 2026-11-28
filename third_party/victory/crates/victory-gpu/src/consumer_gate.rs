//! `VICTORY_CONSUMER_GATE=1|2` (off without it): a compute stage that reads
//! what a dropped stage of its chain should have written this frame is not
//! run either.
//!
//! A dispatch or indirect dispatch is dropped when its title pipeline has no
//! host pipeline (refused at translation, or still being made at world
//! entry), cannot be bound, is a ray-only pass left out, or a stage of the
//! structure builder that is off. Its outputs then keep last frame's values,
//! or whatever the memory held. TuxBox issue #232 (RTX 5090 and 3080 Ti):
//! with the shade, gather and trace stages of the title's primary-ray and
//! irradiance-cache chains written nothing, their verify, virtual-table and
//! probe-signal stages ran on that data and held the card past its context
//! switch timeout (Xid 109, 5-7 s into the intro world). Making the six
//! consumers no-ops moved the hang to about 128 s.
//!
//! The chains are known by program name (`CS_<chain>_<stage>_...`):
//! - consumers: `ShadeVerify`, `UpdateVirtualTable`,
//!   `ConsumeProbeShadingSignal`;
//! - producers (`=1`): `GatherVBuffer*`, `Shade`, `ShadeSecondary`,
//!   `ShadowTrace*`, `RegisterShadowTraceRequests`, `IntegrateIrradianceSH`;
//!   `=2`: every other stage of the chain too (its argument writers,
//!   schedulers, allocators).
//!
//! A consumer is held while a producer of its chain was dropped in this
//! frame or the one before (a consumer that comes before its producer in a
//! frame reads what the last frame's did not write). Held dispatches are
//! counted: a `consumer gate:` line at the first hold of a chain, one every
//! 5 s while it holds, and a line in the hang watch.
//!
//! Source: brcly/TuxBox issue #232 (NickStallman), the finding only; no code.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// A chain of the title's compute stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chain {
    Primary = 0,
    IrradianceCache = 1,
}

const CHAINS: [(&str, Chain); 2] = [
    ("CS_Primary_", Chain::Primary),
    ("CS_IrradianceCache_", Chain::IrradianceCache),
];

/// Stages that read what the producers write.
const CONSUMERS: [&str; 3] = [
    "ShadeVerify",
    "UpdateVirtualTable",
    "ConsumeProbeShadingSignal",
];

/// The stages TuxBox found written nothing before the hang.
const PRODUCERS: [&str; 6] = [
    "GatherVBuffer",
    "Shade",
    "ShadeSecondary",
    "ShadowTrace",
    "RegisterShadowTraceRequests",
    "IntegrateIrradianceSH",
];

/// What a stage is in its chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Consumer,
    Producer,
    /// Any other stage: a producer with `=2`.
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Role {
    chain: Chain,
    kind: Kind,
}

/// The role of a program by its name (`CS_Primary_ShadeVerify_214ec2b4_Wrapped`).
pub(crate) fn role_of(name: &str) -> Option<Role> {
    let (rest, chain) = CHAINS
        .iter()
        .find_map(|&(prefix, chain)| name.strip_prefix(prefix).map(|rest| (rest, chain)))?;
    let stage = rest.split('_').next().unwrap_or(rest);
    let kind = if CONSUMERS.contains(&stage) {
        Kind::Consumer
    } else if PRODUCERS.contains(&stage) {
        Kind::Producer
    } else {
        Kind::Other
    };
    Some(Role { chain, kind })
}

/// The switch: 0 off, 1 the named producers, 2 every stage of a chain.
/// Read once.
pub(crate) fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let level = parse(std::env::var("VICTORY_CONSUMER_GATE").ok().as_deref());
        if level > 0 {
            tracing::info!(
                "VICTORY_CONSUMER_GATE={level}: verify, virtual-table and probe-signal stages of the primary and irradiance-cache chains are held while a producer of theirs was dropped"
            );
        }
        level
    })
}

fn parse(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

fn produces(kind: Kind, level: u8) -> bool {
    match kind {
        Kind::Producer => level >= 1,
        Kind::Other => level >= 2,
        Kind::Consumer => false,
    }
}

/// Whether a producer dropped with stamp `stamp` (its frame + 1; 0: never)
/// holds a consumer in `frame`: dropped in this frame or the one before.
fn holds_at(stamp: u64, frame: u64) -> bool {
    stamp != 0 && stamp >= frame
}

/// Per chain: the frame + 1 a producer was last dropped in (0: never).
static DROPPED_IN: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Per chain: producer dispatches dropped, consumer dispatches held.
static DROPS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static HELD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Holds at the last periodic line, and when it was (ms since the first).
static SAID: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

thread_local! {
    /// Dispatches this thread issued to a host list.
    static ISSUED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The role of title pipeline `this`; `None` when the switch is off.
pub(crate) fn role(this: crate::object::This) -> Option<Role> {
    if level() == 0 {
        return None;
    }
    crate::pipeline::chain_role(this)
}

/// Before a dispatch of `role`: whether it is held (counted and said).
pub(crate) fn holds(role: Option<Role>) -> bool {
    let Some(role) = role.filter(|r| r.kind == Kind::Consumer) else {
        return false;
    };
    let c = role.chain as usize;
    if !holds_at(DROPPED_IN[c].load(Relaxed), crate::frame::current()) {
        return false;
    }
    let held = HELD[c].fetch_add(1, Relaxed) + 1;
    if held == 1 {
        tracing::warn!(
            chain = ?role.chain,
            frame = crate::frame::current(),
            "consumer gate: a consumer stage held, a producer of its chain was dropped (VICTORY_CONSUMER_GATE)"
        );
    }
    periodic();
    true
}

/// A dispatch issued to a host list (straight after the host `Dispatch`).
pub(crate) fn issued() {
    ISSUED.with(|i| i.set(i.get() + 1));
}

/// Dispatches this thread has issued ([`issued`]).
pub(crate) fn issued_count() -> u64 {
    ISSUED.with(std::cell::Cell::get)
}

/// After a dispatch of `role`: whether it ran. A producer that did not
/// holds its chain's consumers.
pub(crate) fn after(role: Option<Role>, ran: bool) {
    let Some(role) = role else { return };
    if ran || !produces(role.kind, level()) {
        return;
    }
    let c = role.chain as usize;
    DROPPED_IN[c].store(crate::frame::current() + 1, Relaxed);
    if DROPS[c].fetch_add(1, Relaxed) == 0 {
        tracing::warn!(
            chain = ?role.chain,
            frame = crate::frame::current(),
            "consumer gate: a producer stage was dropped; its chain's consumers are held while that goes on"
        );
    }
}

/// One line every 5 s while consumers are being held.
fn periodic() {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64;
    let last = LAST_MS.load(Relaxed);
    if now < last + 5000
        || LAST_MS
            .compare_exchange(last, now.max(1), Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let held = [0, 1].map(|c| HELD[c].load(Relaxed));
    let since = [0, 1].map(|c| held[c] - SAID[c].swap(held[c], Relaxed));
    tracing::info!("consumer gate: {}", line(held, since));
}

fn line(held: [u64; 2], since: [u64; 2]) -> String {
    let drops = [0, 1].map(|c| DROPS[c].load(Relaxed));
    format!(
        "held {} consumer dispatches ({} since the last line): primary {}, irradiance cache {}; producer dispatches dropped: primary {}, irradiance cache {}",
        held[0] + held[1],
        since[0] + since[1],
        held[0],
        held[1],
        drops[0],
        drops[1],
    )
}

/// For the hang watch: what the gate held and why; `None` when off.
pub(crate) fn hang_line() -> Option<String> {
    let level = level();
    if level == 0 {
        return None;
    }
    let held = [0, 1].map(|c| HELD[c].load(Relaxed));
    let frame = crate::frame::current();
    let holding: Vec<&str> = [("primary", 0), ("irradiance cache", 1)]
        .into_iter()
        .filter(|&(_, c)| holds_at(DROPPED_IN[c].load(Relaxed), frame))
        .map(|(name, _)| name)
        .collect();
    Some(format!(
        "consumer gate ({level}): {}; holding now: {}",
        line(held, [0, 0]),
        if holding.is_empty() {
            "none".to_owned()
        } else {
            holding.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_six_consumers_are_known_by_name() {
        for name in [
            "CS_Primary_ShadeVerify_214ec2b4_Wrapped",
            "CS_IrradianceCache_ShadeVerify_0fd5a201_Wrapped",
            "CS_Primary_UpdateVirtualTable_6db0122e_Wrapped",
            "CS_IrradianceCache_UpdateVirtualTable_f1ac830c_Wrapped",
            "CS_Primary_ConsumeProbeShadingSignal_24cafd88_Wrapped",
            "CS_IrradianceCache_ConsumeProbeShadingSignal_104a3aab_Wrapped",
        ] {
            assert_eq!(
                role_of(name).map(|r| r.kind),
                Some(Kind::Consumer),
                "{name}"
            );
        }
    }

    /// Stage names that begin like a producer's are other stages
    /// (`ShadeCopy` is not `Shade`).
    #[test]
    fn producers_are_matched_by_whole_stage() {
        let kind = |name| role_of(name).map(|r| r.kind);
        assert_eq!(
            kind("CS_Primary_Shade_9f00aa11_Wrapped"),
            Some(Kind::Producer)
        );
        assert_eq!(
            kind("CS_Primary_GatherVBuffer_WithFar_cc76bb5a_Wrapped"),
            Some(Kind::Producer)
        );
        assert_eq!(
            kind("CS_IrradianceCache_ShadeSecondary_1_Wrapped"),
            Some(Kind::Producer)
        );
        assert_eq!(
            kind("CS_IrradianceCache_ShadowTrace_WithFar_2_Wrapped"),
            Some(Kind::Producer)
        );
        assert_eq!(
            kind("CS_Primary_ShadeCopy_a40799d4_Wrapped"),
            Some(Kind::Other)
        );
        assert_eq!(
            kind("CS_Primary_GenerateShadeArgs_dd7b4baa_Wrapped"),
            Some(Kind::Other)
        );
        assert_eq!(kind("CS_RaytracingResolve_Opaque_1_Wrapped"), None);
        assert_eq!(kind("CS_PrimaryX_Shade_1"), None);
        assert_eq!(
            role_of("CS_Primary_Shade").map(|r| r.chain),
            Some(Chain::Primary)
        );
    }

    #[test]
    fn the_switch_levels() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("yes")), 0);
        assert!(produces(Kind::Producer, 1));
        assert!(!produces(Kind::Other, 1));
        assert!(produces(Kind::Other, 2));
        assert!(!produces(Kind::Consumer, 2));
        assert!(!produces(Kind::Producer, 0));
    }

    /// A drop holds the consumers of its frame and the next, not later ones.
    #[test]
    fn a_drop_holds_for_this_frame_and_the_next() {
        assert!(!holds_at(0, 0));
        assert!(!holds_at(0, 7));
        let stamp = 10 + 1;
        assert!(holds_at(stamp, 10));
        assert!(holds_at(stamp, 11));
        assert!(!holds_at(stamp, 12));
    }

    /// A consumer is held only after a producer of its own chain was dropped.
    #[test]
    fn only_the_dropped_chains_consumers_are_held() {
        let primary_consumer = role_of("CS_Primary_ShadeVerify_1");
        let cache_consumer = role_of("CS_IrradianceCache_ShadeVerify_1");
        let cache_producer = role_of("CS_IrradianceCache_Shade_1");
        assert!(!holds(primary_consumer));
        assert!(!holds(cache_consumer));
        // The switch is off in tests: `after` marks nothing.
        after(cache_producer, false);
        assert!(!holds(cache_consumer));
        // As a drop with the switch on marks it.
        DROPPED_IN[Chain::IrradianceCache as usize].store(crate::frame::current() + 1, Relaxed);
        assert!(holds(cache_consumer));
        assert!(!holds(primary_consumer));
        assert!(!holds(cache_producer), "a producer is never held");
        assert!(HELD[Chain::IrradianceCache as usize].load(Relaxed) >= 1);
        DROPPED_IN[Chain::IrradianceCache as usize].store(0, Relaxed);
    }
}

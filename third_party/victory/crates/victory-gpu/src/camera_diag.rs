//! How the camera tap names the title's programs, and
//! `VICTORY_CAMERA_DIAG=1` (off by default, needs `VICTORY_CAMERA_CAPTURE=1`):
//! now and then one whole frame's candidate passes for the camera (velocity,
//! depth, TAA and upscale, blits and composites, the UI) are logged, one
//! line each with the render targets, depth and textures they bind (title
//! pointer, host DXGI format, size), so a run tells which passes write
//! depth, motion vectors and the picture before the UI even when the
//! camera tap's match misses.
//!
//! A graphics container names its semantics (`SV_Position`, `SV_Target`)
//! before its program, so the program is the identifier ending in
//! `_Wrapped` ([`wrapped_name`]), not the first `XX_` one.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

/// `VICTORY_CAMERA_DIAG=1` (needs `VICTORY_CAMERA_CAPTURE=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::camera::on() && std::env::var_os("VICTORY_CAMERA_DIAG").is_some_and(|v| v == "1")
    })
}

/// The program a container carries: the identifier that ends in
/// `_Wrapped` (`PS_VelocityFill_4f86a26b_Wrapped`), from its first letter.
pub fn wrapped_name(container: &[u8]) -> Option<&str> {
    const END: &[u8] = b"_Wrapped";
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let at = container.windows(END.len()).position(|w| w == END)?;
    let end = at + END.len();
    if container.get(end).is_some_and(|&b| ident(b)) {
        return None;
    }
    let start = container[..at]
        .iter()
        .rposition(|&b| !ident(b))
        .map_or(0, |p| p + 1);
    let start = start
        + container[start..at]
            .iter()
            .position(u8::is_ascii_alphabetic)?;
    std::str::from_utf8(&container[start..end]).ok()
}

/// Programs of the title's by `Shader::hash` (the first 16 hex digits of
/// the container's blake3), from `shaders.rpf` of Series X 1.0.2.1, for a
/// container that carries no name.
pub const KNOWN: &[(&str, &str)] = &[
    ("9ab96b7a002d6ac8", "PS_VelocityFill_4f86a26b_Wrapped"),
    ("4adcfeaa24ae6450", "VS_VelocityFill_4f86a26b_Wrapped"),
    (
        "0c35931f998617b7",
        "CS_2GBufFixupAndClearTemp_Velocity_2b5e7808_Wrapped",
    ),
    (
        "da73a9a6e262b539",
        "PS_2GBufFixupAndClearTemp_Velocity_ca13643b_Wrapped",
    ),
    (
        "565cc0b38920b229",
        "CS_2GBufFixupAndClearTemp_649be315_Wrapped",
    ),
    (
        "57618bcd8cc9101e",
        "CS_CombineVelocityTextures_f885d39d_Wrapped",
    ),
    (
        "6cf64d35bc433830",
        "PS_BlitUISDRToBackbuffer_8e9c2e8a_Wrapped",
    ),
    (
        "4f4ec7b16673362f",
        "PS_TAA2_SpatialUpscaleWithDithering_08f754c8_Wrapped",
    ),
    ("df11dcd012cd7382", "CS_TAA2_Resolve_dcad0c81_Wrapped"),
];

/// The program's name: carried, else known by hash, else empty.
pub fn program_name(container: &[u8], hash: &str) -> String {
    wrapped_name(container)
        .or_else(|| KNOWN.iter().find(|k| k.0 == hash).map(|k| k.1))
        .unwrap_or("")
        .to_owned()
}

/// A Scaleform UI pixel program (`FVertexCxformEAlpha`, `FTexTGCxformEAlpha`,
/// `FSolid`, `FText`...): the HUD and menus draw with these.
pub fn is_scaleform(name: &str) -> bool {
    let n = name.split(':').next_back().unwrap_or(name);
    n.starts_with('F')
        && !n.starts_with("FX")
        && (n.contains("Cxform")
            || ["FSolid", "FText", "FTexTG", "FVertex", "FYUV", "FBox"]
                .iter()
                .any(|p| n.starts_with(p)))
}

/// Why a program is a candidate for the camera, by name.
pub fn tag(name: &str) -> Option<&'static str> {
    const KEYS: &[(&str, &str)] = &[
        ("Velocity", "velocity"),
        ("Motion", "motion"),
        ("BlitUI", "ui"),
        ("UIColor", "ui"),
        ("Backbuffer", "backbuffer"),
        ("TAA", "taa"),
        ("TemporalResolve", "taa"),
        ("Upscale", "upscale"),
        ("Composite", "composite"),
        ("PostFX", "postfx"),
        ("onemap", "tonemap"),
        ("Blit", "blit"),
        ("ResolveDepth", "depth"),
        ("DepthResolve", "depth"),
        ("Clear", "clear"),
    ];
    if is_scaleform(name) {
        return Some("scaleform");
    }
    KEYS.iter().find(|k| name.contains(k.0)).map(|k| k.1)
}

/// `DXGI_FORMAT_R16G16_FLOAT`, the motion vectors' likely format.
pub const R16G16_FLOAT: i32 = 34;

/// One binding of a candidate pass: `rt0`, `dsv`, `u19`, `t211`...
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    pub slot: String,
    pub title: u64,
    pub format: i32,
    pub size: [u32; 2],
}

/// One bind of a candidate pass.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The bind's place among the frame's binds (all threads).
    pub bind: u32,
    pub list: usize,
    pub stage: &'static str,
    pub hash: String,
    pub name: String,
    pub tag: Option<&'static str>,
    pub shapes: Vec<Shape>,
}

impl Entry {
    /// A candidate: named like one, or writing an R16G16_FLOAT target.
    pub fn wanted(&self) -> bool {
        self.tag.is_some()
            || self.shapes.iter().any(|s| {
                s.format == R16G16_FLOAT && (s.slot.starts_with("rt") || s.slot.starts_with('u'))
            })
    }

    pub fn line(&self, frame: u64, of: usize, index: usize) -> String {
        let shapes: Vec<String> = self
            .shapes
            .iter()
            .map(|s| {
                format!(
                    "{} {:#x} fmt {} {}x{}",
                    s.slot, s.title, s.format, s.size[0], s.size[1]
                )
            })
            .collect();
        format!(
            "camera diag: frame={frame} #{index}/{of} bind={} list={:#x} {} {} {} [{}] {}",
            self.bind,
            self.list,
            self.stage,
            self.hash,
            if self.name.is_empty() {
                "?"
            } else {
                &self.name
            },
            self.tag.unwrap_or("shape"),
            shapes.join("; ")
        )
    }
}

/// Seconds between logged frames, and how many frames a run logs.
const EVERY_S: f32 = 10.0;
const MAX_DUMPS: u32 = 12;
/// Entries logged per frame at most.
const MAX_ENTRIES: usize = 160;

static RECORDING: AtomicBool = AtomicBool::new(false);
static BINDS: AtomicU32 = AtomicU32::new(0);
static DUMPS: AtomicU32 = AtomicU32::new(0);
static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Whether this frame's binds are being looked at (else nothing to do).
pub fn recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// The next bind's place in the frame.
pub fn next_bind() -> u32 {
    BINDS.fetch_add(1, Ordering::Relaxed)
}

/// Keeps a bind's entry for the frame's dump if it is a candidate.
pub fn note(e: Entry) {
    if e.wanted() {
        ENTRIES.lock().unwrap_or_else(|e| e.into_inner()).push(e);
    }
}

/// Lines for one frame's entries, in bind order, with a summary first.
pub fn dump_lines(frame: u64, binds: u32, mut entries: Vec<Entry>, found: &str) -> Vec<String> {
    entries.sort_by_key(|e| e.bind);
    let n = entries.len();
    let mut out = vec![format!(
        "camera diag: frame={frame} binds={binds} candidates={n} logged={} {found}",
        n.min(MAX_ENTRIES)
    )];
    out.extend(
        entries
            .iter()
            .take(MAX_ENTRIES)
            .enumerate()
            .map(|(i, e)| e.line(frame, n, i)),
    );
    out
}

/// End of frame `frame`: logs the recorded frame's candidates, and decides
/// whether the next frame is recorded (every [`EVERY_S`], at most
/// [`MAX_DUMPS`] frames a run).
pub fn frame_end(frame: u64, found: &str) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    if !on() {
        return;
    }
    let binds = BINDS.swap(0, Ordering::Relaxed);
    let entries = std::mem::take(&mut *ENTRIES.lock().unwrap_or_else(|e| e.into_inner()));
    if RECORDING.swap(false, Ordering::Relaxed) {
        DUMPS.fetch_add(1, Ordering::Relaxed);
        for line in dump_lines(frame, binds, entries, found) {
            tracing::info!("{line}");
        }
    }
    if DUMPS.load(Ordering::Relaxed) >= MAX_DUMPS {
        return;
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_none_or(|t| t.elapsed().as_secs_f32() >= EVERY_S) {
        *last = Some(std::time::Instant::now());
        RECORDING.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_is_the_wrapped_name_not_the_first_semantic() {
        // As in shaders.rpf: semantics first, then the program.
        let ps =
            b"\0SV_Position\0TEXCOORD\0SV_Target\0TEXCOORD\0PS_VelocityFill_4f86a26b_Wrapped\0YR^u";
        assert_eq!(wrapped_name(ps), Some("PS_VelocityFill_4f86a26b_Wrapped"));
        let cs =
            b"..\0deferred_to_gbuffer_fill:CS_2GBufFixupAndClearTemp_Velocity_2b5e7808_Wrapped\0";
        assert_eq!(
            wrapped_name(cs),
            Some("CS_2GBufFixupAndClearTemp_Velocity_2b5e7808_Wrapped")
        );
        assert_eq!(wrapped_name(b"\x01\x02_9_Wrapped"), None);
        assert_eq!(wrapped_name(b"SV_Position\0SV_Target\0"), None);
        assert_eq!(wrapped_name(b"PS_X_Wrappedness"), None);
    }

    #[test]
    fn a_nameless_container_is_known_by_its_hash() {
        assert_eq!(
            program_name(b"SV_Position", "9ab96b7a002d6ac8"),
            "PS_VelocityFill_4f86a26b_Wrapped"
        );
        assert_eq!(program_name(b"SV_Position", "0000000000000000"), "");
        assert_eq!(
            program_name(b"\0PS_Blit_f0d2c907_Wrapped\0", "9ab96b7a002d6ac8"),
            "PS_Blit_f0d2c907_Wrapped"
        );
    }

    #[test]
    fn candidates_by_name_and_by_shape() {
        assert_eq!(tag("PS_VelocityFill_4f86a26b_Wrapped"), Some("velocity"));
        assert_eq!(tag("PS_BlitUISDRToBackbuffer_8e9c2e8a_Wrapped"), Some("ui"));
        assert_eq!(
            tag("PS_TAA2_SpatialUpscaleWithDithering_08f754c8_Wrapped"),
            Some("taa")
        );
        assert_eq!(
            tag("FVertexCxformEAlpha_bc8bd7de_Wrapped"),
            Some("scaleform")
        );
        assert_eq!(
            tag("FTexTGCxformEAlphaDither_62a02128_Wrapped"),
            Some("scaleform")
        );
        assert_eq!(tag("ps_simple_tonemap_6a08c418_Wrapped"), Some("tonemap"));
        assert_eq!(tag("CS_Gather_Classified_13_255b235d_Wrapped"), None);
        assert!(!is_scaleform("FXAA_Something_Wrapped"));
        let mut e = Entry {
            bind: 3,
            list: 0x10,
            stage: "compute",
            hash: "0123456789abcdef".into(),
            name: "CS_Unknown_Wrapped".into(),
            tag: None,
            shapes: vec![Shape {
                slot: "t3".into(),
                title: 0x20,
                format: R16G16_FLOAT,
                size: [1920, 1080],
            }],
        };
        assert!(!e.wanted(), "read, not written");
        e.shapes[0].slot = "u19".into();
        assert!(e.wanted());
        let line = e.line(7, 1, 0);
        assert!(line.contains("frame=7 #0/1 bind=3 list=0x10 compute 0123456789abcdef CS_Unknown_Wrapped [shape] u19 0x20 fmt 34 1920x1080"), "{line}");
    }

    #[test]
    fn a_dump_is_in_bind_order_and_capped() {
        let e = |bind| Entry {
            bind,
            list: 1,
            stage: "pixel",
            hash: String::new(),
            name: String::new(),
            tag: Some("blit"),
            shapes: Vec::new(),
        };
        let mut entries: Vec<Entry> = (0..MAX_ENTRIES as u32 + 5).rev().map(e).collect();
        entries.swap(0, 3);
        let lines = dump_lines(9, 4000, entries, "velocity=none");
        assert_eq!(lines.len(), MAX_ENTRIES + 1);
        assert!(
            lines[0].contains("binds=4000 candidates=165 logged=160 velocity=none"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("#0/165 bind=0 "), "{}", lines[1]);
        assert!(lines[2].contains("#1/165 bind=1 "), "{}", lines[2]);
    }
}

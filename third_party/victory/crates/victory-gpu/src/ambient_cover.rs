//! What the stand-in for the indirect light (`crate::ambient`) did after
//! each dispatch of a program it follows, per program (hash) and per frame,
//! said every 5 s in one `ambient cover:` line, whatever
//! `VICTORY_AMBIENT_VARIANTS` is.
//!
//! A frame in which a resolve of the diffuse light got no stand-in keeps the
//! title's own light: black indoors with the title's rays finding nothing,
//! while what lamps and the sun light directly (characters' key lights)
//! stays lit. The line lists the numbers of the last such frames, to be laid
//! against the frames `VICTORY_FLICKER_WATCH`/`VICTORY_POP_WATCH` name.
//! A program's first outcome of each kind is said once at once.

use std::sync::Mutex;
use std::time::Instant;

use crate::ambient::Role;

/// What came of one dispatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Outcome {
    /// Raised.
    Applied,
    /// Raised, but the image at the fixed place is not the one at the role's
    /// register (`crate::ambient_places::Places::elsewhere`): the light
    /// itself stayed as the title had it.
    Elsewhere,
    /// Nothing recorded, and why.
    Skipped(&'static str),
}

impl Outcome {
    /// Whether the light the role writes got no stand-in.
    fn dark(self) -> bool {
        self != Outcome::Applied
    }

    fn label(self) -> &'static str {
        match self {
            Outcome::Applied => "applied",
            Outcome::Elsewhere => "raised another image (the fixed place, not the register)",
            Outcome::Skipped(why) => why,
        }
    }
}

/// How many dark frames a line lists.
const LISTED: usize = 12;
/// Seconds between lines.
const EVERY: f32 = 5.0;

struct Program {
    hash: String,
    name: String,
    role: Role,
    /// Since the last line, by outcome.
    counts: Vec<(Outcome, u64)>,
    /// Outcomes said once.
    told: Vec<Outcome>,
}

#[derive(Default)]
struct Cover {
    programs: Vec<Program>,
    /// Since the last line: frames with a resolve dispatch, those with one
    /// left dark, and the last dark frames.
    frames: u64,
    dark: u64,
    listed: Vec<u64>,
    /// The frame last counted, and whether it was counted dark.
    frame: Option<(u64, bool)>,
}

impl Cover {
    /// Counts a dispatch of program `hash` in `frame`; true if it is the
    /// program's first outcome of this kind.
    fn note(
        &mut self,
        hash: &str,
        name: impl FnOnce() -> String,
        role: Role,
        outcome: Outcome,
        frame: u64,
    ) -> bool {
        let at = match self.programs.iter().position(|p| p.hash == hash) {
            Some(at) => at,
            None => {
                self.programs.push(Program {
                    hash: hash.to_owned(),
                    name: name(),
                    role,
                    counts: Vec::new(),
                    told: Vec::new(),
                });
                self.programs.len() - 1
            }
        };
        let program = &mut self.programs[at];
        match program.counts.iter_mut().find(|(o, _)| *o == outcome) {
            Some((_, n)) => *n += 1,
            None => program.counts.push((outcome, 1)),
        }
        let new = !program.told.contains(&outcome);
        if new {
            program.told.push(outcome);
        }
        if role == Role::Resolve {
            let dark = outcome.dark();
            match self.frame {
                Some((f, was_dark)) if f == frame => {
                    if dark && !was_dark {
                        self.dark_frame(frame);
                    }
                }
                _ => {
                    self.frames += 1;
                    if dark {
                        self.dark_frame(frame);
                    }
                    self.frame = Some((frame, dark));
                }
            }
        }
        new
    }

    fn dark_frame(&mut self, frame: u64) {
        self.dark += 1;
        if self.listed.len() == LISTED {
            self.listed.remove(0);
        }
        self.listed.push(frame);
        self.frame = Some((frame, true));
    }

    /// The line for the last `seconds`; the counts start again.
    fn line(&mut self, seconds: f32) -> String {
        let frames: Vec<String> = self.listed.drain(..).map(|f| f.to_string()).collect();
        let mut line = format!(
            "ambient cover: in the last {seconds:.0} s (VICTORY_AMBIENT_VARIANTS={}) {} frames resolved the diffuse light, \
             {} of them with a dispatch left without the stand-in (last: {})",
            u8::from(crate::ambient_places::variants()),
            std::mem::take(&mut self.frames),
            std::mem::take(&mut self.dark),
            if frames.is_empty() {
                "none".to_owned()
            } else {
                frames.join(" ")
            },
        );
        for p in &mut self.programs {
            if p.counts.is_empty() {
                continue;
            }
            let total: u64 = p.counts.iter().map(|(_, n)| n).sum();
            let applied = p
                .counts
                .iter()
                .find(|(o, _)| *o == Outcome::Applied)
                .map_or(0, |(_, n)| *n);
            line += &format!(
                "; {} {} {:?}: {applied} of {total} applied",
                p.hash, p.name, p.role
            );
            for (o, n) in p.counts.iter().filter(|(o, _)| o.dark()) {
                line += &format!(", {n} {}", o.label());
            }
            p.counts.clear();
        }
        line
    }
}

static COVER: Mutex<Option<(Cover, Instant)>> = Mutex::new(None);

/// Counts what came of a dispatch of `shader` (of `role`); says a program's
/// first outcome of each kind at once and every 5 s the line.
pub(crate) fn note(shader: &crate::pipeline::Shader, role: Role, outcome: Outcome) {
    let frame = crate::frame::current();
    let mut cover = COVER.lock().unwrap_or_else(|e| e.into_inner());
    let (cover, since) = cover.get_or_insert_with(|| (Cover::default(), Instant::now()));
    if cover.note(&shader.hash, || shader.camera_name(), role, outcome, frame) {
        let hash = &shader.hash;
        let program = shader.camera_name();
        match outcome {
            Outcome::Applied => tracing::info!(
                hash,
                program,
                ?role,
                "stand-in for the indirect light: applied"
            ),
            o => tracing::warn!(
                hash,
                program,
                ?role,
                why = o.label(),
                frame,
                "no stand-in for the indirect light"
            ),
        }
    }
    let seconds = since.elapsed().as_secs_f32();
    if seconds >= EVERY {
        *since = Instant::now();
        tracing::info!("{}", cover.line(seconds));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name() -> String {
        "CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped".to_owned()
    }

    #[test]
    fn each_outcome_is_new_once_per_program() {
        let mut c = Cover::default();
        assert!(c.note("aa", name, Role::Resolve, Outcome::Applied, 1));
        assert!(!c.note("aa", name, Role::Resolve, Outcome::Applied, 2));
        assert!(c.note("aa", name, Role::Resolve, Outcome::Elsewhere, 3));
        assert!(c.note("bb", name, Role::Resolve, Outcome::Applied, 3));
        assert!(c.note(
            "aa",
            name,
            Role::Resolve,
            Outcome::Skipped("missing image UAV"),
            4
        ));
        assert!(!c.note(
            "aa",
            name,
            Role::Resolve,
            Outcome::Skipped("missing image UAV"),
            5
        ));
    }

    #[test]
    fn a_frame_is_dark_once_whatever_its_dispatches() {
        let mut c = Cover::default();
        // Frame 10: one applied, one skipped: dark. Frame 11: two applied.
        // Frame 12: skipped twice: dark once.
        c.note("aa", name, Role::Resolve, Outcome::Applied, 10);
        c.note(
            "cb",
            name,
            Role::Resolve,
            Outcome::Skipped("another image"),
            10,
        );
        c.note("aa", name, Role::Resolve, Outcome::Applied, 11);
        c.note("aa", name, Role::Resolve, Outcome::Applied, 11);
        c.note("cb", name, Role::Resolve, Outcome::Elsewhere, 12);
        c.note("cb", name, Role::Resolve, Outcome::Elsewhere, 12);
        // The other roles do not count frames.
        c.note(
            "rad",
            name,
            Role::Radiance,
            Outcome::Skipped("not kept"),
            13,
        );
        let line = c.line(5.0);
        assert!(
            line.contains("3 frames resolved the diffuse light, 2 of them"),
            "{line}"
        );
        assert!(line.contains("(last: 10 12)"), "{line}");
        assert!(
            line.contains(
                "; aa CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped Resolve: 3 of 3 applied;"
            ),
            "{line}"
        );
        assert!(line.contains("; cb CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped Resolve: 0 of 3 applied, 1 another image, 2 raised another image"), "{line}");
        assert!(line.contains("rad CS_ScreenProbes_TemporalResolve_CB_a1c765aa_Wrapped Radiance: 0 of 1 applied, 1 not kept"), "{line}");
        // The counts start again; programs without dispatches are left out.
        c.note("aa", name, Role::Resolve, Outcome::Applied, 20);
        let line = c.line(5.0);
        assert!(
            line.contains("1 frames resolved the diffuse light, 0 of them")
                && line.contains("(last: none)"),
            "{line}"
        );
        assert!(
            line.contains("aa ") && !line.contains("cb ") && !line.contains("rad "),
            "{line}"
        );
    }

    #[test]
    fn the_last_dark_frames_are_listed() {
        let mut c = Cover::default();
        for frame in 0..20 {
            c.note("cb", name, Role::Resolve, Outcome::Skipped("x"), frame);
        }
        let line = c.line(5.0);
        assert!(
            line.contains("20 frames resolved the diffuse light, 20 of them"),
            "{line}"
        );
        assert!(
            line.contains("(last: 8 9 10 11 12 13 14 15 16 17 18 19)"),
            "{line}"
        );
    }
}

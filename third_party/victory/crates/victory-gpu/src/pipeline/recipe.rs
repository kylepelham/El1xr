//! (bootfast) Pipeline recipes: with `VICTORY_PIPELINE_RECIPES=1` a run
//! writes down every title pipeline it made a host pipeline for (its shader
//! containers and its graphics state, as the title gave them), into
//! `pipeline-recipes.bin` beside the pipeline library, so that an offline
//! warm-up can make them all again (`victory warm --recipes`, `dev.py
//! prewarm --pipelines`) after a change to the translator or to a switch in
//! its key, before the next run wants them.
//!
//! A pipeline's name in the library holds its translated code
//! (`library::name`), so each such change leaves every library entry
//! unused, and a run's first boot makes some 3,000 pipelines again (on top
//! of the translations, which `dev.py prewarm` does without a device). The
//! recipe keeps what the name is made from before translation: the
//! containers (which are the title's: the file stays in the user's cache
//! folder, beside the library that already holds what they compile to) and
//! the state ([`GraphicsState::encode`]). Recipes only grow: a run adds the
//! ones it made that the file did not have; nothing is written without the
//! switch.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};

use super::Shader;
use super::host::GraphicsState;

/// The recipes' file, beside the library's.
pub(crate) const FILE: &str = "pipeline-recipes.bin";
const MAGIC: &[u8; 8] = b"VPRECIP1";
const STAGES: [&str; 8] = [
    "vertex",
    "pixel",
    "domain",
    "hull",
    "geometry",
    "compute",
    "amplification",
    "mesh",
];

/// Whether runs write recipes down (`VICTORY_PIPELINE_RECIPES=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_RECIPES").is_some_and(|v| v == "1"))
}

/// One pipeline: its key (`make::Pending`'s), its stages (stage, the full
/// hash of the container), and its graphics state's bytes.
#[derive(Clone, Debug, PartialEq)]
struct Recipe {
    key: String,
    stages: Vec<(u8, [u8; 32])>,
    graphics: Option<Arc<[u8]>>,
}

#[derive(Default)]
struct Book {
    /// The containers by full hash.
    containers: HashMap<[u8; 32], Arc<[u8]>>,
    recipes: Vec<Recipe>,
    keys: HashSet<String>,
    /// Recipes added since the file was written (or read).
    dirty: bool,
}

impl Book {
    fn add(&mut self, recipe: Recipe) -> bool {
        if !self.keys.insert(recipe.key.clone()) {
            return false;
        }
        self.recipes.push(recipe);
        self.dirty = true;
        true
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        let used: HashSet<&[u8; 32]> = self
            .recipes
            .iter()
            .flat_map(|r| r.stages.iter().map(|(_, h)| h))
            .collect();
        let mut hashes: Vec<&[u8; 32]> = used
            .into_iter()
            .filter(|h| self.containers.contains_key(*h))
            .collect();
        hashes.sort_unstable();
        out.extend_from_slice(&(hashes.len() as u32).to_le_bytes());
        for h in hashes {
            let bytes = &self.containers[h];
            out.extend_from_slice(h);
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let whole: Vec<&Recipe> = self
            .recipes
            .iter()
            .filter(|r| {
                r.stages
                    .iter()
                    .all(|(_, h)| self.containers.contains_key(h))
            })
            .collect();
        out.extend_from_slice(&(whole.len() as u32).to_le_bytes());
        for r in whole {
            out.extend_from_slice(&(r.key.len() as u32).to_le_bytes());
            out.extend_from_slice(r.key.as_bytes());
            out.push(r.stages.len() as u8);
            for (stage, h) in &r.stages {
                out.push(*stage);
                out.extend_from_slice(h);
            }
            match &r.graphics {
                None => out.extend_from_slice(&u32::MAX.to_le_bytes()),
                Some(g) => {
                    out.extend_from_slice(&(g.len() as u32).to_le_bytes());
                    out.extend_from_slice(g);
                }
            }
        }
        out
    }

    /// The book in `bytes` (`None`: not a recipes file, or cut short, or a
    /// container whose bytes do not hash to its name).
    fn decode(bytes: &[u8]) -> Option<Book> {
        let mut r = Reader(bytes);
        if r.take(8)? != MAGIC {
            return None;
        }
        let mut book = Book::default();
        for _ in 0..r.u32()? {
            let h: [u8; 32] = r.take(32)?.try_into().ok()?;
            let len = r.u32()? as usize;
            let code = r.take(len)?;
            if blake3::hash(code).as_bytes() != &h {
                return None;
            }
            book.containers.insert(h, Arc::from(code));
        }
        for _ in 0..r.u32()? {
            let len = r.u32()? as usize;
            let key = std::str::from_utf8(r.take(len)?).ok()?.to_owned();
            let n = r.take(1)?[0];
            let mut stages = Vec::with_capacity(n as usize);
            for _ in 0..n {
                let stage = r.take(1)?[0];
                if usize::from(stage) >= STAGES.len() {
                    return None;
                }
                stages.push((stage, r.take(32)?.try_into().ok()?));
            }
            let glen = r.u32()?;
            let graphics = if glen == u32::MAX {
                None
            } else {
                Some(Arc::from(r.take(glen as usize)?))
            };
            book.add(Recipe {
                key,
                stages,
                graphics,
            });
        }
        if !r.0.is_empty() {
            return None;
        }
        book.dirty = false;
        Some(book)
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
}

/// This run's book: the file's recipes once read, and those made since.
static BOOK: Mutex<Option<Book>> = Mutex::new(None);

/// The book, with the file in `dir` read in the first time.
fn book<'a>(guard: &'a mut Option<Book>, dir: Option<&Path>) -> &'a mut Book {
    guard.get_or_insert_with(|| {
        dir.and_then(|d| std::fs::read(d.join(FILE)).ok())
            .and_then(|bytes| Book::decode(&bytes))
            .unwrap_or_default()
    })
}

fn stage_index(stage: &str) -> Option<u8> {
    STAGES.iter().position(|s| *s == stage).map(|i| i as u8)
}

/// After a host pipeline was made for `key` (with the switch): its recipe
/// written down, if new.
pub(super) fn made(key: &str, shaders: &[Arc<Shader>], graphics: Option<&GraphicsState>) {
    if !on() {
        return;
    }
    let mut guard = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    let book = book(&mut guard, super::library::dir());
    if book.keys.contains(key) {
        return;
    }
    let mut stages = Vec::with_capacity(shaders.len());
    for s in shaders {
        let Some(stage) = stage_index(s.stage) else {
            return;
        };
        let h = *blake3::hash(&s.container).as_bytes();
        book.containers
            .entry(h)
            .or_insert_with(|| Arc::from(s.container.as_slice()));
        stages.push((stage, h));
    }
    book.add(Recipe {
        key: key.to_owned(),
        stages,
        graphics: graphics.map(|g| Arc::from(g.encode())),
    });
}

/// Writes the book to `dir` if recipes were added since (with the switch),
/// beside the library at its save.
pub(super) fn save(dir: &Path) {
    if !on() {
        return;
    }
    let bytes = {
        let mut guard = BOOK.lock().unwrap_or_else(|e| e.into_inner());
        let book = book(&mut guard, Some(dir));
        if !book.dirty {
            return;
        }
        book.dirty = false;
        (book.encode(), book.recipes.len())
    };
    let path = dir.join(FILE);
    let tmp = path.with_extension("tmp");
    match std::fs::write(&tmp, &bytes.0).and_then(|()| std::fs::rename(&tmp, &path)) {
        Ok(()) => tracing::info!(
            recipes = bytes.1,
            bytes = bytes.0.len(),
            "pipeline recipes saved (VICTORY_PIPELINE_RECIPES)"
        ),
        Err(e) => tracing::warn!(error = %e, "pipeline recipes not saved"),
    }
}

/// What [`warm`] did: recipes read, host pipelines made (or loaded),
/// refused, left out, and recipes not read (a state of another layout) or
/// whose key came out other than recorded.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Warmed {
    pub recipes: usize,
    pub made: usize,
    pub refused: usize,
    pub left_out: usize,
    pub unreadable: usize,
    pub other_key: usize,
    /// Of the host pipelines, those the driver compiled and those the
    /// library already held.
    pub compiled: u64,
    pub loaded: u64,
}

/// Makes the host pipeline of every recipe in the cache's file on `threads`
/// threads, into the library (saved after), for the offline warm-up.
pub fn warm(threads: usize) -> std::io::Result<Warmed> {
    let Some(dir) = super::library::dir() else {
        return Err(std::io::Error::other("no pipeline cache folder"));
    };
    let bytes = std::fs::read(dir.join(FILE))?;
    let book = Book::decode(&bytes)
        .ok_or_else(|| std::io::Error::other("the recipes file does not read"))?;
    let [graphics, leftovers, duplicates, _] = duplicates_in(&book);
    tracing::info!(
        graphics,
        with_leftovers = leftovers,
        same_pipeline_other_key = duplicates,
        canonical = super::pso_canon::on(),
        "pipeline keys of the recipes: ignored state and duplicate keys (VICTORY_PSO_CANON)"
    );
    let before = [super::library::MADE.load(Relaxed), super::library::LOADED.load(Relaxed)];
    let next = AtomicUsize::new(0);
    let counts = [const { AtomicUsize::new(0) }; 5];
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, 64).min(book.recipes.len().max(1)) {
            s.spawn(|| {
                loop {
                    let Some(r) = book.recipes.get(next.fetch_add(1, Relaxed)) else {
                        break;
                    };
                    let graphics = match &r.graphics {
                        None => None,
                        Some(g) => match GraphicsState::decode(g) {
                            Some(g) => Some(Box::new(g)),
                            None => {
                                counts[3].fetch_add(1, Relaxed);
                                continue;
                            }
                        },
                    };
                    let shaders = r
                        .stages
                        .iter()
                        .map(|(stage, h)| {
                            super::shader(STAGES[usize::from(*stage)], &book.containers[h])
                        })
                        .collect();
                    let (key, made) = super::make::warm_recipe(shaders, graphics);
                    if key != r.key {
                        counts[4].fetch_add(1, Relaxed);
                    }
                    let at = match made {
                        Some(true) => 0,
                        Some(false) => 1,
                        None => 2,
                    };
                    counts[at].fetch_add(1, Relaxed);
                }
            });
        }
    });
    super::library::save(true);
    Ok(Warmed {
        recipes: book.recipes.len(),
        made: counts[0].load(Relaxed),
        refused: counts[1].load(Relaxed),
        left_out: counts[2].load(Relaxed),
        unreadable: counts[3].load(Relaxed),
        other_key: counts[4].load(Relaxed),
        compiled: super::library::MADE.load(Relaxed) - before[0],
        loaded: super::library::LOADED.load(Relaxed) - before[1],
    })
}

/// (bootfast) Of the graphics recipes in a recipes file's `bytes`: how
/// many, how many hold ignored state off its defaults, how many are the
/// same pipeline as an earlier recipe under another key, and how many
/// states did not read (`pso_canon::recipe_duplicates`; `None`: not a
/// recipes file). `warm` logs them.
#[cfg(test)]
fn duplicates(bytes: &[u8]) -> Option<[usize; 4]> {
    Some(duplicates_in(&Book::decode(bytes)?))
}

fn duplicates_in(book: &Book) -> [usize; 4] {
    super::pso_canon::recipe_duplicates(
        book.recipes
            .iter()
            .filter_map(|r| r.graphics.as_deref().map(|g| (&r.stages[..], g))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (portscout, c #20) Offline count of a run's recipes file: how many
    /// graphics pipelines `VICTORY_PSO_CANON` would key as one with another.
    /// `EL1XR_RECIPES_FILE=<pipeline-recipes.bin> cargo test -p victory-gpu
    /// --lib recipes_file_duplicates -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn recipes_file_duplicates() {
        let path = std::env::var("EL1XR_RECIPES_FILE").expect("EL1XR_RECIPES_FILE");
        let book = Book::decode(&std::fs::read(path).expect("read")).expect("a recipes file");
        let [graphics, leftovers, same, unreadable] = duplicates_in(&book);
        let mut by_stages: HashMap<Vec<(u8, [u8; 32])>, HashSet<String>> = HashMap::new();
        let mut depth_off: HashMap<Vec<(u8, [u8; 32])>, HashSet<String>> = HashMap::new();
        for r in &book.recipes {
            let Some(g) = r.graphics.as_deref().and_then(GraphicsState::decode) else {
                continue;
            };
            let mut g = g;
            super::super::pso_canon::canonical(&mut g);
            by_stages.entry(r.stages.clone()).or_default().insert(g.describe());
            if !g.depth.DepthEnable.as_bool() {
                g.depth.DepthFunc = GraphicsState::default().depth.DepthFunc;
                g.depth.DepthWriteMask = GraphicsState::default().depth.DepthWriteMask;
            }
            depth_off.entry(r.stages.clone()).or_default().insert(g.describe());
        }
        let canon: usize = by_stages.values().map(HashSet::len).sum();
        let deeper: usize = depth_off.values().map(HashSet::len).sum();
        println!(
            "recipes={} graphics={graphics} with_leftovers={leftovers} same_pipeline_other_key={same} unreadable={unreadable} \
             stage_sets={} canonical_states={canon} canonical_plus_depth_off={deeper}",
            book.recipes.len(),
            by_stages.len(),
        );
    }

    #[test]
    fn a_book_of_plain_states_has_no_duplicates() {
        assert_eq!(duplicates(&book().encode()), Some([1, 0, 0, 0]));
        assert_eq!(duplicates(b"VPRECIP0"), None);
    }


    fn book() -> Book {
        let mut b = Book::default();
        let vs: Arc<[u8]> = Arc::from(&b"vertex container"[..]);
        let ps: Arc<[u8]> = Arc::from(&b"pixel container bytes"[..]);
        let hv = *blake3::hash(&vs).as_bytes();
        let hp = *blake3::hash(&ps).as_bytes();
        b.containers.insert(hv, vs);
        b.containers.insert(hp, ps);
        assert!(b.add(Recipe {
            key: "k1".into(),
            stages: vec![(0, hv), (1, hp)],
            graphics: Some(Arc::from(GraphicsState::default().encode())),
        }));
        assert!(b.add(Recipe {
            key: "k2".into(),
            stages: vec![(5, hp)],
            graphics: None,
        }));
        assert!(!b.add(Recipe {
            key: "k2".into(),
            stages: vec![],
            graphics: None,
        }));
        b
    }

    #[test]
    fn a_book_reads_back_as_written() {
        let b = book();
        let bytes = b.encode();
        let back = Book::decode(&bytes).expect("reads");
        assert_eq!(back.recipes, b.recipes);
        assert_eq!(back.containers.len(), 2);
        assert!(!back.dirty);
        assert_eq!(back.encode(), bytes);
    }

    #[test]
    fn a_damaged_book_is_not_read() {
        let bytes = book().encode();
        assert!(Book::decode(&bytes[..bytes.len() - 1]).is_none());
        let mut flipped = bytes.clone();
        // A byte of the first container: it no longer hashes to its name.
        flipped[8 + 4 + 32 + 4] ^= 1;
        assert!(Book::decode(&flipped).is_none());
        assert!(Book::decode(b"VPRECIP0").is_none());
        assert_eq!(
            Book::decode(&Book::default().encode()).map(|b| b.recipes.len()),
            Some(0)
        );
    }

    /// A recipe whose container is missing is not written: its pipeline
    /// could not be made again.
    #[test]
    fn a_recipe_without_its_container_is_left_out() {
        let mut b = book();
        b.add(Recipe {
            key: "k3".into(),
            stages: vec![(0, [7; 32])],
            graphics: None,
        });
        let back = Book::decode(&b.encode()).unwrap();
        assert_eq!(back.recipes.len(), 2);
    }

    #[test]
    fn stages_are_named() {
        assert_eq!(stage_index("vertex"), Some(0));
        assert_eq!(stage_index("mesh"), Some(7));
        assert_eq!(stage_index("other"), None);
    }
}

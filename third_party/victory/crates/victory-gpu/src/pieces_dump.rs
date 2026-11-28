//! `VICTORY_PIPELINE_DUMP=<folder>`: writes descriptions device method 0x1c8
//! gets, and every piece they name, to files in that folder, for the tests
//! that read them back (`pipeline::pieces::tests::captured_states_map`,
//! `victory_shader::pieces::tests::captured_descriptions_parse_and_translate`).
//! The files are the title's: the folder stays outside the repository.
//!
//! What it is for: 12,062 descriptions a run name a stage in the first extra
//! slot (+0x68: code 0xa00 bytes, metadata 0x2d0 in the first one logged),
//! never with a vertex stage, and no pipeline is made from them yet; what that
//! stage is (its metadata kind, its code, the packet registers that go with
//! it) is read from these files. At most [`EXTRA`] such descriptions and
//! [`PLAIN`] others are written, with `VICTORY_PIPELINE_PIECES` on or off.
//!
//! Files: `1c8-<n>-desc.bin`, the 0x108-byte description; `1c8-<n>-blob<k>-<len>.bin`,
//! pair `k` (description words 1 + 2k and 2 + 2k: an address and a length).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The description's bytes: the root signature, then 16 address/length pairs.
const DESC: usize = 0x108;
const WORDS: usize = DESC / 8;
/// The first word after the pixel and vertex stages (+0x68).
const FIRST_EXTRA: usize = 0x68 / 8;
/// The longest piece written (the longest code seen was 123 KiB).
const MOST: u64 = 4 << 20;
/// Descriptions written with a word in an extra slot, and without.
const EXTRA: u32 = 300;
const PLAIN: u32 = 300;

/// The folder, created; `None` when the variable is unset or empty.
fn folder() -> Option<&'static PathBuf> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let folder = PathBuf::from(std::env::var_os("VICTORY_PIPELINE_DUMP").filter(|v| !v.is_empty())?);
            match std::fs::create_dir_all(&folder) {
                Ok(()) => {
                    tracing::info!(folder = %folder.display(), "0x1c8 descriptions are written (VICTORY_PIPELINE_DUMP)");
                    Some(folder)
                }
                Err(e) => {
                    tracing::warn!(folder = %folder.display(), error = %e, "VICTORY_PIPELINE_DUMP: the folder is not made; nothing is written");
                    None
                }
            }
        })
        .as_ref()
}

/// How many of each kind were taken so far: (with an extra slot, without).
#[derive(Default)]
struct Taken {
    extra: u32,
    plain: u32,
}

impl Taken {
    /// The number of the next description of its kind, if its kind is not
    /// full yet.
    fn take(&mut self, extra: bool) -> Option<u32> {
        let n = self.extra + self.plain;
        let count = if extra {
            &mut self.extra
        } else {
            &mut self.plain
        };
        if *count >= if extra { EXTRA } else { PLAIN } {
            return None;
        }
        *count += 1;
        Some(n)
    }
}

/// Whether the description `words` names something past its vertex stage.
fn has_extra(words: &[u64; WORDS]) -> bool {
    words[FIRST_EXTRA..].iter().any(|&w| w != 0)
}

/// The pairs of `words` that look like pieces: (k, address, length).
fn pieces(words: &[u64; WORDS]) -> Vec<(usize, u64, u64)> {
    (0..(WORDS - 1) / 2)
        .map(|k| (k, words[1 + 2 * k], words[2 + 2 * k]))
        .filter(|&(_, address, len)| address >= 0x1_0000 && len != 0 && len <= MOST)
        .collect()
}

/// Device method 0x1c8 got `desc`: written if the variable names a folder
/// and its kind still has room. On the title's thread; costs nothing unset.
pub(crate) fn call(desc: *const u8) {
    let Some(folder) = folder() else {
        return;
    };
    let mut bytes = [0u8; DESC];
    if desc.is_null() || crate::read_there::read_there(desc, &mut bytes) < FIRST_EXTRA * 8 {
        return;
    }
    let mut words = [0u64; WORDS];
    for (w, b) in words.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *w = u64::from_le_bytes(*b);
    }
    static TAKEN: Mutex<Taken> = Mutex::new(Taken { extra: 0, plain: 0 });
    let Some(n) = TAKEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(has_extra(&words))
    else {
        return;
    };
    let write = |name: String, data: &[u8]| {
        if let Err(e) = std::fs::write(folder.join(&name), data) {
            tracing::warn!(file = %name, error = %e, "VICTORY_PIPELINE_DUMP: not written");
        }
    };
    write(format!("1c8-{n:03}-desc.bin"), &bytes);
    for (k, address, len) in pieces(&words) {
        let mut piece = vec![0u8; len as usize];
        if crate::read_there::read_there(address as *const u8, &mut piece) == piece.len() {
            write(format!("1c8-{n:03}-blob{k:02}-{len:x}.bin"), &piece);
        }
    }
    if n + 1 == EXTRA + PLAIN {
        tracing::info!(
            descriptions = n + 1,
            "VICTORY_PIPELINE_DUMP: all descriptions written"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first description logged with its +0x68 slot set (run b5h).
    fn logged() -> [u64; WORDS] {
        let mut w = [0u64; WORDS];
        w[0] = 0x2ba_0000_1000;
        w[1..13].copy_from_slice(&[
            0xd065f362c,
            0x1e8,
            0xd065f3814,
            0x50,
            0xd55a8ee00,
            0x3380,
            0xd065f3864,
            0x830,
            0,
            0,
            0,
            0,
        ]);
        w[13..17].copy_from_slice(&[0xd55a92200, 0xa00, 0xd065f4094, 0x2d0]);
        w
    }

    #[test]
    fn an_extra_stage_is_written_with_the_rest() {
        let w = logged();
        assert!(has_extra(&w));
        let ks: Vec<usize> = pieces(&w).iter().map(|p| p.0).collect();
        // packet, state, pixel code and metadata, then the +0x68 code and metadata
        assert_eq!(ks, [0, 1, 2, 3, 6, 7]);
        assert_eq!(pieces(&w)[4], (6, 0xd55a92200, 0xa00));
        let mut plain = w;
        plain[13..17].fill(0);
        assert!(!has_extra(&plain));
        // a word that is no address, or a length past any piece, is not read
        plain[9..11].copy_from_slice(&[0x40, 0x100]);
        plain[11..13].copy_from_slice(&[0xd065f4094, MOST + 1]);
        assert_eq!(pieces(&plain).len(), 4);
    }

    #[test]
    fn each_kind_is_capped_and_numbers_run_on() {
        let mut t = Taken::default();
        assert_eq!(t.take(true), Some(0));
        assert_eq!(t.take(false), Some(1));
        for _ in 1..EXTRA {
            assert!(t.take(true).is_some());
        }
        assert_eq!(t.take(true), None);
        // the other kind still has room, numbered after every one taken
        assert_eq!(t.take(false), Some(EXTRA + 1));
        for _ in 2..PLAIN {
            assert!(t.take(false).is_some());
        }
        assert_eq!(t.take(false), None);
        assert!(EXTRA + PLAIN <= 1000, "names keep three digits");
    }
}

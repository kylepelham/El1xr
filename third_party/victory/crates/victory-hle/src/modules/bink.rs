//! (phonescreen, #115) `VICTORY_BINK_OPEN_LOG=1` (read when the title's
//! imports are bound, off by default): the title's calls into its own video
//! library (`Bink2Scarlett.dll`, a title DLL) to open a video go through us
//! and each open is logged (`a video opened`: name, size, frames, rate,
//! length, the record's address, seconds since binding), so whether the
//! phone screen is a Bink video, and which file, can be read off the log.
//! Nothing the library returns is changed.
//!
//! Hand port of bolado-dev/mandy-port 3648d14a + 4f05b594 (the open
//! interposer and the record's words); their `VICTORY_SKIP_INTRO` trimming
//! is not taken (ours skips the intro through `cmdline`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The library's own `BinkOpenWithOptions`.
static OPEN: AtomicUsize = AtomicUsize::new(0);

/// When the title's imports were bound.
static BOUND: OnceLock<std::time::Instant> = OnceLock::new();

/// `HBINK BinkOpenWithOptions(const char* name, const BINK_OPEN_OPTIONS*,
/// U32 flags)`.
type OpenWithOptions = unsafe extern "system" fn(*const u8, *const u8, u32) -> *mut u32;

/// The library this module stands between.
const LIBRARY: &str = "bink2scarlett.dll";

fn asked() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| std::env::var("VICTORY_BINK_OPEN_LOG").is_ok_and(|v| v.trim() == "1"))
}

/// What the title's import `dll!symbol`, bound to the title DLL's export at
/// `real`, is bound to instead: ours with the switch on, or `None` (the
/// export itself).
pub fn interpose(dll: &str, symbol: &str, real: usize) -> Option<usize> {
    interpose_if(asked(), dll, symbol, real)
}

fn interpose_if(on: bool, dll: &str, symbol: &str, real: usize) -> Option<usize> {
    if !on || !dll.eq_ignore_ascii_case(LIBRARY) || real == 0 || symbol != "BinkOpenWithOptions" {
        return None;
    }
    OPEN.store(real, Ordering::Relaxed);
    BOUND.get_or_init(std::time::Instant::now);
    tracing::info!("the title's video opens go through us (VICTORY_BINK_OPEN_LOG=1)");
    Some(open_with_options as OpenWithOptions as usize)
}

/// Words of the library's `BINK` record read: width, height, frames, the
/// frame to be shown, the last frame decoded (all ones before the first),
/// and the frame rate as a fraction.
const HEADER_WORDS: usize = 7;

/// The start of the library's `BINK` record ([`HEADER_WORDS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    width: u32,
    height: u32,
    frames: u32,
    frame: u32,
    rate: u32,
    rate_div: u32,
}

impl Header {
    /// From the record's first words; `None` if they are not a video's.
    fn read(words: [u32; HEADER_WORDS]) -> Option<Header> {
        let [width, height, frames, frame, _decoded, rate, rate_div] = words;
        let plausible = (1..=16384).contains(&width)
            && (1..=16384).contains(&height)
            && frames >= 1
            && frame <= frames
            && rate > 0
            && rate_div > 0;
        plausible.then_some(Header {
            width,
            height,
            frames,
            frame,
            rate,
            rate_div,
        })
    }

    /// The video's length in seconds.
    fn seconds(&self) -> f64 {
        f64::from(self.frames) * f64::from(self.rate_div) / f64::from(self.rate)
    }
}

/// Up to 200 bytes of the name the title opens, if it is one (printable
/// text up to its end); a flag may make it a handle or memory instead.
fn name(at: *const u8, flags: u32) -> String {
    // The library's flags for a file handle and for memory in place of a
    // name.
    const NOT_A_NAME: u32 = 0x0080_0000 | 0x0400_0000;
    if (at as usize) < 0x1_0000 || flags & NOT_A_NAME != 0 {
        return format!("({at:?}, flags {flags:#x})");
    }
    let mut text = String::new();
    for i in 0..200 {
        // SAFETY: a name the title passes: readable up to its end, which
        // the loop stops at.
        let byte = unsafe { at.add(i).read() };
        match byte {
            0 => return text,
            0x20..=0x7e => text.push(byte as char),
            _ => return format!("({at:?}, flags {flags:#x})"),
        }
    }
    text
}

unsafe extern "system" fn open_with_options(
    name_at: *const u8,
    options: *const u8,
    flags: u32,
) -> *mut u32 {
    let real = OPEN.load(Ordering::Relaxed);
    if real == 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: the library's export, of this signature.
    let open: OpenWithOptions = unsafe { std::mem::transmute(real) };
    // SAFETY: the title's arguments, passed on as they are.
    let bink = unsafe { open(name_at, options, flags) };
    let name = name(name_at, flags);
    if bink.is_null() {
        tracing::info!(
            name,
            flags = format_args!("{flags:#x}"),
            "a video did not open"
        );
        return bink;
    }
    // SAFETY: the library's record, which starts with these words.
    let words = unsafe { std::array::from_fn(|i| bink.add(i).read()) };
    let since = BOUND
        .get()
        .map_or(std::time::Duration::ZERO, |at| at.elapsed());
    let Some(header) = Header::read(words) else {
        tracing::warn!(
            name,
            ?words,
            "a video opened whose record does not read as one"
        );
        return bink;
    };
    tracing::info!(
        name,
        width = header.width,
        height = header.height,
        frames = header.frames,
        frame = header.frame,
        fps = f64::from(header.rate) / f64::from(header.rate_div),
        seconds = format_args!("{:.1}", header.seconds()),
        flags = format_args!("{flags:#x}"),
        record = format_args!("{bink:?}"),
        after_bind_s = format_args!("{:.1}", since.as_secs_f64()),
        "a video opened"
    );
    bink
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_videos_record_reads_as_its_size_frames_and_rate() {
        // The phone's size as the Bink pass draws it (batch 5 part E).
        let phone = Header::read([736, 1280, 300, 1, u32::MAX, 30, 1]).expect("a video");
        assert_eq!((phone.width, phone.height, phone.frames), (736, 1280, 300));
        assert!((phone.seconds() - 10.0).abs() < 0.01);
        let ntsc = Header::read([1920, 1080, 1800, 1, u32::MAX, 30000, 1001]).expect("a video");
        assert!((ntsc.seconds() - 60.06).abs() < 0.01);
        // Not a video's record.
        assert_eq!(Header::read([0, 2160, 1800, 1, 0, 30, 1]), None);
        assert_eq!(Header::read([3840, 2160, 0, 0, 0, 30, 1]), None);
        assert_eq!(Header::read([3840, 2160, 10, 11, 0, 30, 1]), None);
        assert_eq!(Header::read([3840, 2160, 10, 1, 0, 0, 1]), None);
    }

    #[test]
    fn names_are_read_up_to_their_end_or_left_as_addresses() {
        assert_eq!(
            name(c"movies:/phone.bk2".as_ptr().cast(), 0),
            "movies:/phone.bk2"
        );
        assert_eq!(name(0x1234 as *const u8, 0), "(0x1234, flags 0x0)");
        let handle = name(c"x".as_ptr().cast(), 0x0080_0000);
        assert!(handle.starts_with('('), "{handle}");
        let bytes = [0x41u8, 0x01, 0];
        assert!(name(bytes.as_ptr(), 0).starts_with('('));
    }

    /// Only the library's open is put between, and only with the switch.
    #[test]
    fn only_the_librarys_open_is_put_between_with_the_switch() {
        assert_eq!(
            interpose_if(false, "Bink2Scarlett.dll", "BinkOpenWithOptions", 0x1000),
            None
        );
        assert_eq!(
            interpose_if(true, "kernel32.dll", "BinkOpenWithOptions", 0x1000),
            None
        );
        assert_eq!(
            interpose_if(true, "Bink2Scarlett.dll", "BinkClose", 0x1000),
            None
        );
        assert_eq!(
            interpose_if(true, "Bink2Scarlett.dll", "BinkOpenWithOptions", 0),
            None
        );
        assert_eq!(
            interpose_if(true, "Bink2Scarlett.dll", "BinkOpenWithOptions", 0x1000),
            Some(open_with_options as OpenWithOptions as usize)
        );
    }
}

use std::path::Path;

use anyhow::{Context, Result};

use crate::{Console, Unimplemented};

#[cfg(windows)]
pub fn run(
    exe: &Path,
    unimplemented: Unimplemented,
    strict: bool,
    execute: bool,
    console: Console,
    title_args: &[String],
) -> Result<()> {
    use std::sync::Arc;
    use victory_hle::{HleResolver, HostPolicy};
    use victory_loader::{BindReport, UnimplementedPolicy, WithTitleDlls};

    // A title that relaunched itself: the old process must be gone first.
    victory_hle::modules::lifecycle::wait_for_previous();
    let cfg = victory_core::Config::load()?;
    // `VICTORY_FRAMETIME_DUMP=1`: write beside the rest of Victory's cache.
    if std::env::var_os("VICTORY_FRAMETIME_DUMP").is_some_and(|v| v.to_str() == Some("1")) {
        victory_gpu::frametime::use_dump_path(cfg.cache_dir.join("frametime.csv"));
    }
    let original = std::fs::read(exe).with_context(|| format!("reading {}", exe.display()))?;
    let prepared = victory_loader::prepare(&original)?;
    let hash = prepared.original_hash.to_hex();
    let label = match victory_game::identify(&hash) {
        Some(build) => format!("{} ({hash})", build.label),
        None => format!("unknown ({hash})"),
    };
    println!("build          {label}");
    victory_gpu::frametime::use_build_label(label);

    let images = cfg.cache_subdir("images")?;
    victory_hle::modules::d3d12x::use_shader_cache(cfg.cache_subdir("shaders")?);
    let image = victory_loader::load(&prepared, &images)?;
    // The program is mapped from the copy `load` wrote: the file's bytes
    // and their patched copy are not read again, and were held for the
    // whole run (two blocks of 114.7 MB each for the title, in
    // physical memory: run 65's memory map).
    drop(prepared);
    drop(original);
    victory_hle::runtime::set_main_image(image.base());
    victory_hle::runtime::set_image_path(image.base(), exe);
    victory_hle::runtime::set_image_size(image.base(), image.info().size_of_image as usize);
    // SAFETY: the title image stays mapped for the whole run.
    unsafe { victory_gpu::modmenu::init(image.base(), image.info().size_of_image as usize) };
    let effective_args = victory_hle::modules::cmdline::set_title_command_line(exe, title_args);
    if !effective_args.is_empty() {
        println!("title args     {}", effective_args.join(" "));
    }
    println!(
        "mapped at      {:#x} (preferred {:#x})",
        image.base(),
        image.info().image_base
    );
    let resolver = HleResolver::new(if strict {
        HostPolicy::none()
    } else {
        HostPolicy::default()
    });
    let wanted = |dll: &str| title_dll(&resolver.host, dll);
    let title_dir = exe.parent().unwrap_or(Path::new("."));
    // A missing manifest leaves id 0 and no storage quota.
    let manifest =
        std::fs::read_to_string(title_dir.join("MicrosoftGame.config")).unwrap_or_default();
    let title = victory_hle::runtime::Title::from_manifest(&manifest);
    let storage = cfg.data_subdir(&format!("{:08X}/storage", title.id))?;
    println!(
        "title          {:08X}, storage {} ({} MB)",
        title.id,
        storage.display(),
        title.storage_bytes >> 20
    );
    // The console's fixed drives: the package as `G:`, temporary files `T:`.
    victory_hle::modules::files::mount_at('G', title_dir);
    let temp = cfg.data_subdir(&format!("{:08X}/temp", title.id))?;
    victory_hle::modules::files::mount_at('T', &temp);
    victory_hle::runtime::set_title(victory_hle::runtime::Title { storage, ..title });
    victory_hle::console::set(match console {
        Console::SeriesX => victory_hle::console::Console::SeriesX,
        Console::SeriesS => victory_hle::console::Console::SeriesS,
    });
    println!("console        {}", victory_hle::console::current().name());
    let dlls = victory_loader::load_title_dlls(image.info(), title_dir, &images, &wanted)?;
    for (name, dll) in &dlls {
        if name.eq_ignore_ascii_case(victory_hle::modules::ffx::LOADER)
            && let Some(real) = dll.export("ffxDispatch")
        {
            victory_hle::modules::ffx::set_real_dispatch(real);
        }
        if name.eq_ignore_ascii_case(victory_hle::modules::ffx::LOADER)
            && let Some(real) = dll.export("ffxQuery")
        {
            victory_hle::modules::ffx::set_real_query(real);
        }
        println!("title dll      {name} at {:#x}", dll.base());
        victory_hle::runtime::set_image_path(dll.base(), &title_dir.join(name));
        victory_hle::runtime::set_image_size(dll.base(), dll.info().size_of_image as usize);
    }

    let policy = match unimplemented {
        Unimplemented::Trap => UnimplementedPolicy::Trap,
        Unimplemented::Zero => UnimplementedPolicy::ReturnZero,
    };
    let with_dlls = WithTitleDlls {
        inner: &resolver,
        dlls: &dlls,
    };
    let show = |name: &str, report: BindReport| {
        println!(
            "bound          {name}: {} hle, {} host, {} title, {} missing",
            report.hle,
            report.host,
            report.title,
            report.missing.len()
        );
        for m in &report.missing {
            println!("  missing      {}!{}", m.dll, m.symbol);
        }
    };
    for (name, dll) in &dlls {
        show(name, dll.bind_imports(&with_dlls, policy)?);
    }
    let exe_name = exe.file_name().unwrap_or_default().to_string_lossy();
    // `VICTORY_UPSCALE=stretch|dlss` (opt-in on Series S, or Series X with
    // `VICTORY_UPSCALER=7`): `ffxDispatch` and `ffxQuery` are intercepted by
    // `victory_hle::modules::ffx`; the rest of the DLL stays the title's.
    // Unset (default): untouched, preserving default native 4K Series X.
    let upscale_mode = victory_hle::modules::ffx::asked_mode();
    let series_s = victory_hle::console::current() == victory_hle::console::Console::SeriesS;
    let take = victory_hle::modules::ffx::should_take_dispatch(
        series_s,
        victory_loader::resolution::asked_upscaler(),
        victory_hle::modules::fidelityfx::enabled(),
        upscale_mode,
    );
    // VICTORY_CAMERA_CAPTURE=1: observe the dispatch for the camera record.
    let observe = !take
        && victory_hle::modules::ffx::should_observe_dispatch(
            series_s,
            victory_loader::resolution::asked_upscaler(),
            victory_hle::modules::fidelityfx::enabled(),
            victory_hle::modules::ffx::camera_capture(),
        );
    if observe {
        println!("upscale        the title's ffxDispatch is observed (VICTORY_CAMERA_CAPTURE=1)");
    }
    let take = take || observe;
    if take && upscale_mode.is_some() {
        println!(
            "upscale        the title's ffxDispatch is ours (VICTORY_UPSCALE={})",
            upscale_mode.unwrap().as_str()
        );
    } else if !take && upscale_mode.is_some() && !victory_hle::modules::fidelityfx::enabled() {
        println!(
            "upscale        VICTORY_UPSCALE ignored on Series X without VICTORY_UPSCALER=7 (keeping native upscaler)"
        );
    }
    show(&exe_name, image.bind_imports(&TakeDispatch(&with_dlls, take), policy)?);
    victory_hle::bvh_hook::install(&image)?; // VICTORY_BVH_HOOK=1 (LNX); off: nothing
    // The output size and render scale (VICTORY_OUTPUT, VICTORY_RENDER_SCALE,
    // VICTORY_UPSCALER; each off without it).
    #[cfg(windows)]
    if execute {
        let size = image.info().size_of_image as usize;
        // SAFETY: the image stays mapped and nothing of it has run yet.
        unsafe { victory_loader::resolution::apply_asked(image.base(), size) };
        // The settings that can then change live (the in-game menu's).
        crate::live_hooks::register(take && upscale_mode.is_some());
        // VICTORY_UPSCALE_LIVE_TEST (off without it): live upscale changes on a timer.
        victory_gpu::upscale_live::spawn_test_schedule(|s| {
            if let Err(why) = victory_loader::resolution::request_ffx_scale(s) {
                tracing::warn!(
                    scale = s,
                    why,
                    "VICTORY_UPSCALE_LIVE_TEST: render scale not changed"
                );
            }
        });
    }
    // VICTORY_LOAD_FPS=<rate> (off without it): the title runs at that rate
    // while it loads and at its own 30 after. At 25 or more frames a second
    // it baked its characters' skin before the textures were there, and
    // every head was without it for the run; at 5 they were there. The end
    // of loading is victory-gpu's (no bake for ten seconds).
    // (avatarmanz/VIctory-7900XTX PR #449)
    #[cfg(windows)]
    if execute && let Some(load) = victory_loader::frame_rate::asked_load_rate() {
        use victory_loader::frame_rate::{PLAYING_RATE, Patched, SITES, apply, change};
        let size = image.info().size_of_image as usize;
        let base = image.base();
        // SAFETY: the image stays mapped and nothing of it has run yet.
        let done = unsafe { apply(base, size, load, &SITES) };
        let patched = done.iter().filter(|d| **d == Patched::Done).count();
        println!(
            "frame rate: the title's rate {load} while it loads, {PLAYING_RATE} after ({patched} of {} reads of its setting patched)",
            SITES.len()
        );
        if patched > 0 {
            std::thread::spawn(move || {
                while !victory_core::LOADING_OVER.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                // SAFETY: the title's image, mapped for the run.
                let changed = unsafe { change(base, size, load, PLAYING_RATE, &SITES) };
                println!("frame rate: loading over, the title's rate {PLAYING_RATE} from now ({changed} sites)");
            });
        }
    }
    if !execute {
        println!("not executing (pass --execute to start the title)");
        return Ok(());
    }
    victory_loader::report_crashes();
    victory_loader::log_crashes_to(temp.with_file_name("crashes.log"));
    victory_loader::sample_profile::start();
    victory_hle::runtime::use_console_timers();
    victory_hle::modules::audio::mute::start();
    struct MuteStop;
    impl Drop for MuteStop {
        fn drop(&mut self) {
            victory_hle::modules::audio::mute::stop();
        }
    }
    let _mute_stop = MuteStop;
    #[cfg(feature = "diagnostics")]
    diagnostics(&image, &cfg);
    let late = LateDlls {
        host: resolver.host.clone(),
        title_dir: title_dir.to_owned(),
        cache_dir: images,
        policy,
        mapped: std::sync::Mutex::new(dlls.clone()),
        lock: LoaderLock::default(),
    };
    victory_hle::runtime::set_title_dll_loader(Box::new(move |file| {
        late.load(file, show).map_err(|e| format!("{e:#}"))
    }));
    // SAFETY: imports are bound; from here on we run title code.
    let main = unsafe { Arc::new(image).spawn_main(dlls)? };
    let code = main
        .join()
        .map_err(|_| anyhow::anyhow!("title main thread panicked"))??;
    println!("title exited with {code}");
    Ok(())
}

/// The debugging aids (README): scheduled stack dumps, watched values and
/// memory saved on a crash.
#[cfg(all(windows, feature = "diagnostics"))]
fn diagnostics(image: &victory_loader::LoadedImage, cfg: &victory_core::Config) {
    victory_loader::dump_stacks_on_schedule();
    victory_hle::modules::audio::capture::start();
    watch(image.base(), image.info().size_of_image as usize);
    // Debugging aid: `VICTORY_CRASH_SAVE=<rva>:<len>,...` (hex) writes those
    // title ranges to `crash-<rva>.bin` in the cache dir on a crash.
    let size = image.info().size_of_image as usize;
    let ranges = std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default();
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    victory_loader::save_on_crash(
        ranges
            .split(',')
            .filter_map(|r| r.split_once(':'))
            .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
            .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
            .map(|(rva, len)| {
                let file = cfg.cache_dir.join(format!("crash-{rva:x}.bin"));
                (image.base() + rva, len, file)
            })
            .collect(),
    );
}

/// Debugging aid: `VICTORY_WATCH=<rva>,...` (hex) logs the u32 at each
/// title+rva whenever it changes, polled every 5 ms (an engine's state, say).
#[cfg(all(windows, feature = "diagnostics"))]
fn watch(base: usize, size: usize) {
    let Ok(list) = std::env::var("VICTORY_WATCH") else {
        return;
    };
    let rvas: Vec<usize> = list
        .split(',')
        .filter_map(|r| usize::from_str_radix(r.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&rva| rva + 4 <= size)
        .collect();
    std::thread::spawn(move || {
        let mut seen = vec![None; rvas.len()];
        loop {
            for (&rva, seen) in rvas.iter().zip(&mut seen) {
                // SAFETY: four bytes inside the mapped image, checked above.
                let value = unsafe { ((base + rva) as *const u32).read_volatile() };
                if *seen != Some(value) {
                    eprintln!("VIctory: title+{rva:#x} = {value:#x}");
                    *seen = Some(value);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });
}

/// Intercepts only `amd_fidelityfx_loader_xbox.dll!ffxDispatch` and
/// `ffxQuery` (its failed answers, `victory_hle::modules::ffx::query`) when
/// opted in via `VICTORY_UPSCALE=stretch|dlss`; forwards all other imports
/// unchanged.
#[cfg(windows)]
struct TakeDispatch<'a>(&'a dyn victory_loader::ImportResolver, bool);

#[cfg(windows)]
impl victory_loader::ImportResolver for TakeDispatch<'_> {
    fn resolve(&self, dll: &str, symbol: &victory_pe::ImportSymbol) -> victory_loader::Resolution {
        let ours = match symbol {
            victory_pe::ImportSymbol::Name { name, .. }
                if self.1 && dll.eq_ignore_ascii_case(victory_hle::modules::ffx::LOADER) =>
            {
                match name.as_str() {
                    "ffxDispatch" => Some(victory_hle::modules::ffx::dispatch_address()),
                    "ffxQuery" => Some(victory_hle::modules::ffx::query_address()),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(address) = ours {
            return victory_loader::Resolution::Found {
                address,
                provenance: victory_loader::Provenance::Hle,
            };
        }
        self.0.resolve(dll, symbol)
    }
}

/// DLLs the host or our runtime provide are never taken from the title.
#[cfg(windows)]
fn title_dll(host: &victory_hle::HostPolicy, dll: &str) -> bool {
    !host.allows(dll) && !victory_hle::registry().iter().any(|m| m.serves(dll))
}

/// Title DLLs the title loads itself (`LoadLibrary`) that nothing imports,
/// e.g. The title's upscaler behind its FidelityFX loader.
#[cfg(windows)]
struct LateDlls {
    host: victory_hle::HostPolicy,
    title_dir: std::path::PathBuf,
    cache_dir: std::path::PathBuf,
    policy: victory_loader::UnimplementedPolicy,
    /// Every title DLL mapped so far, to bind new ones against.
    mapped: std::sync::Mutex<Vec<(String, victory_loader::LoadedImage)>>,
    lock: LoaderLock,
}

#[cfg(windows)]
impl LateDlls {
    /// Every `LoadLibrary` of the title comes here first. Returns a title
    /// image called `file`, mapping it and the title DLLs it needs first,
    /// binding them and running their `DllMain` on the calling thread, as
    /// the Windows loader would; `None` if `file` is no title image.
    fn load(
        &self,
        file: &str,
        show: impl Fn(&str, victory_loader::BindReport),
    ) -> Result<Option<usize>> {
        use victory_hle::runtime::{forget_image, image_named, set_image_path};
        // Waits while another thread initialises a DLL; a `DllMain` that
        // loads another DLL re-enters.
        self.lock.hold(|| {
            if let Some(base) = image_named(file) {
                return Ok(Some(base));
            }
            let new = {
                let mut mapped = self.mapped.lock().unwrap_or_else(|e| e.into_inner());
                let wanted = |dll: &str| title_dll(&self.host, dll) && image_named(dll).is_none();
                let new = victory_loader::load_title_dll(
                    file,
                    &self.title_dir,
                    &self.cache_dir,
                    &wanted,
                )?;
                let hle = victory_hle::HleResolver::new(self.host.clone());
                let old = victory_loader::WithTitleDlls {
                    inner: &hle,
                    dlls: &mapped,
                };
                let resolver = victory_loader::WithTitleDlls {
                    inner: &old,
                    dlls: &new,
                };
                for (name, dll) in &new {
                    println!(
                        "title dll      {name} at {:#x} (loaded by the title)",
                        dll.base()
                    );
                    show(name, dll.bind_imports(&resolver, self.policy)?);
                    set_image_path(dll.base(), &self.title_dir.join(name));
                    victory_hle::runtime::set_image_size(
                        dll.base(),
                        dll.info().size_of_image as usize,
                    );
                }
                // Visible before their `DllMain`s run, which may load more.
                mapped.extend(new.iter().cloned());
                new
            };
            let failed = new.iter().enumerate().find_map(|(i, (name, dll))| {
                // SAFETY: bound above, dependencies attached first.
                let attached = unsafe { dll.attach() };
                attached
                    .err()
                    .map(|e| (i, e.context(format!("attaching {name}"))))
            });
            let Some((i, error)) = failed else {
                return Ok(new.last().map(|(_, dll)| dll.base()));
            };
            // As `LoadLibrary` does when a `DllMain` fails: detach what this
            // load attached (the failed one too) and unmap all of it.
            let bases: Vec<_> = new.iter().map(|(_, d)| d.base()).collect();
            self.mapped
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(_, d)| !bases.contains(&d.base()));
            for (_, done) in new[..=i].iter().rev() {
                // SAFETY: attached (or failing to) on this thread just now.
                unsafe { done.detach() };
            }
            for (_, dll) in new {
                forget_image(dll.base());
                // SAFETY: detached, and no title code holds it yet.
                unsafe { dll.unload() };
            }
            Err(error)
        })
    }
}

/// The Windows loader lock's shape: one thread at a time, re-entered by
/// the thread that holds it.
#[cfg(windows)]
#[derive(Default)]
struct LoaderLock {
    owner: std::sync::Mutex<(Option<std::thread::ThreadId>, usize)>,
    free: std::sync::Condvar,
}

#[cfg(windows)]
impl LoaderLock {
    fn hold<T>(&self, f: impl FnOnce() -> T) -> T {
        let me = std::thread::current().id();
        let lock = || self.owner.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut owner = lock();
            while owner.0.is_some_and(|o| o != me) {
                owner = self.free.wait(owner).unwrap_or_else(|e| e.into_inner());
            }
            *owner = (Some(me), owner.1 + 1);
        }
        let result = f();
        let mut owner = lock();
        owner.1 -= 1;
        if owner.1 == 0 {
            owner.0 = None;
            self.free.notify_one();
        }
        result
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::sync::{Arc, Mutex};

    #[test]
    fn loader_lock_is_reentrant_and_exclusive() {
        let lock = Arc::new(super::LoaderLock::default());
        assert_eq!(lock.hold(|| lock.hold(|| 7)), 7);
        let order = Arc::new(Mutex::new(Vec::new()));
        let other = lock.hold(|| {
            let (theirs, their_order) = (lock.clone(), order.clone());
            let other = std::thread::spawn(move || {
                theirs.hold(|| their_order.lock().unwrap().push(2));
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            order.lock().unwrap().push(1);
            other
        });
        other.join().unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn take_dispatch_only_intercepts_ffx_dispatch_when_enabled() {
        use victory_loader::{ImportResolver, Provenance, Resolution};
        use victory_pe::ImportSymbol;

        struct Inner;
        impl ImportResolver for Inner {
            fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
                Resolution::Found {
                    address: 0x1234,
                    provenance: Provenance::Title,
                }
            }
        }

        let dispatch = ImportSymbol::Name {
            hint: 0,
            name: "ffxDispatch".to_owned(),
        };
        let other = ImportSymbol::Name {
            hint: 0,
            name: "ffxCreateContext".to_owned(),
        };
        let loader = victory_hle::modules::ffx::LOADER;

        // Disabled (default): every import falls through to Inner.
        let off = super::TakeDispatch(&Inner, false);
        assert!(matches!(
            off.resolve(loader, &dispatch),
            Resolution::Found { address: 0x1234, provenance: Provenance::Title }
        ));

        // Enabled: only amd_fidelityfx_loader_xbox.dll!ffxDispatch and
        // ffxQuery are taken.
        let on = super::TakeDispatch(&Inner, true);
        assert!(matches!(
            on.resolve(loader, &dispatch),
            Resolution::Found { address, provenance: Provenance::Hle }
                if address == victory_hle::modules::ffx::dispatch_address()
        ));
        let query = ImportSymbol::Name {
            hint: 0,
            name: "ffxQuery".to_owned(),
        };
        assert!(matches!(
            on.resolve(loader, &query),
            Resolution::Found { address, provenance: Provenance::Hle }
                if address == victory_hle::modules::ffx::query_address()
        ));
        assert!(matches!(
            off.resolve(loader, &query),
            Resolution::Found { address: 0x1234, provenance: Provenance::Title }
        ));
        assert!(matches!(
            on.resolve(loader, &other),
            Resolution::Found { address: 0x1234, provenance: Provenance::Title }
        ));
        assert!(matches!(
            on.resolve("other.dll", &dispatch),
            Resolution::Found { address: 0x1234, provenance: Provenance::Title }
        ));
    }
}

#[cfg(not(windows))]
pub fn run(_: &Path, _: Unimplemented, _: bool, _: bool, _: Console, _: &[String]) -> Result<()> {
    anyhow::bail!("running titles requires Windows")
}

use super::*;
use std::collections::BTreeSet;
use std::ffi::OsString;

/// `third_party/victory`.
fn victory_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                rs_files(&path, out);
            }
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

/// Every `"VICTORY_..."` string literal in the given folders of the tree.
fn literals(dirs: &[&str]) -> BTreeSet<String> {
    let mut files = Vec::new();
    for dir in dirs {
        rs_files(&victory_root().join(dir), &mut files);
    }
    let mut names = BTreeSet::new();
    // Made-up names, not reads: this file's.
    let skip = |f: &Path| {
        let f = f.to_string_lossy().replace('\\', "/");
        f.ends_with("victory-core/src/settings/tests.rs")
    };
    for file in files.into_iter().filter(|f| !skip(f)) {
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let mut rest = &text[..];
        while let Some(at) = rest.find("\"VICTORY_") {
            let tail = &rest[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') && !tail[..end].ends_with('_') {
                names.insert(tail[..end].to_string());
            }
            rest = &tail[end..];
        }
    }
    names
}

/// The launcher's tuned set: `(name, value)`.
fn launcher_tuned() -> Vec<(String, String)> {
    let text =
        std::fs::read_to_string(victory_root().join("../../tools/lancer_victory.ps1")).unwrap();
    let table = &text[text.find("$tuned = [ordered]@{").unwrap()
        ..text.find("foreach ($name in $tuned.Keys)").unwrap()];
    table
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().split_once('=')?;
            let name = name.trim();
            let value = rest.trim().strip_prefix('\'')?.split('\'').next()?;
            is_variable(name).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

#[test]
fn every_variable_the_tree_reads_is_registered() {
    let missing: Vec<String> = literals(&["crates", "apps", "xtask"])
        .into_iter()
        .filter(|n| info(n).is_none() && !NOT_SETTINGS.contains(&n.as_str()) && n != FILE_VARIABLE)
        .collect();
    assert!(
        missing.is_empty(),
        "register these in settings/table.rs or settings/expert.rs: {missing:?}"
    );
}

#[test]
fn switches_in_the_shader_cache_key_are_retranslate() {
    // `victory-shader`'s build script puts every switch its source names
    // into the translation fingerprint.
    for name in literals(&["crates/victory-shader/src"]) {
        if let Some(info) = info(&name) {
            assert_eq!(info.apply, Apply::Retranslate, "{name}");
        }
    }
    for name in [
        "VICTORY_PORT_CBV",
        "VICTORY_WAVE_COLLAPSE",
        "VICTORY_MEET_PRUNE",
        "VICTORY_WAVE32_NVIDIA",
        "VICTORY_NATIVE_GLASS",
        "VICTORY_RT_MISS_SKY",
        "VICTORY_RT_GLASS_SKY",
    ] {
        assert_eq!(info(name).unwrap().apply, Apply::Retranslate, "{name}");
    }
}

#[test]
fn the_launchers_switches_are_described_and_their_values_valid() {
    let tuned = launcher_tuned();
    assert!(tuned.len() > 50);
    for (name, value) in tuned {
        let info = info(&name).unwrap_or_else(|| panic!("{name} is not registered"));
        assert!(!info.expert, "{name}");
        assert!(
            valid(info.kind, &value),
            "{name}={value} is not valid for {:?}",
            info.kind
        );
    }
}

#[test]
fn the_table_is_sorted_unique_and_consistent() {
    let names: Vec<&str> = table::TABLE.iter().map(|i| i.name).collect();
    assert!(
        names.windows(2).all(|w| w[0] < w[1]),
        "table.rs: not sorted or duplicated"
    );
    let experts: Vec<&str> = expert::EXPERT.iter().map(|e| e.0).collect();
    assert!(
        experts.windows(2).all(|w| w[0] < w[1]),
        "expert.rs: not sorted or duplicated"
    );
    assert!(experts.iter().all(|n| !names.contains(n)));
    assert_eq!(infos().len(), names.len() + experts.len());
    for i in infos() {
        assert!(is_variable(i.name), "{}", i.name);
        assert!(!i.label.is_empty() && !i.group.is_empty(), "{}", i.name);
        assert!(
            i.default.is_empty() || valid(i.kind, i.default),
            "{}: default {}",
            i.name,
            i.default
        );
        assert!(!NOT_SETTINGS.contains(&i.name), "{}", i.name);
    }
}

#[test]
fn the_settings_file_is_named_by_the_variable_else_the_user_folder() {
    let env = |file: Option<&'static str>| {
        move |k: &str| match k {
            "VICTORY_SETTINGS_FILE" => file.map(OsString::from),
            "LOCALAPPDATA" => Some(OsString::from("C:/Users/x/AppData/Local")),
            _ => None,
        }
    };
    assert_eq!(
        file_path_from(env(None)),
        Some(PathBuf::from(
            "C:/Users/x/AppData/Local/victory/settings.toml"
        ))
    );
    assert_eq!(
        file_path_from(env(Some(""))),
        Some(PathBuf::from(
            "C:/Users/x/AppData/Local/victory/settings.toml"
        ))
    );
    assert_eq!(file_path_from(env(Some("none"))), None);
    assert_eq!(file_path_from(env(Some("NONE"))), None);
    assert_eq!(
        file_path_from(env(Some("D:/s.toml"))),
        Some(PathBuf::from("D:/s.toml"))
    );
    assert_eq!(file_path_from(|_| None), None);
}

#[test]
fn the_file_reads_strings_numbers_and_booleans() {
    let (values, problems) = parse_file(
        "# comment\nVICTORY_PRESENT_CAP = 60\nVICTORY_AMBIENT = 0.8\nVICTORY_OVERLAY = false\n\
         VICTORY_VRAM_IDLE_MS = \"8000,2000,1000\"\nVICTORY_NOT_A_SETTING_X = \"1\"\nOTHER = 1\n[table]\nVICTORY_X = 1\n",
    );
    let get = |k: &str| values.get(k).map(String::as_str);
    assert_eq!(get("VICTORY_PRESENT_CAP"), Some("60"));
    assert_eq!(get("VICTORY_AMBIENT"), Some("0.8"));
    assert_eq!(get("VICTORY_OVERLAY"), Some("0"));
    assert_eq!(get("VICTORY_VRAM_IDLE_MS"), Some("8000,2000,1000"));
    assert_eq!(
        get("VICTORY_NOT_A_SETTING_X"),
        Some("1"),
        "unknown names are kept for other builds"
    );
    assert_eq!(get("OTHER"), None);
    assert_eq!(values.len(), 5);
    assert_eq!(problems.len(), 3, "{problems:?}");
    let (values, problems) = parse_file("VICTORY_X = = 1");
    assert!(values.is_empty() && problems.len() == 1);
}

#[test]
fn the_environment_wins_over_the_file() {
    let mut file = BTreeMap::new();
    file.insert("VICTORY_COPY_COALESCE".to_string(), "4096".to_string());
    file.insert("VICTORY_PRESENT_CAP".to_string(), "60".to_string());
    let (applied, overridden) = split(&file, |n| n == "VICTORY_COPY_COALESCE");
    assert_eq!(applied, ["VICTORY_PRESENT_CAP"]);
    assert_eq!(overridden, ["VICTORY_COPY_COALESCE"]);
}

#[test]
fn no_file_changes_nothing_and_logs_nothing() {
    assert!(Loaded::default().lines().is_empty());
    let loaded = Loaded {
        path: Some(PathBuf::from("s.toml")),
        ..Loaded::default()
    };
    assert!(loaded.lines().is_empty());
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    // A missing file reads as no file.
    assert!(
        matches!(std::fs::read_to_string(&missing), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    );
}

#[test]
fn saved_files_read_back_the_same() {
    let mut values = BTreeMap::new();
    values.insert("VICTORY_POP_FOLDER".to_string(), "C:\\a \"b\"".to_string());
    values.insert("VICTORY_PRESENT_CAP".to_string(), "60".to_string());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("victory").join("settings.toml");
    write_file(&path, &values).unwrap();
    let (back, problems) = parse_file(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(back, values);
    assert!(problems.is_empty());
}

/// A read-only settings file is not replaced, and no temporary file is left beside it.
#[test]
fn a_read_only_file_is_refused_and_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    std::fs::write(&path, "VICTORY_PRESENT_CAP = \"30\"\n").unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&path, perms.clone()).unwrap();
    let mut values = BTreeMap::new();
    values.insert("VICTORY_PRESENT_CAP".to_string(), "60".to_string());
    let err = write_file(&path, &values).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "VICTORY_PRESENT_CAP = \"30\"\n");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["settings.toml"]);
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn changes_merge_over_the_file_and_a_cleared_one_leaves_it() {
    let file: BTreeMap<String, String> = [("VICTORY_A1", "1"), ("VICTORY_B1", "2")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let changed: BTreeMap<String, String> = [("VICTORY_A1", ""), ("VICTORY_C1", "3")]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let out = merged(&file, &changed);
    assert_eq!(
        out.keys().map(String::as_str).collect::<Vec<_>>(),
        ["VICTORY_B1", "VICTORY_C1"]
    );
}

#[test]
fn values_are_checked_against_their_kind() {
    assert!(
        valid(Kind::Bool, "1")
            && valid(Kind::Bool, "0")
            && !valid(Kind::Bool, "2")
            && !valid(Kind::Bool, "on")
    );
    let int = Kind::Int { min: -2, max: 2 };
    assert!(valid(int, "-2") && valid(int, " 2") && !valid(int, "3") && !valid(int, "x"));
    let float = Kind::Float {
        min: 0.0,
        max: 1.0,
        step: 0.1,
    };
    assert!(valid(float, "0.5") && !valid(float, "1.5"));
    assert!(
        valid(Kind::Choice(&["real", "compare"]), "real") && !valid(Kind::Choice(&["real"]), "x")
    );
    assert_eq!(
        set("VICTORY_NO_SUCH_SETTING", "1"),
        Err(SetError::Unknown("VICTORY_NO_SUCH_SETTING".into()))
    );
    assert!(matches!(
        set("VICTORY_PRESENT_SYNC", "9"),
        Err(SetError::Invalid { .. })
    ));
}

fn parse_cap(v: Option<&str>) -> u64 {
    v.and_then(|v| v.parse().ok()).unwrap_or(0)
}

#[test]
fn a_live_setting_is_read_again_after_a_change() {
    static CAP: Live = Live::new("VICTORY_PRESENT_CAP", parse_cap);
    if std::env::var_os("VICTORY_PRESENT_CAP").is_some() {
        return;
    }
    assert_eq!(CAP.get(), 0);
    assert_eq!(set("VICTORY_PRESENT_CAP", "45"), Ok(Applied::Now));
    assert_eq!(CAP.get(), 45);
    assert_eq!(get("VICTORY_PRESENT_CAP").as_deref(), Some("45"));
    assert!(!pending_restart().contains(&"VICTORY_PRESENT_CAP"));
    assert_eq!(set("VICTORY_PRESENT_CAP", ""), Ok(Applied::Now));
    assert_eq!(CAP.get(), 0);
    assert_eq!(
        get("VICTORY_PRESENT_CAP").as_deref(),
        Some("0"),
        "back to the default"
    );
}

#[test]
fn a_restart_setting_waits_and_is_listed() {
    if std::env::var_os("VICTORY_CARD_WINDOWS_MB").is_some() {
        return;
    }
    assert_eq!(apply_of("VICTORY_CARD_WINDOWS_MB"), Some(Apply::Restart));
    assert_eq!(
        set("VICTORY_CARD_WINDOWS_MB", "1024"),
        Ok(Applied::OnRestart)
    );
    assert_eq!(get("VICTORY_CARD_WINDOWS_MB").as_deref(), Some("1024"));
    assert_eq!(launched("VICTORY_CARD_WINDOWS_MB"), None);
    assert!(pending_restart().contains(&"VICTORY_CARD_WINDOWS_MB"));
    set("VICTORY_CARD_WINDOWS_MB", "").unwrap();
    assert!(!pending_restart().contains(&"VICTORY_CARD_WINDOWS_MB"));
}

static HOOKED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[test]
fn a_hook_makes_a_setting_live() {
    assert_eq!(apply_of("VICTORY_DLSS_SHARPNESS"), Some(Apply::Restart));
    on_change("VICTORY_DLSS_SHARPNESS", |v| {
        HOOKED.lock().unwrap().push(v.to_string())
    });
    assert_eq!(apply_of("VICTORY_DLSS_SHARPNESS"), Some(Apply::Live));
    assert_eq!(set("VICTORY_DLSS_SHARPNESS", "0.5"), Ok(Applied::Now));
    assert_eq!(*HOOKED.lock().unwrap(), ["0.5"]);
}

/// What `dev_live.py`'s `schedule_text` writes for a two-key arm.
const DEV_LIVE_SCHEDULE: &str = "# dev.py train live arm awake: A=off B=on, 4 x 40 s from 150 s (ABAB)\n\
at 150s set VICTORY_SKIP_WC_BACK=0\nat 150s set VICTORY_PRESENT_SMOOTH=0\n\
at 190s set VICTORY_SKIP_WC_BACK=1\nat 190s set VICTORY_PRESENT_SMOOTH=0.5\n\
at 230s set VICTORY_SKIP_WC_BACK=0\nat 230s set VICTORY_PRESENT_SMOOTH=0\n\
\n  at 270.5s set VICTORY_SKIP_WC_BACK=1\nat 270.5s set VICTORY_PRESENT_SMOOTH=0.5\n";

#[test]
fn a_schedule_reads_dev_live_lines_into_numbered_segments() {
    let steps = schedule::parse(DEV_LIVE_SCHEDULE).unwrap();
    assert_eq!(steps.len(), 8);
    let segs: Vec<(f64, usize, &str, &str)> = steps
        .iter()
        .map(|s| (s.at, s.segment, s.name, s.value.as_str()))
        .collect();
    assert_eq!(segs[0], (150.0, 1, "VICTORY_SKIP_WC_BACK", "0"));
    assert_eq!(segs[1], (150.0, 1, "VICTORY_PRESENT_SMOOTH", "0"));
    assert_eq!(segs[3], (190.0, 2, "VICTORY_PRESENT_SMOOTH", "0.5"));
    assert_eq!(segs[7], (270.5, 4, "VICTORY_PRESENT_SMOOTH", "0.5"));
    // Out of order: sorted by time, the same time keeping its order.
    let steps = schedule::parse(
        "at 20s set VICTORY_SKIP_WC_BACK=1\nat 10s set VICTORY_SKIP_WC_BACK=0\n",
    )
    .unwrap();
    assert_eq!(
        steps.iter().map(|s| (s.at, s.segment)).collect::<Vec<_>>(),
        [(10.0, 1), (20.0, 2)]
    );
    assert_eq!(schedule::parse("# nothing\n\n"), Ok(Vec::new()));
}

#[test]
fn a_schedule_with_a_setting_that_is_not_live_is_refused_whole() {
    let err = schedule::parse(
        "at 10s set VICTORY_SKIP_WC_BACK=1\nat 20s set VICTORY_CARD_WINDOWS_MB=1024\n",
    )
    .unwrap_err();
    assert_eq!(err.len(), 1, "{err:?}");
    assert!(
        err[0].starts_with("line 2: VICTORY_CARD_WINDOWS_MB is Restart, not Live"),
        "{err:?}"
    );
    let err = schedule::parse("at 1s set VICTORY_PORT_CBV=1\n").unwrap_err();
    assert!(err[0].contains("Retranslate, not Live"), "{err:?}");
    let err = schedule::parse(
        "at 1s set VICTORY_NO_SUCH_SETTING=1\nat 1s set VICTORY_SKIP_WC_BACK=2\n\
         at x set VICTORY_SKIP_WC_BACK=1\nat 5 set VICTORY_SKIP_WC_BACK=1\nevery 30s toggle VICTORY_SKIP_WC_BACK\n\
         at -1s set VICTORY_SKIP_WC_BACK=1\n",
    )
    .unwrap_err();
    assert_eq!(err.len(), 6, "{err:?}");
    assert!(err[0].contains("no setting is called VICTORY_NO_SUCH_SETTING"));
    assert!(err[1].contains("does not take \"2\""));
}

#[test]
fn a_schedule_sets_each_change_at_its_time_and_logs_its_segment() {
    if std::env::var_os("VICTORY_SKIP_WC_BACK").is_some() {
        return;
    }
    static ON: Live = Live::new("VICTORY_SKIP_WC_BACK", |v| u64::from(v == Some("1")));
    let steps = schedule::parse(
        "at 0.25s set VICTORY_SKIP_WC_BACK=1\nat 0.5s set VICTORY_SKIP_WC_BACK=0\n",
    )
    .unwrap();
    let zero = Instant::now();
    let mut waits = Vec::new();
    let mut seen = Vec::new();
    let mut lines = Vec::new();
    // Each wait returns at once; what the setting reads after the change is recorded.
    schedule::run(
        &steps,
        zero,
        |t| {
            seen.push(ON.get_bool());
            waits.push(t - zero);
        },
        |line| lines.push(line),
    );
    assert_eq!(
        waits,
        [
            std::time::Duration::from_millis(250),
            std::time::Duration::from_millis(500)
        ]
    );
    assert_eq!(seen, [false, true], "the first change took effect before the second wait");
    assert!(!ON.get_bool(), "the second change took effect");
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0].starts_with("settings: segment 1 VICTORY_SKIP_WC_BACK=1 at "),
        "{lines:?}"
    );
    assert!(
        lines[1].starts_with("settings: segment 2 VICTORY_SKIP_WC_BACK=0 at "),
        "{lines:?}"
    );
    set("VICTORY_SKIP_WC_BACK", "").unwrap();
}

#[test]
fn the_segment_line_is_the_one_dev_live_reads() {
    let step = schedule::Step {
        at: 190.0,
        segment: 2,
        name: "VICTORY_PRESENT_SMOOTH",
        value: "0.5".into(),
    };
    // tests/test_dev_live.py `test_the_registry_segment_line_parses` holds the same line.
    assert_eq!(
        schedule::segment_line(&step, 190.04),
        "settings: segment 2 VICTORY_PRESENT_SMOOTH=0.5 at 190.0"
    );
}

#[test]
fn every_live_setting_is_read_through_a_live_cell_or_a_hook() {
    let mut files = Vec::new();
    rs_files(&victory_root().join("crates"), &mut files);
    let text: String = files
        .iter()
        .filter(|f| !f.ends_with("tests.rs"))
        .map(|f| std::fs::read_to_string(f).unwrap_or_default())
        .collect::<String>()
        .split_whitespace()
        .collect();
    for i in infos().iter().filter(|i| i.apply == Apply::Live) {
        assert!(
            text.contains(&format!("Live::new(\"{}\"", i.name))
                || text.contains(&format!("on_change(\"{}\"", i.name)),
            "{} is Live but nothing reads it through settings::Live or settings::on_change",
            i.name
        );
    }
}

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
    // This file's own made-up names are not reads.
    let this = Path::new(file!()).file_name().unwrap();
    for file in files.into_iter().filter(|f| {
        !(f.file_name() == Some(this) && f.parent().is_some_and(|p| p.ends_with("settings")))
    }) {
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

//! (El1xr) The menu's state and navigation, with no OS calls: tabs from the registry's groups, the
//! selected row, value stepping by kind, presets, and what a key, button or click does.

use super::model::{Applied, Apply, FIRST, Info, Kind, MenuModel, named};

/// A tab: one of the registry's groups, or the presets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Group(&'static str),
    Presets,
}

impl Tab {
    pub const DISPLAY: Tab = Tab::Group("Display");

    pub fn title(self) -> &'static str {
        match self {
            Tab::Group(g) => g,
            Tab::Presets => "Presets",
        }
    }
}

/// Groups in the order their tabs come; others follow as the registry first names them, then
/// Presets, then [`EXPERIMENTAL`].
const GROUP_ORDER: &[&str] = &[
    "Display",
    "Submission",
    "Memory",
    "Streaming",
    "Threads",
    "Shaders",
    "Fixes",
    "Diagnostics",
    "Launch",
];
/// The registry's group of undescribed (expert) settings: the last tab.
pub const EXPERIMENTAL: &str = "Experimental";

/// Whether `info` is listed: expert settings only with the expert toggle on.
pub fn listed(info: &Info, expert: bool) -> bool {
    expert || !info.expert
}

/// The tabs: each group with a listed setting, then Presets, then Experimental.
pub fn tabs(model: &dyn MenuModel, expert: bool) -> Vec<Tab> {
    let mut groups: Vec<&'static str> = Vec::new();
    for info in model.infos().iter().filter(|i| listed(i, expert)) {
        if !groups.contains(&info.group) {
            groups.push(info.group);
        }
    }
    let rank = |g: &&str| {
        GROUP_ORDER
            .iter()
            .position(|o| o == g)
            .unwrap_or(GROUP_ORDER.len())
    };
    groups.sort_by_key(rank);
    let mut out: Vec<Tab> = groups
        .iter()
        .filter(|&&g| g != EXPERIMENTAL)
        .map(|&g| Tab::Group(g))
        .collect();
    out.push(Tab::Presets);
    if groups.contains(&EXPERIMENTAL) {
        out.push(Tab::Group(EXPERIMENTAL));
    }
    out
}

/// One line of a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Index into the model's `infos()`.
    Setting(usize),
    /// Index into the model's `presets()`.
    Preset(usize),
    Save,
    Undo,
}

/// What the user did, already decoded from keys, buttons or the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Toggle,
    Close,
    Up,
    Down,
    Left,
    Right,
    Activate,
    NextTab,
    PrevTab,
    ToggleExpert,
    SelectTab(Tab),
    SelectRow(usize),
}

pub struct Menu {
    pub open: bool,
    pub tab: Tab,
    /// Index into `rows()`.
    pub selected: usize,
    /// First visible row.
    pub scroll: usize,
    pub expert: bool,
    /// The last change's outcome, shown in the footer.
    pub status: Option<String>,
}

impl Default for Menu {
    fn default() -> Self {
        Menu {
            open: false,
            tab: Tab::DISPLAY,
            selected: 0,
            scroll: 0,
            expert: false,
            status: None,
        }
    }
}

impl Menu {
    pub fn tabs(&self, model: &dyn MenuModel) -> Vec<Tab> {
        tabs(model, self.expert)
    }

    pub fn rows(&self, model: &dyn MenuModel) -> Vec<Row> {
        rows(model, self.tab, self.expert)
    }

    /// Applies `action`; returns whether anything the menu draws changed.
    pub fn act(&mut self, model: &mut dyn MenuModel, action: Action) -> bool {
        if !self.open {
            if action == Action::Toggle {
                self.open = true;
                self.status = None;
                return true;
            }
            return false;
        }
        let rows = self.rows(model);
        match action {
            Action::Toggle | Action::Close => self.open = false,
            Action::Up => self.selected = self.selected.saturating_sub(1),
            Action::Down => self.selected = (self.selected + 1).min(rows.len().saturating_sub(1)),
            Action::Left | Action::Right => {
                let dir = if action == Action::Left { -1 } else { 1 };
                match rows.get(self.selected) {
                    Some(&Row::Setting(i)) => self.step(model, i, dir),
                    _ => return false,
                }
            }
            Action::Activate => match rows.get(self.selected) {
                Some(&Row::Setting(i)) => {
                    let info = model.infos()[i];
                    if matches!(info.kind, Kind::Bool | Kind::Choice(_))
                        || named(info.name).is_some()
                    {
                        self.step(model, i, 1);
                    } else {
                        return false;
                    }
                }
                Some(&Row::Preset(p)) => self.apply_preset(model, p),
                Some(Row::Save) => {
                    self.status = Some(match model.save() {
                        Ok(()) => "Settings saved".into(),
                        Err(e) => format!("Not saved: {e}"),
                    })
                }
                Some(Row::Undo) => self.undo(model),
                None => return false,
            },
            Action::NextTab | Action::PrevTab => {
                let tabs = self.tabs(model);
                let at = tabs.iter().position(|&t| t == self.tab).unwrap_or(0);
                let n = tabs.len();
                let next = if action == Action::NextTab {
                    (at + 1) % n
                } else {
                    (at + n - 1) % n
                };
                self.select_tab(tabs[next]);
            }
            Action::SelectTab(tab) => {
                if tab == self.tab || !self.tabs(model).contains(&tab) {
                    return false;
                }
                self.select_tab(tab);
            }
            Action::SelectRow(row) => {
                if row >= rows.len() || row == self.selected {
                    return false;
                }
                self.selected = row;
            }
            Action::ToggleExpert => {
                self.expert = !self.expert;
                let tabs = self.tabs(model);
                if !tabs.contains(&self.tab) {
                    self.select_tab(tabs[0]);
                }
            }
        }
        true
    }

    fn select_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.scroll = 0;
    }

    /// Keeps the selected row among the `visible` rows shown from `scroll`.
    pub fn follow(&mut self, visible: usize, rows: usize) {
        let visible = visible.max(1);
        self.selected = self.selected.min(rows.saturating_sub(1));
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(rows.saturating_sub(visible));
    }

    fn step(&mut self, model: &mut dyn MenuModel, i: usize, dir: i32) {
        let info = model.infos()[i];
        let current = model.get(info.name).unwrap_or_default();
        let Some(next) = step_value(info.name, info.kind, &current, dir) else {
            self.status = Some(format!("{} is edited in the settings file", info.label));
            return;
        };
        if next != current {
            self.set(model, info, &next);
        }
    }

    fn set(&mut self, model: &mut dyn MenuModel, info: Info, value: &str) {
        let shown = shown(info.name, info.kind, value);
        self.status = Some(match model.set(info.name, value) {
            Ok(Applied::Now) => format!("{}: {shown}", info.label),
            Ok(Applied::OnRestart) if model.apply_of(info.name) == Apply::Retranslate => {
                format!("{}: {shown} after a restart and shader rebuild", info.label)
            }
            Ok(Applied::OnRestart) => format!("{}: {shown} after a restart", info.label),
            Err(e) => e.to_string(),
        });
    }

    fn apply_preset(&mut self, model: &mut dyn MenuModel, p: usize) {
        let preset = model.presets()[p];
        let failed = preset
            .values
            .iter()
            .filter(|(n, v)| model.set(n, v).is_err())
            .count();
        self.status = Some(if failed == 0 {
            format!("{} preset applied", preset.name)
        } else {
            format!("{} preset: {failed} values refused", preset.name)
        });
    }

    /// Every setting changed while running back to its launch value.
    fn undo(&mut self, model: &mut dyn MenuModel) {
        let changed: Vec<&'static str> = model
            .infos()
            .iter()
            .filter(|i| model.get(i.name) != model.launched(i.name))
            .map(|i| i.name)
            .collect();
        for name in &changed {
            let _ = model.set(name, "");
        }
        self.status = Some(match changed.len() {
            0 => "Nothing to undo".into(),
            n => format!("{n} changes undone"),
        });
    }
}

/// The rows of `tab`: a group's listed settings, [`FIRST`] ones first.
pub fn rows(model: &dyn MenuModel, tab: Tab, expert: bool) -> Vec<Row> {
    let group = match tab {
        Tab::Presets => {
            let presets = (0..model.presets().len()).map(Row::Preset);
            return presets.chain([Row::Save, Row::Undo]).collect();
        }
        Tab::Group(g) => g,
    };
    let infos = model.infos();
    let mut at: Vec<usize> = (0..infos.len())
        .filter(|&i| infos[i].group == group && listed(&infos[i], expert))
        .collect();
    at.sort_by_key(|&i| {
        FIRST
            .iter()
            .position(|&n| n == infos[i].name)
            .unwrap_or(FIRST.len())
    });
    at.into_iter().map(Row::Setting).collect()
}

/// The value one step from `current` in direction `dir` (negative: down), `None` for text.
/// Named values step in their own order; integers on a round step (1, 2 or 5 times a power of
/// ten, about a 48th of the range); choices wrap.
pub fn step_value(name: &str, kind: Kind, current: &str, dir: i32) -> Option<String> {
    let dir = dir.signum();
    if let Some(values) = named(name) {
        let n = values.len() as i32;
        let at = values.iter().position(|&(v, _)| v == current);
        let next = at.map_or(0, |at| (at as i32 + dir).rem_euclid(n));
        return Some(values[next as usize].0.to_string());
    }
    Some(match kind {
        Kind::Bool => if current == "1" { "0" } else { "1" }.to_string(),
        Kind::Int { min, max } => {
            let step = round_step((max - min) / 48);
            let v = current.trim().parse::<i64>().unwrap_or(min);
            let below = v.div_euclid(step) * step;
            let next = match dir {
                1 => below + step,
                _ if below < v => below,
                _ => below - step,
            };
            next.clamp(min, max).to_string()
        }
        Kind::Float { min, max, step } => {
            let v = current.trim().parse::<f64>().unwrap_or(min);
            let next = ((v + step * f64::from(dir)) / step).round() * step;
            format!("{:.*}", decimals(step), next.clamp(min, max))
        }
        Kind::Choice(options) => {
            let n = options.len() as i32;
            let at = options.iter().position(|&o| o == current).unwrap_or(0) as i32;
            options[(at + dir).rem_euclid(n) as usize].to_string()
        }
        Kind::Text => return None,
    })
}

/// The 1-2-5 series value at or below `raw` (at least 1).
fn round_step(raw: i64) -> i64 {
    let mut unit = 1;
    while unit * 10 <= raw {
        unit *= 10;
    }
    [5, 2, 1]
        .into_iter()
        .map(|k| k * unit)
        .find(|&s| s <= raw)
        .unwrap_or(1)
}

fn decimals(step: f64) -> usize {
    let mut d = 0;
    while d < 6
        && ((step * 10f64.powi(d as i32)).round() - step * 10f64.powi(d as i32)).abs() > 1e-9
    {
        d += 1;
    }
    d
}

/// Choice values shown in capitals.
const ACRONYMS: &[&str] = &["dlss", "dlaa", "fsr", "taa", "fxaa", "xess"];

/// How a value of setting `name` reads in the menu (empty: the code's default).
pub fn shown(name: &str, kind: Kind, value: &str) -> String {
    if let Some(&(_, label)) = named(name).and_then(|v| v.iter().find(|(v, _)| *v == value)) {
        return label.to_string();
    }
    match kind {
        _ if value.is_empty() => "Default".to_string(),
        Kind::Bool => if value == "1" { "On" } else { "Off" }.to_string(),
        Kind::Choice(_) if ACRONYMS.contains(&value) => value.to_uppercase(),
        Kind::Choice(_) => {
            let mut c = value.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
        Kind::Int { min: 0, .. } if value == "0" => "Off".to_string(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::model::{Registry, registry_lock};
    use super::*;
    use victory_core::settings;

    fn opened(model: &mut Registry) -> Menu {
        let mut m = Menu::default();
        assert!(m.act(model, Action::Toggle));
        m
    }

    fn row_of(model: &Registry, tab: Tab, name: &str) -> usize {
        rows(model, tab, false)
            .iter()
            .position(|r| matches!(r, Row::Setting(i) if model.infos()[*i].name == name))
            .unwrap_or_else(|| panic!("{name} not under {tab:?}"))
    }

    #[test]
    fn closed_menu_ignores_everything_but_toggle() {
        let mut m = Menu::default();
        let mut model = Registry::default();
        for a in [
            Action::Down,
            Action::Right,
            Action::Activate,
            Action::NextTab,
            Action::Close,
            Action::ToggleExpert,
        ] {
            assert!(!m.act(&mut model, a));
        }
        assert_eq!((m.selected, m.tab, m.expert), (0, Tab::DISPLAY, false));
        assert!(m.act(&mut model, Action::Toggle));
        assert!(m.open);
        assert!(m.act(&mut model, Action::Close));
        assert!(!m.open);
    }

    #[test]
    fn tabs_are_the_registrys_groups_and_expert_ones_wait_for_the_toggle() {
        let model = Registry::default();
        let plain = tabs(&model, false);
        let all = tabs(&model, true);
        assert_eq!(plain[0], Tab::DISPLAY);
        assert_eq!(plain.last(), Some(&Tab::Presets));
        assert!(!plain.contains(&Tab::Group(EXPERIMENTAL)));
        assert_eq!(all.last(), Some(&Tab::Group(EXPERIMENTAL)));
        for g in [
            "Submission",
            "Memory",
            "Streaming",
            "Threads",
            "Shaders",
            "Fixes",
            "Diagnostics",
            "Launch",
        ] {
            assert!(plain.contains(&Tab::Group(g)), "{g}");
        }
        // Every setting is under exactly its group's tab; expert ones only with the toggle on.
        for (expert, tabs) in [(false, &plain), (true, &all)] {
            let mut seen = vec![0usize; model.infos().len()];
            for &tab in tabs.iter() {
                for row in rows(&model, tab, expert) {
                    if let Row::Setting(i) = row {
                        let info = model.infos()[i];
                        assert_eq!(Tab::Group(info.group), tab, "{}", info.name);
                        assert!(expert || !info.expert, "{}", info.name);
                        seen[i] += 1;
                    }
                }
            }
            for (i, info) in model.infos().iter().enumerate() {
                assert_eq!(seen[i], usize::from(listed(info, expert)), "{}", info.name);
            }
        }
        assert_eq!(rows(&model, Tab::Presets, false).last(), Some(&Row::Undo));
    }

    #[test]
    fn the_display_tab_starts_with_the_upscaler_rows() {
        let model = Registry::default();
        let names: Vec<&str> = rows(&model, Tab::DISPLAY, false)
            .iter()
            .take(5)
            .map(|r| match r {
                Row::Setting(i) => model.infos()[*i].name,
                _ => "",
            })
            .collect();
        assert_eq!(
            names,
            [
                "VICTORY_UPSCALE",
                "VICTORY_DLSS_QUALITY",
                "VICTORY_FFX_SCALE",
                "VICTORY_DLSS_SHARPNESS",
                "VICTORY_FRAME_GEN"
            ]
        );
    }

    #[test]
    fn up_down_clamp_tabs_wrap_and_expert_hides_its_tab() {
        let mut model = Registry::default();
        let mut m = opened(&mut model);
        m.act(&mut model, Action::Up);
        assert_eq!(m.selected, 0);
        for _ in 0..200 {
            m.act(&mut model, Action::Down);
        }
        assert_eq!(m.selected, m.rows(&model).len() - 1);
        m.act(&mut model, Action::NextTab);
        assert_eq!((m.tab, m.selected), (Tab::Group("Submission"), 0));
        m.act(&mut model, Action::PrevTab);
        m.act(&mut model, Action::PrevTab);
        assert_eq!(
            m.tab,
            Tab::Presets,
            "wraps past the first tab, Experimental hidden"
        );
        assert!(!m.act(&mut model, Action::SelectTab(Tab::Group(EXPERIMENTAL))));
        m.act(&mut model, Action::ToggleExpert);
        m.act(&mut model, Action::NextTab);
        assert_eq!(m.tab, Tab::Group(EXPERIMENTAL));
        m.act(&mut model, Action::ToggleExpert);
        assert_eq!(
            m.tab,
            Tab::DISPLAY,
            "hiding expert leaves the Experimental tab"
        );
    }

    #[test]
    fn values_step_by_kind() {
        let k = |kind, cur, dir| step_value("X", kind, cur, dir);
        assert_eq!(k(Kind::Bool, "0", 1).unwrap(), "1");
        assert_eq!(k(Kind::Bool, "", 1).unwrap(), "1");
        let cap = Kind::Int { min: 0, max: 360 };
        assert_eq!(k(cap, "0", 1).unwrap(), "5");
        assert_eq!(k(cap, "57", 1).unwrap(), "60", "back on the grid");
        assert_eq!(k(cap, "57", -1).unwrap(), "55");
        assert_eq!(k(cap, "0", -1).unwrap(), "0");
        assert_eq!(k(cap, "360", 1).unwrap(), "360");
        assert_eq!(k(Kind::Int { min: -2, max: 2 }, "0", -1).unwrap(), "-1");
        assert_eq!(
            k(
                Kind::Int {
                    min: 0,
                    max: 1048576
                },
                "0",
                1
            )
            .unwrap(),
            "20000"
        );
        let scale = Kind::Float {
            min: 0.5,
            max: 1.0,
            step: 0.05,
        };
        assert_eq!(k(scale, "0.75", -1).unwrap(), "0.70");
        assert_eq!(k(scale, "1.0", 1).unwrap(), "1.00");
        assert_eq!(k(scale, "", 1).unwrap(), "0.55");
        let c = Kind::Choice(&["off", "fsr"]);
        assert_eq!(k(c, "off", -1).unwrap(), "fsr");
        assert_eq!(k(c, "fsr", 1).unwrap(), "off");
        assert_eq!(k(Kind::Text, "x", 1), None);
        // Named values: DLSS quality steps from ultra performance up to DLAA.
        let q = Kind::Int { min: 0, max: 5 };
        assert_eq!(step_value("VICTORY_DLSS_QUALITY", q, "2", 1).unwrap(), "4");
        assert_eq!(step_value("VICTORY_DLSS_QUALITY", q, "0", -1).unwrap(), "3");
        assert_eq!(shown("VICTORY_DLSS_QUALITY", q, "0"), "Performance");
        assert_eq!(shown("VICTORY_DLSS_QUALITY", q, "5"), "DLAA");
        assert_eq!(
            shown("VICTORY_UPSCALE", Kind::Choice(&["off"]), "off"),
            "Title's own"
        );
        assert_eq!(shown("X", c, "fsr"), "FSR");
        assert_eq!(shown("X", Kind::Int { min: 0, max: 9 }, "0"), "Off");
        assert_eq!(shown("X", scale, ""), "Default");
    }

    static HOOK: Once = Once::new();
    static HOOKED: AtomicUsize = AtomicUsize::new(0);
    static SAVES: AtomicUsize = AtomicUsize::new(0);

    /// Live settings apply at once and stay out of the restart band; restart and rebuild ones
    /// join it; SAVE goes to the model's save; UNDO puts everything back.
    #[test]
    fn live_restart_and_rebuild_changes_go_through_the_registry() {
        let _lock = registry_lock();
        HOOK.call_once(|| {
            settings::on_change("VICTORY_DLSS_SHARPNESS", |_| {
                HOOKED.fetch_add(1, Ordering::SeqCst);
            })
        });
        let mut model = Registry::with_save(|| {
            SAVES.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let mut m = opened(&mut model);
        assert_eq!(
            model.apply_of("VICTORY_DLSS_SHARPNESS"),
            Apply::Live,
            "a hook makes it live"
        );
        assert_eq!(model.apply_of("VICTORY_FRAME_GEN"), Apply::Restart);
        assert_eq!(model.apply_of("VICTORY_SCC_MINMAX"), Apply::Retranslate);

        let hooked = HOOKED.load(Ordering::SeqCst);
        let row = row_of(&model, Tab::DISPLAY, "VICTORY_DLSS_SHARPNESS");
        m.act(&mut model, Action::SelectRow(row));
        m.act(&mut model, Action::Right);
        assert_eq!(model.get("VICTORY_DLSS_SHARPNESS").as_deref(), Some("0.05"));
        assert_eq!(m.status.as_deref(), Some("DLSS sharpness: 0.05"));
        assert_eq!(HOOKED.load(Ordering::SeqCst), hooked + 1);
        assert!(model.pending_restart().is_empty());

        let row = row_of(&model, Tab::DISPLAY, "VICTORY_FRAME_GEN");
        m.act(&mut model, Action::SelectRow(row));
        m.act(&mut model, Action::Activate);
        assert_eq!(model.get("VICTORY_FRAME_GEN").as_deref(), Some("fsr"));
        assert_eq!(
            m.status.as_deref(),
            Some("Frame generation: FSR after a restart")
        );
        assert_eq!(model.pending_restart(), ["VICTORY_FRAME_GEN"]);

        m.act(&mut model, Action::SelectTab(Tab::Group("Shaders")));
        let row = row_of(&model, Tab::Group("Shaders"), "VICTORY_SCC_MINMAX");
        m.act(&mut model, Action::SelectRow(row));
        m.act(&mut model, Action::Activate);
        assert_eq!(
            m.status.as_deref(),
            Some("Scc minmax: On after a restart and shader rebuild")
        );
        assert_eq!(model.pending_restart().len(), 2);

        m.act(&mut model, Action::SelectTab(Tab::Group("Launch")));
        let row = row_of(&model, Tab::Group("Launch"), "VICTORY_CACHE_DIR");
        m.act(&mut model, Action::SelectRow(row));
        m.act(&mut model, Action::Right);
        assert_eq!(
            m.status.as_deref(),
            Some("Cache folder is edited in the settings file")
        );

        m.act(&mut model, Action::SelectTab(Tab::Presets));
        m.act(&mut model, Action::SelectRow(0));
        m.act(&mut model, Action::Activate);
        assert_eq!(m.status.as_deref(), Some("Quality preset applied"));
        assert_eq!(model.get("VICTORY_DLSS_QUALITY").as_deref(), Some("5"));
        assert_eq!(model.get("VICTORY_UPSCALE").as_deref(), Some("dlss"));
        let presets = model.presets().len();
        m.act(&mut model, Action::SelectRow(presets));
        let saves = SAVES.load(Ordering::SeqCst);
        m.act(&mut model, Action::Activate);
        assert_eq!(
            (SAVES.load(Ordering::SeqCst), m.status.as_deref()),
            (saves + 1, Some("Settings saved"))
        );
        m.act(&mut model, Action::SelectRow(presets + 1));
        m.act(&mut model, Action::Activate);
        assert_eq!(m.status.as_deref(), Some("5 changes undone"));
        assert!(model.pending_restart().is_empty());
        assert_eq!(
            model.get("VICTORY_UPSCALE"),
            model.launched("VICTORY_UPSCALE")
        );
        m.act(&mut model, Action::Activate);
        assert_eq!(m.status.as_deref(), Some("Nothing to undo"));
    }

    #[test]
    fn follow_keeps_the_selection_visible() {
        let mut m = Menu {
            selected: 9,
            ..Menu::default()
        };
        m.follow(4, 12);
        assert_eq!(m.scroll, 6);
        m.selected = 2;
        m.follow(4, 12);
        assert_eq!(m.scroll, 2);
        m.scroll = 11;
        m.follow(4, 12);
        assert_eq!(m.scroll, 2, "scroll never past the last full page");
    }
}

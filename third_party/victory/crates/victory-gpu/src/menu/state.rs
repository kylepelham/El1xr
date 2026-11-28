//! (El1xr) The menu's state and navigation, with no OS calls: tabs from the registry's groups, the
//! selected row, value stepping by kind, presets, and what a key, button or click does.

use super::model::{Applied, Apply, Info, Kind, MenuModel, OTHER, first_pick, named, sections};

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
    if !model.presets().is_empty() {
        out.push(Tab::Presets);
    }
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
    /// A section title in a sectioned tab: drawn, never selected.
    Heading(&'static str),
}

/// Whether `row` can be selected (everything but a heading).
pub fn selectable(row: &Row) -> bool {
    !matches!(row, Row::Heading(_))
}

/// The first row that can be selected.
pub fn first_selectable(rows: &[Row]) -> usize {
    rows.iter().position(selectable).unwrap_or(0)
}

/// How many Down presses go from the first selectable row to row `to`.
pub fn downs_to(rows: &[Row], to: usize) -> usize {
    let from = first_selectable(rows);
    rows.iter()
        .enumerate()
        .filter(|&(i, r)| i > from && i <= to && selectable(r))
        .count()
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
    /// The last save of a restart change failed: the restart band says so.
    pub unsaved: bool,
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
            unsaved: false,
        }
    }
}

impl Menu {
    /// A closed menu on `model`'s first tab when it has no Display tab.
    pub fn for_model(model: &dyn MenuModel) -> Self {
        let mut menu = Menu::default();
        let tabs = menu.tabs(model);
        if !tabs.contains(&menu.tab)
            && let Some(&first) = tabs.first()
        {
            menu.tab = first;
        }
        menu
    }

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
                let rows = self.rows(model);
                if rows.get(self.selected).is_none_or(|r| !selectable(r)) {
                    self.selected = first_selectable(&rows);
                }
                return true;
            }
            return false;
        }
        let rows = self.rows(model);
        match action {
            Action::Toggle | Action::Close => self.open = false,
            Action::Up => {
                if let Some(r) = (0..self.selected).rev().find(|&r| selectable(&rows[r])) {
                    self.selected = r;
                    // Bring the section title above into view with its first row.
                    if r > 0 && !selectable(&rows[r - 1]) {
                        self.scroll = self.scroll.min(r - 1);
                    }
                }
            }
            Action::Down => {
                if let Some(r) = (self.selected + 1..rows.len()).find(|&r| selectable(&rows[r])) {
                    self.selected = r;
                }
            }
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
                Some(Row::Save) => self.status = Some(save_status(model.save())),
                Some(Row::Undo) => self.undo(model),
                Some(Row::Heading(_)) | None => return false,
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
                self.select_tab(model, tabs[next]);
            }
            Action::SelectTab(tab) => {
                if tab == self.tab || !self.tabs(model).contains(&tab) {
                    return false;
                }
                self.select_tab(model, tab);
            }
            Action::SelectRow(row) => {
                if row >= rows.len() || row == self.selected || !selectable(&rows[row]) {
                    return false;
                }
                self.selected = row;
            }
            Action::ToggleExpert => {
                self.expert = !self.expert;
                let tabs = self.tabs(model);
                if !tabs.contains(&self.tab) {
                    self.select_tab(model, tabs[0]);
                }
            }
        }
        true
    }

    fn select_tab(&mut self, model: &dyn MenuModel, tab: Tab) {
        self.tab = tab;
        self.selected = first_selectable(&rows(model, tab, self.expert));
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
        let current = model
            .get(info.name)
            .filter(|v| !v.is_empty())
            .or_else(|| model.effective(info.name))
            .unwrap_or_default();
        let Some(next) = step_value(info.name, info.kind, &current, dir) else {
            self.status = Some(format!("{} is edited in the settings file", info.label));
            return;
        };
        if next != current {
            self.set(model, info, &next);
        }
    }

    /// Sets one value. A change that waits for the next start is written to the settings file at
    /// once: that start is when it counts.
    fn set(&mut self, model: &mut dyn MenuModel, info: Info, value: &str) {
        let shown = shown(info.name, info.kind, value);
        self.status = Some(match model.set(info.name, value) {
            Ok(Applied::Now) => format!("{}: {shown}", info.label),
            Ok(Applied::OnRestart) => {
                let when = if model.apply_of(info.name) == Apply::Retranslate {
                    "at the next start, shaders rebuilt"
                } else {
                    "at the next start"
                };
                format!("{}: {shown} {when}{}", info.label, self.keep(model))
            }
            Err(e) => e.to_string(),
        });
    }

    /// Saves the settings file for changes that wait for the next start; the status suffix.
    fn keep(&mut self, model: &mut dyn MenuModel) -> String {
        match model.save() {
            Ok(()) => {
                self.unsaved = false;
                ", saved".into()
            }
            Err(e) => {
                self.unsaved = true;
                format!("; {}", save_status(Err(e)).to_lowercase())
            }
        }
    }

    /// Whether any of `names` waits for the next start.
    fn any_restart(model: &dyn MenuModel, names: &[&str]) -> bool {
        names.iter().any(|n| model.apply_of(n) != Apply::Live)
    }

    fn apply_preset(&mut self, model: &mut dyn MenuModel, p: usize) {
        let preset = model.presets()[p];
        let failed = preset
            .values
            .iter()
            .filter(|(n, v)| model.set(n, v).is_err())
            .count();
        let names: Vec<&str> = preset.values.iter().map(|(n, _)| *n).collect();
        let kept = if Self::any_restart(model, &names) {
            self.keep(model)
        } else {
            String::new()
        };
        self.status = Some(if failed == 0 {
            format!("{} preset applied{kept}", preset.name)
        } else {
            format!("{} preset: {failed} values refused{kept}", preset.name)
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
        let kept = if Self::any_restart(model, &changed) {
            self.keep(model)
        } else {
            String::new()
        };
        self.status = Some(match changed.len() {
            0 => "Nothing to undo".into(),
            n => format!("{n} changes undone{kept}"),
        });
    }
}

/// What SAVE says: a read-only file or folder and a run without a file in words, other errors as
/// the OS gives them.
fn save_status(saved: std::io::Result<()>) -> String {
    match saved {
        Ok(()) => "Settings saved".into(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            "Not saved: the settings file or its folder is read-only".into()
        }
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            "Not saved: no settings file in this run".into()
        }
        Err(e) => format!("Not saved: {e}"),
    }
}

/// The rows of `tab`: a group's listed settings in the registry's order, or under its section
/// headings when it has sections (the rest under [`OTHER`]).
pub fn rows(model: &dyn MenuModel, tab: Tab, expert: bool) -> Vec<Row> {
    let group = match tab {
        Tab::Presets => {
            let presets = (0..model.presets().len()).map(Row::Preset);
            return presets.chain([Row::Save, Row::Undo]).collect();
        }
        Tab::Group(g) => g,
    };
    let infos = model.infos();
    let at: Vec<usize> = (0..infos.len())
        .filter(|&i| infos[i].group == group && listed(&infos[i], expert))
        .collect();
    let Some(sections) = sections(group) else {
        return at.into_iter().map(Row::Setting).collect();
    };
    let mut out = Vec::new();
    let mut used = vec![false; infos.len()];
    for &(title, names) in sections {
        let found: Vec<usize> = names
            .iter()
            .filter_map(|n| at.iter().copied().find(|&i| infos[i].name == *n))
            .collect();
        if found.is_empty() {
            continue;
        }
        out.push(Row::Heading(title));
        for i in found {
            used[i] = true;
            out.push(Row::Setting(i));
        }
    }
    let rest: Vec<usize> = at.into_iter().filter(|&i| !used[i]).collect();
    if !rest.is_empty() {
        out.push(Row::Heading(OTHER));
        out.extend(rest.into_iter().map(Row::Setting));
    }
    out
}

/// The value one step from `current` in direction `dir` (negative: down), `None` for text.
/// Named values step in their own order; integers on a round step (1, 2 or 5 times a power of
/// ten, about a 48th of the range); choices wrap.
pub fn step_value(name: &str, kind: Kind, current: &str, dir: i32) -> Option<String> {
    let dir = dir.signum();
    if let Some(values) = named(name) {
        let n = values.len() as i32;
        let at = values.iter().position(|&(v, _)| v == current);
        let next = match at {
            Some(at) => (at as i32 + dir).rem_euclid(n) as usize,
            None => first_pick(name)
                .and_then(|s| values.iter().position(|&(v, _)| v == s))
                .unwrap_or(0),
        };
        return Some(values[next].0.to_string());
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
        Kind::Float { step, .. } => match value.trim().parse::<f64>() {
            Ok(v) => format!("{v:.*}", decimals(step).max(2)),
            Err(_) => value.to_string(),
        },
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

    /// The Display tab comes in headed sections, resolution and upscaling first; its settings no
    /// section names follow under Other; Down skips the titles.
    #[test]
    fn the_display_tab_starts_with_the_upscaler_rows() {
        let model = Registry::default();
        let display = rows(&model, Tab::DISPLAY, false);
        let names: Vec<&str> = display
            .iter()
            .take(5)
            .map(|r| match r {
                Row::Setting(i) => model.infos()[*i].name,
                Row::Heading(h) => h,
                _ => "",
            })
            .collect();
        assert_eq!(
            names,
            [
                "Resolution and upscaling",
                "VICTORY_RESOLUTION",
                "VICTORY_UPSCALE",
                "VICTORY_UPSCALE_QUALITY",
                "VICTORY_DLSS_SHARPNESS",
            ]
        );
        let headings: Vec<&str> = display
            .iter()
            .filter_map(|r| match r {
                Row::Heading(h) => Some(*h),
                _ => None,
            })
            .collect();
        assert_eq!(
            &headings[..5],
            ["Resolution and upscaling", "Frame pacing", "Screen and loading", "Lighting", "Overrides"]
        );
        assert!(display.windows(2).all(|w| selectable(&w[0]) || selectable(&w[1])), "no empty section");
        let overlay = row_of(&model, Tab::DISPLAY, "VICTORY_OVERLAY");
        let mut m = Menu::default();
        let mut model = Registry::default();
        m.act(&mut model, Action::Toggle);
        for _ in 0..downs_to(&display, overlay) {
            m.act(&mut model, Action::Down);
        }
        assert_eq!(m.selected, overlay);
        assert!(!m.act(&mut model, Action::SelectRow(overlay - 1)) || selectable(&display[overlay - 1]));
    }

    #[test]
    fn up_down_clamp_tabs_wrap_and_expert_hides_its_tab() {
        let mut model = Registry::default();
        let mut m = opened(&mut model);
        m.act(&mut model, Action::Up);
        assert_eq!(m.selected, 1, "row 0 is a section title");
        assert_eq!(rows(&model, Tab::DISPLAY, false)[0], Row::Heading("Resolution and upscaling"));
        for _ in 0..200 {
            m.act(&mut model, Action::Down);
        }
        assert_eq!(m.selected, m.rows(&model).len() - 1);
        m.act(&mut model, Action::NextTab);
        assert_eq!((m.tab, m.selected), (Tab::Group("Submission"), 0), "no sections there");
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
            "Off"
        );
        // Quality steps from Quality first when unset, then through the modes; Resolution from
        // Native.
        let modes = Kind::Choice(victory_core::render_plan::QUALITIES);
        assert_eq!(step_value("VICTORY_UPSCALE_QUALITY", modes, "", 1).unwrap(), "quality");
        assert_eq!(step_value("VICTORY_UPSCALE_QUALITY", modes, "", -1).unwrap(), "quality");
        assert_eq!(step_value("VICTORY_UPSCALE_QUALITY", modes, "quality", -1).unwrap(), "balanced");
        assert_eq!(step_value("VICTORY_UPSCALE_QUALITY", modes, "dlaa", 1).unwrap(), "ultra_performance");
        assert_eq!(shown("VICTORY_UPSCALE_QUALITY", modes, "ultra_performance"), "Ultra performance");
        let res = Kind::Choice(victory_core::render_plan::RESOLUTIONS);
        assert_eq!(step_value("VICTORY_RESOLUTION", res, "", 1).unwrap(), "native");
        assert_eq!(step_value("VICTORY_RESOLUTION", res, "native", -1).unwrap(), "3840x2160");
        assert_eq!(step_value("VICTORY_RESOLUTION", res, "2560x1440", 1).unwrap(), "3840x2160");
        assert_eq!(shown("VICTORY_RESOLUTION", res, "2560x1440"), "2560 x 1440");
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
            Some("Frame generation: FSR at the next start, saved")
        );
        assert_eq!(model.pending_restart(), ["VICTORY_FRAME_GEN"]);

        m.act(&mut model, Action::SelectTab(Tab::Group("Shaders")));
        let row = row_of(&model, Tab::Group("Shaders"), "VICTORY_SCC_MINMAX");
        m.act(&mut model, Action::SelectRow(row));
        m.act(&mut model, Action::Activate);
        assert_eq!(
            m.status.as_deref(),
            Some("Scc minmax: On at the next start, shaders rebuilt, saved")
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
        assert_eq!(m.status.as_deref(), Some("Quality preset applied, saved"));
        assert_eq!(model.get("VICTORY_UPSCALE_QUALITY").as_deref(), Some("quality"));
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
        assert_eq!(m.status.as_deref(), Some("5 changes undone, saved"));
        assert!(model.pending_restart().is_empty());
        assert_eq!(
            model.get("VICTORY_UPSCALE"),
            model.launched("VICTORY_UPSCALE")
        );
        m.act(&mut model, Action::Activate);
        assert_eq!(m.status.as_deref(), Some("Nothing to undo"));
    }

    /// (rtlight) Ray tracing is a Display row with its RESTART tag: a step turns it on for the
    /// next launch, and clearing it leaves the restart band.
    #[test]
    fn ray_tracing_is_a_display_row_that_waits_for_a_restart() {
        let _lock = registry_lock();
        let name = settings::ray_tracing::NAME;
        if std::env::var_os(name).is_some() {
            return;
        }
        let mut model = Registry::with_save(|| Ok(()));
        let mut m = opened(&mut model);
        assert_eq!(model.apply_of(name), Apply::Restart);
        let row = row_of(&model, Tab::DISPLAY, name);
        m.act(&mut model, Action::SelectRow(row));
        assert_eq!(model.get(name).as_deref(), Some("off"));
        m.act(&mut model, Action::Right);
        assert_eq!(model.get(name).as_deref(), Some("on"));
        assert_eq!(m.status.as_deref(), Some("Ray tracing: On at the next start, saved"));
        assert!(model.pending_restart().contains(&name));
        model.set(name, "").unwrap();
        assert!(!model.pending_restart().contains(&name));
    }

    #[test]
    fn save_says_why_it_failed() {
        let mut model = Registry::with_save(|| Err(std::io::ErrorKind::PermissionDenied.into()));
        let mut m = opened(&mut model);
        m.act(&mut model, Action::SelectTab(Tab::Presets));
        let save = model.presets().len();
        m.act(&mut model, Action::SelectRow(save));
        m.act(&mut model, Action::Activate);
        assert_eq!(
            m.status.as_deref(),
            Some("Not saved: the settings file or its folder is read-only")
        );
        let none = std::io::Error::new(std::io::ErrorKind::Unsupported, "no settings file (VICTORY_SETTINGS_FILE=none)");
        assert_eq!(save_status(Err(none)), "Not saved: no settings file in this run");
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

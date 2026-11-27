//! (El1xr) The menu's state and navigation, with no OS calls: tabs, the selected row, value
//! stepping, presets, and what a key, button or click does.

use super::model::{Applied, Info, Kind, MenuModel, SetError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Display,
    Performance,
    Diagnostics,
    Presets,
    Experimental,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Display,
        Tab::Performance,
        Tab::Diagnostics,
        Tab::Presets,
        Tab::Experimental,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Display => "Display",
            Tab::Performance => "Performance",
            Tab::Diagnostics => "Diagnostics",
            Tab::Presets => "Presets",
            Tab::Experimental => "Experimental",
        }
    }
}

/// The tab a setting is listed under: expert settings under Experimental, the rest by group
/// (unknown groups under Performance).
pub fn tab_of(info: &Info) -> Tab {
    if info.expert {
        return Tab::Experimental;
    }
    match info.group {
        "Display" => Tab::Display,
        "Diagnostics" => Tab::Diagnostics,
        _ => Tab::Performance,
    }
}

/// One line of a tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// Index into the model's `infos()`.
    Setting(usize),
    /// Index into the model's `presets()`.
    Preset(usize),
    Save,
    Reset,
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
            tab: Tab::Display,
            selected: 0,
            scroll: 0,
            expert: false,
            status: None,
        }
    }
}

impl Menu {
    /// The tabs shown: Experimental only with the expert toggle on.
    pub fn tabs(&self) -> Vec<Tab> {
        Tab::ALL
            .into_iter()
            .filter(|&t| t != Tab::Experimental || self.expert)
            .collect()
    }

    pub fn rows(&self, model: &dyn MenuModel) -> Vec<Row> {
        rows(model, self.tab)
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
                    if matches!(info.kind, Kind::Bool | Kind::Choice(_)) {
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
                Some(Row::Reset) => self.reset(model),
                None => return false,
            },
            Action::NextTab | Action::PrevTab => {
                let tabs = self.tabs();
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
                if tab == self.tab || !self.tabs().contains(&tab) {
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
                if !self.expert && self.tab == Tab::Experimental {
                    self.select_tab(Tab::Display);
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
        let current = model
            .get(info.name)
            .unwrap_or_else(|| info.default.to_string());
        let Some(next) = step_value(info.kind, &current, dir) else {
            self.status = Some(format!("{} is edited in the settings file", info.label));
            return;
        };
        if next != current {
            self.set(model, info, &next);
        }
    }

    fn set(&mut self, model: &mut dyn MenuModel, info: Info, value: &str) {
        let (name, label, value_text) = (info.name, info.label, shown(info.kind, value));
        self.status = Some(match model.set(name, value) {
            Ok(Applied::Now) => format!("{label}: {value_text}"),
            Ok(Applied::OnRestart) => format!("{label}: {value_text} after a restart"),
            Err(SetError::Unknown) => format!("{name} is not a setting"),
            Err(SetError::Invalid(why)) => why,
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

    fn reset(&mut self, model: &mut dyn MenuModel) {
        let defaults: Vec<(&'static str, &'static str)> =
            model.infos().iter().map(|i| (i.name, i.default)).collect();
        for (name, value) in defaults {
            if model.get(name).as_deref() != Some(value) {
                let _ = model.set(name, value);
            }
        }
        self.status = Some("Defaults restored".into());
    }
}

/// The rows of `tab`.
pub fn rows(model: &dyn MenuModel, tab: Tab) -> Vec<Row> {
    if tab == Tab::Presets {
        let presets = (0..model.presets().len()).map(Row::Preset);
        return presets.chain([Row::Save, Row::Reset]).collect();
    }
    model
        .infos()
        .iter()
        .enumerate()
        .filter(|(_, info)| tab_of(info) == tab)
        .map(|(i, _)| Row::Setting(i))
        .collect()
}

/// The value one step from `current` in direction `dir` (negative: down), `None` for text.
pub fn step_value(kind: Kind, current: &str, dir: i32) -> Option<String> {
    Some(match kind {
        Kind::Bool => if current == "1" { "0" } else { "1" }.to_string(),
        Kind::Int { min, max } => {
            let step = ((max - min) / 48).max(1);
            let v = current.parse::<i64>().unwrap_or(min);
            (v + step * i64::from(dir.signum()))
                .clamp(min, max)
                .to_string()
        }
        Kind::Float { min, max, step } => {
            let v = current.parse::<f64>().unwrap_or(min);
            let decimals = decimals(step);
            let next = ((v + step * f64::from(dir.signum())) / step).round() * step;
            format!("{:.*}", decimals, next.clamp(min, max))
        }
        Kind::Choice(options) => {
            let n = options.len() as i64;
            let at = options.iter().position(|&o| o == current).unwrap_or(0) as i64;
            options[(at + i64::from(dir.signum())).rem_euclid(n) as usize].to_string()
        }
        Kind::Text => return None,
    })
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

/// How a value reads in the menu.
pub fn shown(kind: Kind, value: &str) -> String {
    match kind {
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
    use super::super::model::DemoModel;
    use super::*;

    fn opened() -> (Menu, DemoModel) {
        let mut m = Menu::default();
        let mut model = DemoModel::default();
        assert!(m.act(&mut model, Action::Toggle));
        (m, model)
    }

    #[test]
    fn closed_menu_ignores_everything_but_toggle() {
        let mut m = Menu::default();
        let mut model = DemoModel::default();
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
        assert_eq!((m.selected, m.tab, m.expert), (0, Tab::Display, false));
        assert_eq!(model.get("VICTORY_UPSCALE").as_deref(), Some("off"));
        assert!(m.act(&mut model, Action::Toggle));
        assert!(m.open);
        assert!(m.act(&mut model, Action::Close));
        assert!(!m.open);
    }

    #[test]
    fn up_down_clamp_and_tabs_wrap() {
        let (mut m, mut model) = opened();
        m.act(&mut model, Action::Up);
        assert_eq!(m.selected, 0);
        for _ in 0..50 {
            m.act(&mut model, Action::Down);
        }
        assert_eq!(m.selected, m.rows(&model).len() - 1);
        m.act(&mut model, Action::NextTab);
        assert_eq!((m.tab, m.selected), (Tab::Performance, 0));
        m.act(&mut model, Action::PrevTab);
        m.act(&mut model, Action::PrevTab);
        assert_eq!(
            m.tab,
            Tab::Presets,
            "wraps past the first tab, Experimental hidden"
        );
        m.act(&mut model, Action::ToggleExpert);
        m.act(&mut model, Action::NextTab);
        assert_eq!(m.tab, Tab::Experimental);
        m.act(&mut model, Action::ToggleExpert);
        assert_eq!(
            m.tab,
            Tab::Display,
            "hiding expert leaves the Experimental tab"
        );
        assert!(!m.act(&mut model, Action::SelectTab(Tab::Experimental)));
    }

    #[test]
    fn expert_rows_only_under_experimental() {
        let model = DemoModel::default();
        for tab in Tab::ALL {
            for row in rows(&model, tab) {
                if let Row::Setting(i) = row {
                    assert_eq!(model.infos()[i].expert, tab == Tab::Experimental);
                }
            }
        }
        assert_eq!(rows(&model, Tab::Presets).last(), Some(&Row::Reset));
    }

    #[test]
    fn values_step_within_their_kind() {
        assert_eq!(step_value(Kind::Bool, "0", 1).unwrap(), "1");
        assert_eq!(
            step_value(Kind::Int { min: 0, max: 240 }, "240", 1).unwrap(),
            "240"
        );
        assert_eq!(
            step_value(Kind::Int { min: 0, max: 240 }, "0", 1).unwrap(),
            "5"
        );
        assert_eq!(
            step_value(
                Kind::Float {
                    min: 0.33,
                    max: 1.0,
                    step: 0.01
                },
                "0.67",
                -1
            )
            .unwrap(),
            "0.66"
        );
        assert_eq!(
            step_value(
                Kind::Float {
                    min: 0.33,
                    max: 1.0,
                    step: 0.01
                },
                "1.00",
                1
            )
            .unwrap(),
            "1.00"
        );
        let c = Kind::Choice(&["a", "b", "c"]);
        assert_eq!(step_value(c, "a", -1).unwrap(), "c");
        assert_eq!(step_value(c, "c", 1).unwrap(), "a");
        assert_eq!(step_value(Kind::Text, "x", 1), None);
        assert_eq!(shown(Kind::Choice(&["dlss"]), "dlss"), "DLSS");
        assert_eq!(shown(Kind::Choice(&["ultra performance"]), "ultra performance"), "Ultra performance");
        assert_eq!(shown(Kind::Int { min: 0, max: 9 }, "0"), "Off");
    }

    #[test]
    fn changing_values_presets_save_and_reset() {
        let (mut m, mut model) = opened();
        m.act(&mut model, Action::Right); // Upscaler off -> dlss
        assert_eq!(model.get("VICTORY_UPSCALE").as_deref(), Some("dlss"));
        assert_eq!(m.status.as_deref(), Some("Upscaler: DLSS"));
        m.act(&mut model, Action::SelectRow(3));
        m.act(&mut model, Action::Activate); // frame generation
        assert_eq!(model.pending_restart(), vec!["VICTORY_FRAME_GEN"]);
        assert!(m.status.as_deref().unwrap().ends_with("after a restart"));
        m.act(&mut model, Action::SelectTab(Tab::Presets));
        m.act(&mut model, Action::Down);
        m.act(&mut model, Action::Activate); // Balanced
        assert_eq!(model.get("VICTORY_DLSS_MODE").as_deref(), Some("quality"));
        assert_eq!(m.status.as_deref(), Some("Balanced preset applied"));
        m.act(&mut model, Action::SelectRow(3));
        m.act(&mut model, Action::Activate);
        assert_eq!(model.saves, 1);
        m.act(&mut model, Action::SelectRow(4));
        m.act(&mut model, Action::Activate);
        assert_eq!(model.get("VICTORY_UPSCALE").as_deref(), Some("off"));
        assert!(model.pending_restart().is_empty());
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

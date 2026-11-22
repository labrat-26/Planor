//! `planor config`: edit `%APPDATA%\planor\planor.toml` in the terminal.
//!
//! Sections on the left, their settings on the right with the current value (changed ones highlighted,
//! colours with a swatch) and the help text from `planor.example.toml` underneath. Edits are validated by
//! the same rule the daemon uses (`crate::valid_value`), saved with comments and layout preserved
//! (`toml_edit`), and the running daemon is told to reload.
//!
//! Keys: Up/Down move, Tab/Left/Right switch pane, Enter edit (toggles a yes/no, cycles a choice),
//! +/- step a number or choice, d default, s save, q quit, ? help.
use crate::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap},
};
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

#[derive(Clone, Debug)]
pub struct Entry {
    pub section: String,
    pub key: String,
    pub default: toml::Value,
    pub help: String,
}

/// Settings model: defaults and help from the embedded example, values from the user's file.
pub struct Model {
    pub path: PathBuf,
    pub doc: DocumentMut,
    pub sections: Vec<String>,
    pub entries: Vec<Entry>,
    pub dirty: bool,
}

fn comment_lines(prefix: Option<&str>) -> String {
    prefix
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim())
        .collect::<Vec<_>>()
        .join(" ")
}

fn to_value(item: &toml_edit::Item) -> Option<toml::Value> {
    let v = item.as_value()?;
    let text = format!("v = {}", v.to_string().trim());
    toml::from_str::<toml::Table>(&text).ok()?.remove("v")
}

fn to_edit(v: &toml::Value) -> toml_edit::Item {
    match v {
        toml::Value::Boolean(b) => toml_edit::value(*b),
        toml::Value::Integer(i) => toml_edit::value(*i),
        toml::Value::Float(f) => toml_edit::value(*f),
        toml::Value::String(s) => toml_edit::value(s.as_str()),
        other => toml_edit::value(other.to_string()),
    }
}

impl Model {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| crate::EXAMPLE.to_string());
        Self::from_text(path, &text)
    }

    pub fn from_text(path: &Path, user: &str) -> Result<Self> {
        let example: DocumentMut = crate::EXAMPLE.parse()?;
        let doc: DocumentMut = user.parse()?;
        let (mut sections, mut entries) = (vec![], vec![]);
        for (name, item) in example.iter() {
            let Some(table) = item.as_table() else { continue }; // [[rules]] is edited in the file
            sections.push(name.to_string());
            let mut first = true;
            for (key, value) in table.iter() {
                let Some(default) = to_value(value) else { continue };
                let mut help = comment_lines(table.key(key).and_then(|k| k.leaf_decor().prefix()).and_then(|p| p.as_str()));
                if first && help.is_empty() {
                    help = comment_lines(table.decor().suffix().and_then(|p| p.as_str()));
                }
                first = false;
                entries.push(Entry { section: name.to_string(), key: key.to_string(), default, help });
            }
        }
        Ok(Self { path: path.to_path_buf(), doc, sections, entries, dirty: false })
    }

    pub fn in_section(&self, section: &str) -> Vec<usize> {
        (0..self.entries.len()).filter(|&i| self.entries[i].section == section).collect()
    }

    /// The value in effect: the user's when it is valid (what the daemon would use), else the default.
    pub fn value(&self, i: usize) -> toml::Value {
        let e = &self.entries[i];
        self.doc
            .get(&e.section)
            .and_then(|t| t.get(&e.key))
            .and_then(to_value)
            .filter(|v| crate::valid_value(&e.key, &e.default, v))
            .unwrap_or_else(|| e.default.clone())
    }

    pub fn set(&mut self, i: usize, v: toml::Value) -> std::result::Result<(), String> {
        let e = self.entries[i].clone();
        // An integer typed for a decimal setting is fine (2 -> 2.0).
        let v = match (&e.default, v) {
            (toml::Value::Float(_), toml::Value::Integer(n)) => toml::Value::Float(n as f64),
            (_, v) => v,
        };
        if !crate::valid_value(&e.key, &e.default, &v) {
            return Err(match crate::choices(&e.key) {
                Some(c) => format!("{} must be one of: {}", e.key, c.join(", ")),
                None if e.key.ends_with("_color") => format!("{} must be a colour like #3B82F6", e.key),
                None => format!("{} needs a {} ({})", e.key, e.default.type_str(), range_hint(&e)),
            });
        }
        if self.value(i) == v {
            return Ok(());
        }
        if self.doc.get(&e.section).and_then(|t| t.as_table()).is_none() {
            self.doc.insert(&e.section, toml_edit::table());
        }
        let table = self.doc[&e.section].as_table_mut().ok_or("not a table")?;
        // Keep the comment above the key when it already exists.
        match table.get_mut(&e.key).and_then(|it| it.as_value_mut()) {
            Some(slot) => {
                let decor = slot.decor().clone();
                *slot = to_edit(&v).into_value().map_err(|_| "bad value")?;
                *slot.decor_mut() = decor;
            }
            None => {
                table.insert(&e.key, to_edit(&v));
            }
        }
        self.dirty = true;
        Ok(())
    }

    /// Parse typed text for setting `i` (quotes optional for words).
    pub fn parse_input(&self, i: usize, text: &str) -> std::result::Result<toml::Value, String> {
        let t = text.trim();
        Ok(match &self.entries[i].default {
            toml::Value::Boolean(_) => match t.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => toml::Value::Boolean(true),
                "false" | "no" | "off" | "0" => toml::Value::Boolean(false),
                _ => return Err("type yes or no".into()),
            },
            toml::Value::Integer(_) => toml::Value::Integer(t.parse().map_err(|_| "type a whole number")?),
            toml::Value::Float(_) => toml::Value::Float(t.parse().map_err(|_| "type a number")?),
            _ => toml::Value::String(t.trim_matches('"').to_string()),
        })
    }

    /// +1 / -1: step a number, cycle a choice, flip a yes/no.
    pub fn step(&mut self, i: usize, dir: i64) -> std::result::Result<(), String> {
        let e = &self.entries[i];
        let v = match (self.value(i), crate::choices(&e.key)) {
            (toml::Value::Boolean(b), _) => toml::Value::Boolean(!b),
            (toml::Value::Integer(n), _) => {
                let unit = if n.abs() >= 1000 { 100 } else if n.abs() >= 100 { 10 } else { 1 };
                toml::Value::Integer((n + dir * unit).max(0))
            }
            (toml::Value::Float(f), _) => {
                let unit = if f <= 1. { 0.05 } else { 0.5 };
                toml::Value::Float((((f + dir as f64 * unit) * 100.).round() / 100.).max(0.05))
            }
            (toml::Value::String(s), Some(c)) => {
                let at = c.iter().position(|x| *x == s).unwrap_or(0) as i64;
                toml::Value::String(c[(at + dir).rem_euclid(c.len() as i64) as usize].to_string())
            }
            _ => return Err("type a new value with Enter".into()),
        };
        self.set(i, v)
    }

    pub fn reset(&mut self, i: usize) -> std::result::Result<(), String> {
        let d = self.entries[i].default.clone();
        self.set(i, d)
    }

    pub fn save(&mut self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, self.doc.to_string())?;
        self.dirty = false;
        Ok(())
    }
}

fn range_hint(e: &Entry) -> &'static str {
    match &e.default {
        toml::Value::Integer(_) => "0 to 100000",
        toml::Value::Float(_) if e.key == "default_tile_share" => "above 0, at most 1",
        toml::Value::Float(_) => "above 0",
        _ => "text",
    }
}

fn shown(v: &toml::Value) -> String {
    match v {
        toml::Value::Boolean(true) => "yes".into(),
        toml::Value::Boolean(false) => "no".into(),
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn colour(v: &toml::Value) -> Option<Color> {
    let s = v.as_str()?.strip_prefix('#')?;
    let n = u32::from_str_radix(s, 16).ok()?;
    (s.len() == 6).then(|| Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

#[derive(PartialEq)]
enum Pane {
    Sections,
    Keys,
}

pub struct App {
    pub model: Model,
    section: usize,
    key: usize,
    pane: Pane,
    editing: Option<String>,
    status: String,
    confirm_quit: bool,
    help: bool,
    pub quit: bool,
}

impl App {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            section: 0,
            key: 0,
            pane: Pane::Keys,
            editing: None,
            status: "Enter edit · +/- step · d default · s save · q quit · ? help".into(),
            confirm_quit: false,
            help: false,
            quit: false,
        }
    }

    fn rows(&self) -> Vec<usize> {
        self.model.sections.get(self.section).map(|s| self.model.in_section(s)).unwrap_or_default()
    }

    fn selected(&self) -> Option<usize> {
        self.rows().get(self.key).copied()
    }

    fn report(&mut self, r: std::result::Result<(), String>) {
        match r {
            Ok(()) => {
                if let Some(i) = self.selected() {
                    let e = &self.model.entries[i];
                    self.status = format!("{}.{} = {} (not saved: s to save)", e.section, e.key, shown(&self.model.value(i)));
                }
            }
            Err(e) => self.status = format!("! {e}"),
        }
    }

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if let Some(buf) = self.editing.as_mut() {
            match code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.status = "edit cancelled".into();
                }
                KeyCode::Enter => {
                    let text = self.editing.take().unwrap_or_default();
                    if let Some(i) = self.selected() {
                        let r = self.model.parse_input(i, &text).and_then(|v| self.model.set(i, v));
                        self.report(r);
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => (),
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        let quit_armed = std::mem::take(&mut self.confirm_quit);
        let rows = self.rows().len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if !self.model.dirty || quit_armed {
                    self.quit = true;
                } else {
                    self.confirm_quit = true;
                    self.status = "! unsaved changes: s to save, q again to quit without saving".into();
                }
            }
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = if self.pane == Pane::Keys { Pane::Sections } else { Pane::Keys };
            }
            KeyCode::Left if self.pane == Pane::Keys => self.pane = Pane::Sections,
            KeyCode::Right if self.pane == Pane::Sections => self.pane = Pane::Keys,
            KeyCode::Up | KeyCode::Char('k') => match self.pane {
                Pane::Sections => {
                    self.section = self.section.saturating_sub(1);
                    self.key = 0;
                }
                Pane::Keys => self.key = self.key.saturating_sub(1),
            },
            KeyCode::Down | KeyCode::Char('j') => match self.pane {
                Pane::Sections => {
                    self.section = (self.section + 1).min(self.model.sections.len().saturating_sub(1));
                    self.key = 0;
                }
                Pane::Keys => self.key = (self.key + 1).min(rows.saturating_sub(1)),
            },
            KeyCode::Enter if self.pane == Pane::Sections => self.pane = Pane::Keys,
            KeyCode::Enter => {
                if let Some(i) = self.selected() {
                    let e = &self.model.entries[i];
                    if e.default.is_bool() || crate::choices(&e.key).is_some() {
                        let r = self.model.step(i, 1);
                        self.report(r);
                    } else {
                        self.editing = Some(shown(&self.model.value(i)));
                        self.status = "type the new value, Enter to apply, Esc to cancel".into();
                    }
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Char('-') | KeyCode::Char('_') => {
                if let Some(i) = self.selected() {
                    let dir = if matches!(code, KeyCode::Char('-') | KeyCode::Char('_')) { -1 } else { 1 };
                    let r = self.model.step(i, dir);
                    self.report(r);
                }
            }
            KeyCode::Char('d') => {
                if let Some(i) = self.selected() {
                    let r = self.model.reset(i);
                    self.report(r);
                }
            }
            KeyCode::Char('s') => match self.model.save() {
                Ok(()) => {
                    let reloaded = crate::client(&crate::pipe_name(), &crate::Command::Reload).is_ok();
                    self.status = if reloaded {
                        format!("saved {} · daemon reloaded", self.model.path.display())
                    } else {
                        format!("saved {} · daemon not running (it reads the file when it starts)", self.model.path.display())
                    };
                }
                Err(e) => self.status = format!("! save failed: {e}"),
            },
            _ => (),
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        let [top, body, info, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(6),
            Constraint::Length(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let title = format!(
            " Planor settings · {}{}",
            self.model.path.display(),
            if self.model.dirty { " · modified" } else { "" }
        );
        f.render_widget(Paragraph::new(title).style(Style::new().bold().fg(Color::Cyan)), top);

        let [left, right] = Layout::horizontal([Constraint::Length(16), Constraint::Min(20)]).areas(body);
        let active = |p: Pane| if self.pane == p { Style::new().fg(Color::Cyan) } else { Style::new().fg(Color::DarkGray) };
        let sections: Vec<ListItem> = self.model.sections.iter().map(|s| ListItem::new(s.as_str())).collect();
        let mut ls = ListState::default().with_selected(Some(self.section));
        f.render_stateful_widget(
            List::new(sections)
                .block(Block::new().borders(Borders::ALL).title(" sections ").border_style(active(Pane::Sections)))
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            left,
            &mut ls,
        );

        let rows: Vec<Row> = self
            .rows()
            .into_iter()
            .map(|i| {
                let e = &self.model.entries[i];
                let v = self.model.value(i);
                let changed = v != e.default;
                let mut value = vec![Span::raw(shown(&v))];
                if let Some(c) = colour(&v) {
                    value.insert(0, Span::styled("■ ", Style::new().fg(c)));
                }
                let def = if changed { format!("default {}", shown(&e.default)) } else { String::new() };
                let style = if changed { Style::new().fg(Color::Yellow) } else { Style::new() };
                Row::new(vec![Line::from(e.key.clone()), Line::from(value), Line::from(def).dark_gray()]).style(style)
            })
            .collect();
        let mut ts = TableState::default().with_selected(Some(self.key));
        let name = self.model.sections.get(self.section).cloned().unwrap_or_default();
        f.render_stateful_widget(
            Table::new(rows, [Constraint::Length(30), Constraint::Length(22), Constraint::Min(10)])
                .block(Block::new().borders(Borders::ALL).title(format!(" {name} ")).border_style(active(Pane::Keys)))
                .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
            right,
            &mut ts,
        );

        let text = if self.help {
            "Up/Down move · Tab or Left/Right switch pane · Enter edit (yes/no and choices toggle) · +/- step a \
             number or choice · d back to default · s save (the daemon reloads at once) · q quit. Rules ([[rules]]) \
             are edited in the file itself."
                .to_string()
        } else if let Some(buf) = &self.editing {
            format!("new value: {buf}▏")
        } else {
            self.selected()
                .map(|i| {
                    let e = &self.model.entries[i];
                    let h = if e.help.is_empty() { "No description." } else { e.help.as_str() };
                    format!("{}.{}: {h}", e.section, e.key)
                })
                .unwrap_or_default()
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(Block::new().borders(Borders::ALL)), info);
        let st = if self.status.starts_with('!') { Style::new().fg(Color::Red) } else { Style::new().fg(Color::Gray) };
        f.render_widget(Paragraph::new(self.status.clone()).style(st), foot);
    }
}

pub fn run(path: &Path) -> Result<()> {
    let model = Model::load(path)?;
    let mut app = App::new(model);
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|f| app.draw(f))?;
            if let Event::Key(k) = event::read()? {
                // Windows reports presses and releases; act on presses (and auto-repeat) only.
                if k.kind != KeyEventKind::Release {
                    app.key(k.code, k.modifiers);
                }
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn model() -> Model {
        Model::from_text(Path::new("planor.toml"), crate::EXAMPLE).unwrap()
    }
    fn idx(m: &Model, s: &str, k: &str) -> usize {
        m.entries.iter().position(|e| e.section == s && e.key == k).unwrap()
    }

    #[test]
    fn loads_every_section_and_key_of_the_example_with_help() {
        let m = model();
        for s in ["layout", "borders", "animation", "touchpad", "mouse", "diagnostics", "hotkeys"] {
            assert!(m.sections.contains(&s.to_string()), "section {s}: {:?}", m.sections);
        }
        assert!(!m.sections.contains(&"rules".to_string()), "[[rules]] stays in the file");
        let z = idx(&m, "layout", "stack_zone_px");
        assert_eq!(m.value(z), toml::Value::Integer(120));
        assert!(m.entries[z].help.contains("stack zone"), "help: {}", m.entries[z].help);
    }

    #[test]
    fn edits_are_validated_like_the_daemon_and_saved_with_comments() {
        let mut m = model();
        let gap = idx(&m, "layout", "gap_px");
        assert!(m.set(gap, toml::Value::Integer(-4)).is_err());
        assert!(m.set(gap, toml::Value::String("wide".into())).is_err());
        m.set(gap, toml::Value::Integer(10)).unwrap();
        let share = idx(&m, "layout", "default_tile_share");
        assert!(m.set(share, toml::Value::Float(1.5)).is_err(), "share is at most 1");
        let colour = idx(&m, "borders", "focused_color");
        assert!(m.set(colour, toml::Value::String("blue".into())).is_err());
        m.set(colour, toml::Value::String("#112233".into())).unwrap();
        let curve = idx(&m, "animation", "curve");
        m.step(curve, 1).unwrap();
        assert_eq!(m.value(curve), toml::Value::String("spring".into()));
        let inertia = idx(&m, "touchpad", "swipe_inertia");
        m.step(inertia, 1).unwrap();
        assert_eq!(m.value(inertia), toml::Value::Boolean(false));
        let gain = idx(&m, "touchpad", "swipe_scroll_gain");
        m.set(gain, toml::Value::Integer(3)).unwrap();
        assert_eq!(m.value(gain), toml::Value::Float(3.0), "an integer typed for a decimal setting");
        assert!(m.dirty);
        let text = m.doc.to_string();
        assert!(text.contains("gap_px = 10"));
        assert!(text.contains("focused_color = \"#112233\""));
        assert!(text.contains("# A new window opens on the monitor under the mouse cursor."), "comments kept");
        // What the TUI writes is what the daemon reads.
        let cfg = crate::parse(&text).unwrap();
        assert_eq!(cfg["layout"]["gap_px"].as_integer(), Some(10));
        assert_eq!(cfg["animation"]["curve"].as_str(), Some("spring"));
        assert_eq!(cfg["touchpad"]["swipe_inertia"].as_bool(), Some(false));
        m.reset(gap).unwrap();
        assert_eq!(m.value(gap), toml::Value::Integer(6));
    }

    #[test]
    fn a_user_file_with_only_a_few_keys_and_an_invalid_one() {
        let m = Model::from_text(Path::new("p.toml"), "[layout]\ngap_px = 12\nstack_zone_px = -3\n").unwrap();
        assert_eq!(m.value(idx(&m, "layout", "gap_px")), toml::Value::Integer(12));
        assert_eq!(m.value(idx(&m, "layout", "stack_zone_px")), toml::Value::Integer(120), "invalid -> default, as the daemon does");
        assert_eq!(m.value(idx(&m, "borders", "width_px")), toml::Value::Integer(2), "missing -> default");
    }

    #[test]
    fn keyboard_flow_and_screen() {
        let mut app = App::new(model());
        let render = |app: &App| {
            let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let buf = t.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let screen = render(&app);
        assert!(screen.contains("sections") && screen.contains("layout") && screen.contains("gap_px"), "{screen}");
        // Down to gap_px in [layout], Enter, type 9, Enter.
        let at = app.model.in_section("layout").iter().position(|&i| app.model.entries[i].key == "gap_px").unwrap();
        for _ in 0..at {
            app.key(KeyCode::Down, KeyModifiers::NONE);
        }
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let screen = render(&app);
        assert!(screen.contains("new value:"), "{screen}");
        for _ in 0..4 {
            app.key(KeyCode::Backspace, KeyModifiers::NONE);
        }
        app.key(KeyCode::Char('9'), KeyModifiers::NONE);
        app.key(KeyCode::Enter, KeyModifiers::NONE);
        let gap = idx(&app.model, "layout", "gap_px");
        assert_eq!(app.model.value(gap), toml::Value::Integer(9));
        let screen = render(&app);
        assert!(screen.contains("modified") && screen.contains("default 6"), "{screen}");
        // q with unsaved changes asks first; the second q quits.
        app.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!app.quit);
        app.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit);
    }
}

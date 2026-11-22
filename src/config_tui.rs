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
use crate::settings_model::Model;
use std::path::Path;

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
    (s.len() == 6).then_some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
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

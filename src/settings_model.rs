//! Settings model shared by the settings window (and the legacy TUI until it is removed): defaults, help
//! text and sections come from the embedded `planor.example.toml`, values from the user's file. Edits are
//! validated by the daemon's own rule (`crate::valid_value`) and saved with comments preserved (`toml_edit`).
use crate::Result;
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

pub(crate) fn comment_lines(prefix: Option<&str>) -> String {
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
            let Some(table) = item.as_table() else {
                continue;
            }; // [[rules]] is edited in the file
            sections.push(name.to_string());
            let mut first = true;
            for (key, value) in table.iter() {
                let Some(default) = to_value(value) else {
                    continue;
                };
                let mut help = comment_lines(
                    table
                        .key(key)
                        .and_then(|k| k.leaf_decor().prefix())
                        .and_then(|p| p.as_str()),
                );
                if first && help.is_empty() {
                    help = comment_lines(table.decor().suffix().and_then(|p| p.as_str()));
                }
                first = false;
                entries.push(Entry {
                    section: name.to_string(),
                    key: key.to_string(),
                    default,
                    help,
                });
            }
        }
        Ok(Self {
            path: path.to_path_buf(),
            doc,
            sections,
            entries,
            dirty: false,
        })
    }

    pub fn in_section(&self, section: &str) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| self.entries[i].section == section)
            .collect()
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
                None if e.key.ends_with("_color") => {
                    format!("{} must be a colour like #3B82F6", e.key)
                }
                None => format!(
                    "{} needs a {} ({})",
                    e.key,
                    e.default.type_str(),
                    range_hint(&e)
                ),
            });
        }
        if self.value(i) == v {
            return Ok(());
        }
        if self
            .doc
            .get(&e.section)
            .and_then(|t| t.as_table())
            .is_none()
        {
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
            toml::Value::Integer(_) => {
                toml::Value::Integer(t.parse().map_err(|_| "type a whole number")?)
            }
            toml::Value::Float(_) => toml::Value::Float(t.parse().map_err(|_| "type a number")?),
            _ => toml::Value::String(t.trim_matches('"').to_string()),
        })
    }

    /// The value `step` would set: one notch up (+1) or down (-1) for a number, the next word of a choice,
    /// the opposite of a yes/no. Does not change anything.
    pub fn stepped(&self, i: usize, dir: i64) -> std::result::Result<toml::Value, String> {
        let e = &self.entries[i];
        Ok(match (self.value(i), crate::choices(&e.key)) {
            (toml::Value::Boolean(b), _) => toml::Value::Boolean(!b),
            (toml::Value::Integer(n), _) => {
                let unit = if n.abs() >= 1000 {
                    100
                } else if n.abs() >= 100 {
                    10
                } else {
                    1
                };
                let max = match e.key.as_str() {
                    "drag_dwell_ms" => 5000,
                    "drag_vacancy_delay_ms" => 10000,
                    "drag_edge_scroll_percent" => 40,
                    _ => 100000,
                };
                toml::Value::Integer((n + dir * unit).clamp(0, max))
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
        })
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

pub(crate) fn range_hint(e: &Entry) -> &'static str {
    match &e.default {
        toml::Value::Integer(_) if e.key == "drag_dwell_ms" => "0 to 5000 ms",
        toml::Value::Integer(_) if e.key == "drag_vacancy_delay_ms" => "0 to 10000 ms",
        toml::Value::Integer(_) if e.key == "drag_edge_scroll_percent" => "0 to 40 %",
        toml::Value::Integer(_) => "0 to 100000",
        toml::Value::Float(_) if e.key == "default_tile_share" => "above 0, at most 1",
        toml::Value::Float(_) => "above 0",
        _ => "text",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleMatch {
    Process,
    Class,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleAction {
    Ignore,
    Float,
    Tile,
}
/// One `[[rules]]` row: a window is matched by process name or window class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub matcher: RuleMatch,
    pub value: String,
    pub action: RuleAction,
}
impl RuleAction {
    pub const ALL: [RuleAction; 3] = [RuleAction::Ignore, RuleAction::Float, RuleAction::Tile];
    pub fn word(self) -> &'static str {
        match self {
            RuleAction::Ignore => "ignore",
            RuleAction::Float => "float",
            RuleAction::Tile => "tile",
        }
    }
}
impl RuleMatch {
    pub fn key(self) -> &'static str {
        match self {
            RuleMatch::Process => "process",
            RuleMatch::Class => "class",
        }
    }
}

/// Whether `s` is a hotkey the daemon can bind (the grammar of `input::bindings`): any of `Alt`, `Ctrl`,
/// `Shift`, `Win`, plus exactly one key: `Left`, `Up`, `Right`, `Down`, `Backspace`, `[`, `]` or one character.
pub fn hotkey_valid(s: &str) -> bool {
    let mut keys = 0;
    for token in s.split('+') {
        match token.to_ascii_uppercase().as_str() {
            "ALT" | "CTRL" | "SHIFT" | "WIN" => (),
            "LEFT" | "UP" | "RIGHT" | "DOWN" | "BACKSPACE" => keys += 1,
            t if t.chars().count() == 1 && t != "+" => keys += 1,
            _ => return false,
        }
    }
    keys == 1
}

impl Model {
    /// Re-read the file, dropping the in-memory document: a save then starts from what is on disk, so a
    /// hand edit made while the window was open is kept.
    pub fn reload_from_disk(&mut self) -> Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        self.doc = text.parse()?;
        self.dirty = false;
        Ok(())
    }

    /// `Some(message)` when the file exists but is not valid TOML (the window then refuses to edit it).
    pub fn load_error(path: &Path) -> Option<String> {
        let text = std::fs::read_to_string(path).ok()?;
        text.parse::<DocumentMut>().err().map(|e| e.to_string())
    }

    fn rule_from(t: &toml_edit::Table) -> Option<Rule> {
        let action = match t.get("action")?.as_str()? {
            "ignore" => RuleAction::Ignore,
            "float" => RuleAction::Float,
            "tile" => RuleAction::Tile,
            _ => return None,
        };
        let (matcher, value) = match (
            t.get("process").and_then(|v| v.as_str()),
            t.get("class").and_then(|v| v.as_str()),
        ) {
            (Some(p), _) => (RuleMatch::Process, p),
            (None, Some(c)) => (RuleMatch::Class, c),
            _ => return None,
        };
        Some(Rule {
            matcher,
            value: value.to_string(),
            action,
        })
    }

    /// The rule rows in effect, each with the table it came from. A file with no `rules` key gets the example's
    /// rules (that is what the daemon uses then); `rules = []` means none.
    fn existing_rows(&self) -> Vec<(Rule, toml_edit::Table)> {
        let from = |doc: &DocumentMut| -> Vec<(Rule, toml_edit::Table)> {
            doc.get("rules")
                .and_then(|r| r.as_array_of_tables())
                .map(|rows| {
                    rows.iter()
                        .filter_map(|t| Self::rule_from(t).map(|r| (r, t.clone())))
                        .collect()
                })
                .unwrap_or_default()
        };
        match self.doc.get("rules") {
            Some(_) => from(&self.doc),
            None => crate::EXAMPLE
                .parse::<DocumentMut>()
                .map(|d| from(&d))
                .unwrap_or_default(),
        }
    }

    /// The `[[rules]]` rows the daemon would accept.
    pub fn rules(&self) -> Vec<Rule> {
        self.existing_rows().into_iter().map(|(r, _)| r).collect()
    }

    /// Replace the rules. A row that is still there keeps its table (so a rule matching both a process and a class
    /// keeps both, and comments and other keys survive); no rules at all is written as `rules = []`.
    pub fn set_rules(&mut self, rules: &[Rule]) -> std::result::Result<(), String> {
        if rules.iter().any(|r| r.value.trim().is_empty()) {
            return Err("a rule needs a process or class name".into());
        }
        if self.rules() == rules {
            return Ok(());
        }
        let prefix = self
            .doc
            .get("rules")
            .and_then(|r| r.as_array_of_tables())
            .and_then(|a| a.iter().next())
            .and_then(|t| t.decor().prefix().cloned());
        let mut old: Vec<Option<(Rule, toml_edit::Table)>> =
            self.existing_rows().into_iter().map(Some).collect();
        let mut rows = toml_edit::ArrayOfTables::new();
        for (n, r) in rules.iter().enumerate() {
            let reuse = old.iter().position(|o| {
                o.as_ref()
                    .is_some_and(|(or, _)| or.matcher == r.matcher && or.value == r.value.trim())
            });
            let table = match reuse.and_then(|at| old[at].take()) {
                Some((_, mut t)) => {
                    t["action"] = toml_edit::value(r.action.word());
                    t
                }
                None => {
                    let mut t = toml_edit::Table::new();
                    t.insert(r.matcher.key(), toml_edit::value(r.value.trim()));
                    t.insert("action", toml_edit::value(r.action.word()));
                    if n == 0
                        && let Some(p) = &prefix
                    {
                        t.decor_mut().set_prefix(p.clone());
                    }
                    t
                }
            };
            rows.push(table);
        }
        if rows.is_empty() {
            self.doc.insert(
                "rules",
                toml_edit::Item::Value(toml_edit::Value::Array(toml_edit::Array::new())),
            );
        } else {
            self.doc
                .insert("rules", toml_edit::Item::ArrayOfTables(rows));
        }
        self.dirty = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model::from_text(Path::new("planor.toml"), crate::EXAMPLE).unwrap()
    }
    fn idx(m: &Model, s: &str, k: &str) -> usize {
        m.entries
            .iter()
            .position(|e| e.section == s && e.key == k)
            .unwrap()
    }
    fn temp(name: &str, text: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("planor-{name}-{}.toml", std::process::id()));
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn drag_axis_lock_is_listed_and_on_by_default() {
        let m = model();
        let i = idx(&m, "touchpad", "drag_axis_lock");
        assert_eq!(m.value(i), toml::Value::Boolean(true));
        assert!(!m.entries[i].help.is_empty());
    }

    #[test]
    fn loads_every_section_and_key_of_the_example_with_help() {
        let m = model();
        for s in [
            "layout",
            "borders",
            "animation",
            "touchpad",
            "mouse",
            "diagnostics",
            "hotkeys",
        ] {
            assert!(
                m.sections.contains(&s.to_string()),
                "section {s}: {:?}",
                m.sections
            );
        }
        assert!(
            !m.sections.contains(&"rules".to_string()),
            "[[rules]] has its own editor"
        );
        let z = idx(&m, "layout", "drag_dwell_ms");
        assert_eq!(m.value(z), toml::Value::Integer(250));
        assert!(
            m.entries[z].help.contains("dwell"),
            "help: {}",
            m.entries[z].help
        );
    }

    #[test]
    fn edits_are_validated_like_the_daemon_and_saved_with_comments() {
        let mut m = model();
        let gap = idx(&m, "layout", "gap_px");
        assert!(m.set(gap, toml::Value::Integer(-4)).is_err());
        assert!(m.set(gap, toml::Value::String("wide".into())).is_err());
        m.set(gap, toml::Value::Integer(10)).unwrap();
        let share = idx(&m, "layout", "default_tile_share");
        assert!(
            m.set(share, toml::Value::Float(1.5)).is_err(),
            "share is at most 1"
        );
        let colour = idx(&m, "borders", "focused_color");
        assert!(m.set(colour, toml::Value::String("blue".into())).is_err());
        m.set(colour, toml::Value::String("#112233".into()))
            .unwrap();
        let curve = idx(&m, "animation", "curve");
        let next = m.stepped(curve, 1).unwrap();
        m.set(curve, next).unwrap();
        assert_eq!(m.value(curve), toml::Value::String("spring".into()));
        let inertia = idx(&m, "touchpad", "swipe_inertia");
        let flipped = m.stepped(inertia, 1).unwrap();
        m.set(inertia, flipped).unwrap();
        assert_eq!(m.value(inertia), toml::Value::Boolean(false));
        let gain = idx(&m, "touchpad", "swipe_scroll_gain");
        m.set(gain, toml::Value::Integer(3)).unwrap();
        assert_eq!(
            m.value(gain),
            toml::Value::Float(3.0),
            "an integer typed for a decimal setting"
        );
        assert!(m.dirty);
        let text = m.doc.to_string();
        assert!(text.contains("gap_px = 10"));
        assert!(text.contains("focused_color = \"#112233\""));
        assert!(
            text.contains("# A new window opens on the monitor under the mouse cursor."),
            "comments kept"
        );
        let cfg = crate::parse(&text).unwrap();
        assert_eq!(cfg["layout"]["gap_px"].as_integer(), Some(10));
        assert_eq!(cfg["animation"]["curve"].as_str(), Some("spring"));
        assert_eq!(cfg["touchpad"]["swipe_inertia"].as_bool(), Some(false));
        let default = m.entries[gap].default.clone();
        m.set(gap, default).unwrap();
        assert_eq!(m.value(gap), toml::Value::Integer(6));
    }

    #[test]
    fn a_user_file_with_only_a_few_keys_and_an_invalid_one() {
        let m = Model::from_text(
            Path::new("p.toml"),
            "[layout]\ngap_px = 12\ndrag_dwell_ms = -3\n",
        )
        .unwrap();
        assert_eq!(
            m.value(idx(&m, "layout", "gap_px")),
            toml::Value::Integer(12)
        );
        assert_eq!(
            m.value(idx(&m, "layout", "drag_dwell_ms")),
            toml::Value::Integer(250),
            "invalid -> default"
        );
        assert_eq!(
            m.value(idx(&m, "borders", "width_px")),
            toml::Value::Integer(2),
            "missing -> default"
        );
    }

    #[test]
    fn stepped_previews_without_changing_anything() {
        let m = model();
        let (gap, share, curve) = (
            idx(&m, "layout", "gap_px"),
            idx(&m, "layout", "default_tile_share"),
            idx(&m, "animation", "curve"),
        );
        assert_eq!(m.stepped(gap, 1).unwrap(), toml::Value::Integer(7));
        assert_eq!(m.stepped(gap, -1).unwrap(), toml::Value::Integer(5));
        assert_eq!(m.stepped(share, 1).unwrap(), toml::Value::Float(0.55));
        assert_eq!(
            m.stepped(curve, 1).unwrap(),
            toml::Value::String("spring".into())
        );
        assert_eq!(m.value(gap), toml::Value::Integer(6));
        assert!(!m.dirty);
    }
    #[test]
    fn hotkey_valid_accepts_all_example_hotkeys() {
        let cfg: toml::Value = toml::from_str(crate::EXAMPLE).unwrap();
        for (name, v) in cfg["hotkeys"].as_table().unwrap() {
            assert!(hotkey_valid(v.as_str().unwrap()), "{name} = {v}");
        }
    }

    #[test]
    fn hotkey_valid_rejects() {
        for bad in ["", "Alt+", "Foo+A", "Alt+Left+Right", "Alt", "+A", "Alt++"] {
            assert!(!hotkey_valid(bad), "{bad:?}");
        }
        for good in ["alt+shift+s", "Win+Ctrl+]", "Left", "Ctrl+Alt+Backspace"] {
            assert!(hotkey_valid(good), "{good:?}");
        }
    }

    #[test]
    fn rules_round_trip_preserves_comments() {
        let text = format!("{}\n# my own note\n", crate::EXAMPLE);
        let mut m = Model::from_text(Path::new("planor.toml"), &text).unwrap();
        assert_eq!(
            m.rules(),
            vec![Rule {
                matcher: RuleMatch::Process,
                value: "Taskmgr.exe".into(),
                action: RuleAction::Ignore
            }]
        );
        let mut rules = m.rules();
        rules.push(Rule {
            matcher: RuleMatch::Class,
            value: "Shell_Dialog".into(),
            action: RuleAction::Float,
        });
        m.set_rules(&rules).unwrap();
        let out = m.doc.to_string();
        assert!(out.contains("# A new window opens on the monitor under the mouse cursor."));
        assert!(out.contains("# my own note"));
        let cfg = crate::parse(&out).unwrap();
        let parsed = cfg["rules"].as_array().unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1]["class"].as_str(), Some("Shell_Dialog"));
        assert_eq!(parsed[1]["action"].as_str(), Some("float"));
        assert!(
            m.set_rules(&[Rule {
                matcher: RuleMatch::Process,
                value: "  ".into(),
                action: RuleAction::Tile
            }])
            .is_err()
        );
    }

    #[test]
    fn rules_with_both_keys_survive_other_edits() {
        let text = "[[rules]]\nprocess = \"Code.exe\"\nclass = \"#32770\"\naction = \"float\"\n";
        let mut m = Model::from_text(Path::new("planor.toml"), text).unwrap();
        let mut rules = m.rules();
        assert_eq!(rules.len(), 1);
        rules.push(Rule {
            matcher: RuleMatch::Process,
            value: "Calc.exe".into(),
            action: RuleAction::Ignore,
        });
        m.set_rules(&rules).unwrap();
        let cfg = crate::parse(&m.doc.to_string()).unwrap();
        let rows = cfg["rules"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0]["class"].as_str(),
            Some("#32770"),
            "the class half of the match is kept: {rows:?}"
        );
        assert_eq!(rows[0]["process"].as_str(), Some("Code.exe"));
        assert_eq!(rows[1]["process"].as_str(), Some("Calc.exe"));
    }

    #[test]
    fn changing_an_action_keeps_the_rows_other_keys_and_comments() {
        let text = "[[rules]]\n# keep me\nprocess = \"Code.exe\"\nclass = \"#32770\"\naction = \"float\"\n";
        let mut m = Model::from_text(Path::new("planor.toml"), text).unwrap();
        let mut rules = m.rules();
        rules[0].action = RuleAction::Tile;
        m.set_rules(&rules).unwrap();
        let out = m.doc.to_string();
        assert!(
            out.contains("# keep me")
                && out.contains("class = \"#32770\"")
                && out.contains("action = \"tile\""),
            "{out}"
        );
    }

    #[test]
    fn removing_every_rule_writes_an_empty_list() {
        let mut m = model();
        assert_eq!(m.rules().len(), 1);
        m.set_rules(&[]).unwrap();
        let out = m.doc.to_string();
        assert!(out.contains("rules = []"), "{out}");
        assert_eq!(
            crate::parse(&out).unwrap()["rules"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "the daemon sees no rules, not the defaults"
        );
        let again = Model::from_text(Path::new("planor.toml"), &out).unwrap();
        assert!(again.rules().is_empty());
    }

    #[test]
    fn a_file_without_rules_shows_the_default_rules_in_effect() {
        let mut m = Model::from_text(Path::new("planor.toml"), "[layout]\ngap_px = 6\n").unwrap();
        assert_eq!(
            m.rules(),
            vec![Rule {
                matcher: RuleMatch::Process,
                value: "Taskmgr.exe".into(),
                action: RuleAction::Ignore
            }]
        );
        let mut rules = m.rules();
        rules.push(Rule {
            matcher: RuleMatch::Class,
            value: "Foo".into(),
            action: RuleAction::Float,
        });
        m.set_rules(&rules).unwrap();
        assert_eq!(
            crate::parse(&m.doc.to_string()).unwrap()["rules"]
                .as_array()
                .unwrap()
                .len(),
            2,
            "the default rule is kept, not silently dropped"
        );
    }
    #[test]
    fn load_error_reports_invalid_toml() {
        let bad = temp("bad", "[layout\ngap_px = ");
        assert!(Model::load_error(&bad).is_some());
        assert!(Model::load(&bad).is_err());
        let good = temp("good", "[layout]\ngap_px = 7\n");
        assert!(Model::load_error(&good).is_none());
        assert!(
            Model::load_error(Path::new("Z:/definitely/missing.toml")).is_none(),
            "missing is not an error"
        );
    }

    #[test]
    fn reload_from_disk_sees_external_edit() {
        let p = temp("reload", "[layout]\ngap_px = 7\n");
        let mut m = Model::load(&p).unwrap();
        let gap = idx(&m, "layout", "gap_px");
        assert_eq!(m.value(gap), toml::Value::Integer(7));
        std::fs::write(&p, "[layout]\ngap_px = 9\n").unwrap();
        m.reload_from_disk().unwrap();
        assert_eq!(m.value(gap), toml::Value::Integer(9));
        assert!(!m.dirty);
    }
}

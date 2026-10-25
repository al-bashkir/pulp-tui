//! Modal forms: typed fields, keyboard editing, validation.

use crate::pulp::RepoType;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a picker gets its options (loaded by the app, never by the form).
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// `None` = RPM, File and Deb repositories that can be Content View sources (no CV snapshots).
    Repos(Option<RepoType>),
    Remotes(RepoType),
    Distributions(RepoType),
    ContentGuards,
    SigningServices,
    Roles,
    Users,
    Domains,
    RoleObjects,
    /// Normal (not composite) Content Views of the active server; the value is the view id.
    ContentViews,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    /// Never displayed; never pre-filled from the server.
    Secret,
    Bool,
    Choice(Vec<String>),
    Number,
    /// Pick from server objects. Multi: `value` holds the selected values joined with `\n`.
    Pick {
        source: Source,
        multi: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PickItem {
    pub label: String,
    pub value: String,
    /// Short column shown before the label (`rpm`/`file`); may be empty.
    pub note: String,
}

/// The open picker popup of one field.
#[derive(Debug, Clone)]
pub struct Picker {
    pub field: usize,
    pub filter: String,
    pub selected: usize,
    /// `PickItem::value`s (multi only).
    pub marked: BTreeSet<String>,
    /// Loading the options failed: the filter text becomes the value.
    pub error: Option<String>,
    /// Filter mode: letters type into the filter. Move mode: letters are keys (`j k g G /`).
    pub filtering: bool,
    /// First visible row; kept by the view between frames (scrolloff).
    pub top: Cell<usize>,
}

/// New highlight (0..len) for a movement key in a list of `len` rows; `None` when `k` is not one.
/// Letters (`j k g G`) move only outside filter mode, where they would be typed. Page steps are 10 rows.
pub fn move_key(k: KeyEvent, filtering: bool, selected: usize, len: usize) -> Option<usize> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let to = match (k.code, ctrl) {
        (KeyCode::Up, _) | (KeyCode::Char('k' | 'p'), true) => selected.saturating_sub(1),
        (KeyCode::Down, _) | (KeyCode::Char('j' | 'n'), true) => selected + 1,
        (KeyCode::PageUp, _) | (KeyCode::Char('u'), true) => selected.saturating_sub(10),
        (KeyCode::PageDown, _) | (KeyCode::Char('d'), true) => selected + 10,
        (KeyCode::Char('k'), false) if !filtering => selected.saturating_sub(1),
        (KeyCode::Char('j'), false) if !filtering => selected + 1,
        (KeyCode::Char('g'), false) if !filtering => 0,
        (KeyCode::Char('G'), false) if !filtering => len,
        _ => return None,
    };
    Some(to.min(len.saturating_sub(1)))
}

/// Readline keys on one line; `cur` is a char index. Returns false when `k` is not an edit key.
/// ← → C-b C-f: char, Alt-b Alt-f C-← C-→: word, Home End C-a C-e: line, C-w C-u C-k: delete word back / to start / to end.
pub fn edit_line(s: &mut String, cur: &mut usize, k: KeyEvent) -> bool {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let c = (*cur).min(len);
    let byte = |i: usize| s.char_indices().nth(i).map_or(s.len(), |(b, _)| b);
    // Start of the word before / end of the word after the cursor.
    let back = || {
        let mut i = c;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    };
    let fwd = || {
        let mut i = c;
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    };
    *cur = match k.code {
        KeyCode::Left if ctrl => back(),
        KeyCode::Right if ctrl => fwd(),
        KeyCode::Left => c.saturating_sub(1),
        KeyCode::Right => (c + 1).min(len),
        KeyCode::Char('b') if alt => back(),
        KeyCode::Char('f') if alt => fwd(),
        KeyCode::Char('b') if ctrl => c.saturating_sub(1),
        KeyCode::Char('f') if ctrl => (c + 1).min(len),
        KeyCode::Home | KeyCode::Char('a') if k.code == KeyCode::Home || ctrl => 0,
        KeyCode::End | KeyCode::Char('e') if k.code == KeyCode::End || ctrl => len,
        KeyCode::Backspace if c > 0 => {
            s.replace_range(byte(c - 1)..byte(c), "");
            c - 1
        }
        KeyCode::Backspace => c,
        KeyCode::Delete => {
            if c < len {
                s.replace_range(byte(c)..byte(c + 1), "");
            }
            c
        }
        KeyCode::Char('w') if ctrl => {
            let b = back();
            s.replace_range(byte(b)..byte(c), "");
            b
        }
        KeyCode::Char('u') if ctrl => {
            s.replace_range(..byte(c), "");
            0
        }
        KeyCode::Char('k') if ctrl => {
            s.truncate(byte(c));
            c
        }
        KeyCode::Char(ch) if !ctrl && !alt => {
            s.insert(byte(c), ch);
            c + 1
        }
        _ => return false,
    };
    true
}

#[derive(Debug, Clone)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    pub value: String,
    pub initial: String,
    pub required: bool,
    pub help: String,
    /// Pick options; `None` = not loaded yet.
    pub options: Option<Vec<PickItem>>,
    /// Pick: label of the current value (empty = show `value`).
    pub shown: String,
    /// Pick: values never listed.
    pub exclude: Vec<String>,
}

impl Field {
    pub fn new(key: &str, label: &str, kind: FieldKind) -> Self {
        let value = match &kind {
            FieldKind::Bool => "false".into(),
            FieldKind::Choice(c) => c.first().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        Self {
            key: key.into(),
            label: label.into(),
            kind,
            initial: value.clone(),
            value,
            required: false,
            help: String::new(),
            options: None,
            shown: String::new(),
            exclude: vec![],
        }
    }
    pub fn text(key: &str, label: &str) -> Self {
        Self::new(key, label, FieldKind::Text)
    }
    pub fn pick(key: &str, label: &str, source: Source) -> Self {
        Self::new(key, label, FieldKind::Pick { source, multi: false })
    }
    pub fn multi(mut self) -> Self {
        if let FieldKind::Pick { multi, .. } = &mut self.kind {
            *multi = true;
        }
        self
    }
    pub fn exclude(mut self, values: Vec<String>) -> Self {
        self.exclude = values;
        self
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn help(mut self, h: &str) -> Self {
        self.help = h.into();
        self
    }
    /// Set the current value and remember it as the initial one (edit forms).
    pub fn value(mut self, v: impl Into<String>) -> Self {
        self.value = v.into();
        self.initial = self.value.clone();
        self
    }
    pub fn changed(&self) -> bool {
        self.value != self.initial
    }
    pub fn display(&self) -> String {
        match self.kind {
            FieldKind::Secret => "•".repeat(self.value.chars().count().min(12)),
            FieldKind::Bool => {
                if self.value == "true" {
                    "[x]".into()
                } else {
                    "[ ]".into()
                }
            }
            FieldKind::Choice(_) => format!("< {} >", self.value),
            FieldKind::Pick { .. } => format!("[ {} ▾ ]", if self.shown.is_empty() { &self.value } else { &self.shown }),
            _ => self.value.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Form {
    /// Unique per form, so a late option load can tell whether its form is still open.
    pub id: u64,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// Lines shown above the fields (context / warnings).
    pub notes: Vec<String>,
    pub picker: Option<Picker>,
    /// Edit mode: keys type into the focused Text, Secret or Number field. Else move mode.
    pub editing: bool,
    /// Char index of the edit cursor in the focused field.
    pub cursor: usize,
}

pub enum FormEvent {
    None,
    Submit,
    Cancel,
    /// The options of this field must be loaded; answer with `Form::set_options`.
    Load(usize),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Options of `f` whose label or note contains `filter` (case-insensitive), minus the excluded ones.
fn matching<'a>(f: &'a Field, filter: &str) -> Vec<&'a PickItem> {
    let q = filter.to_lowercase();
    let hit = |s: &str| s.to_lowercase().contains(&q);
    f.options.iter().flatten().filter(|i| !f.exclude.contains(&i.value) && (hit(&i.label) || hit(&i.note))).collect()
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<Field>) -> Self {
        // A lone text field starts in edit mode: nothing else could take the keys, and an early `i` would be lost.
        let editing = matches!(fields.as_slice(), [f] if matches!(f.kind, FieldKind::Text | FieldKind::Secret | FieldKind::Number));
        let cursor = fields.first().map_or(0, |f| f.value.chars().count());
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            title: title.into(),
            fields,
            focus: 0,
            error: None,
            notes: vec![],
            picker: None,
            editing,
            cursor,
        }
    }
    pub fn note(mut self, n: impl Into<String>) -> Self {
        self.notes.push(n.into());
        self
    }

    pub fn get(&self, key: &str) -> &str {
        self.fields.iter().find(|f| f.key == key).map(|f| f.value.trim()).unwrap_or("")
    }
    pub fn opt(&self, key: &str) -> Option<&str> {
        Some(self.get(key)).filter(|v| !v.is_empty())
    }
    pub fn bool(&self, key: &str) -> bool {
        self.get(key) == "true"
    }
    pub fn changed(&self, key: &str) -> bool {
        self.fields.iter().any(|f| f.key == key && f.changed())
    }
    pub fn number(&self, key: &str) -> Option<i64> {
        self.opt(key).and_then(|v| v.parse().ok())
    }

    pub fn validate(&self) -> Result<(), String> {
        for f in &self.fields {
            let v = f.value.trim();
            if f.required && v.is_empty() {
                return Err(format!("{} is required", f.label));
            }
            if f.kind == FieldKind::Number && !v.is_empty() && v.parse::<i64>().is_err() {
                return Err(format!("{} must be a whole number", f.label));
            }
        }
        Ok(())
    }

    /// Store loaded options (an optional single pick gets a `(none)` entry first), or show the error in the open picker.
    /// After an error `options` stays `None`, so the next open loads again.
    pub fn set_options(&mut self, field: usize, result: Result<Vec<PickItem>, String>) {
        let Some(f) = self.fields.get_mut(field) else { return };
        match result {
            Ok(mut items) => {
                if !f.required && matches!(f.kind, FieldKind::Pick { multi: false, .. }) {
                    items.insert(0, PickItem { label: "(none)".into(), value: String::new(), note: String::new() });
                }
                if f.shown.is_empty() {
                    // A pre-filled value shows its label as soon as the options are known.
                    let values: Vec<&str> = f.value.split('\n').filter(|v| !v.is_empty()).collect();
                    f.shown = match values.as_slice() {
                        [v] => items.iter().find(|i| i.value == *v).map(|i| i.label.clone()).unwrap_or_default(),
                        [_, _, ..] if matches!(f.kind, FieldKind::Pick { multi: true, .. }) => format!("{} selected", values.len()),
                        _ => String::new(),
                    };
                }
                f.options = Some(items);
                if self.picker.as_ref().is_some_and(|p| p.field == field) {
                    self.preselect();
                }
            }
            Err(e) => {
                if let Some(p) = self.picker.as_mut().filter(|p| p.field == field) {
                    p.error = Some(e);
                    p.filtering = true; // the only useful action is typing the value
                }
            }
        }
    }

    /// Put the highlight of an unfiltered popup on the current value (multi: the first marked row).
    fn preselect(&mut self) {
        let Some(p) = self.picker.as_mut().filter(|p| p.filter.is_empty()) else { return };
        let f = &self.fields[p.field];
        let multi = matches!(f.kind, FieldKind::Pick { multi: true, .. });
        let on = |i: &&PickItem| if multi { p.marked.contains(&i.value) } else { i.value == f.value };
        p.selected = matching(f, "").iter().position(on).unwrap_or(0);
    }

    /// Options of field `key` whose value is selected.
    pub fn picked(&self, key: &str) -> Vec<PickItem> {
        let Some(f) = self.fields.iter().find(|f| f.key == key) else { return vec![] };
        let values: Vec<&str> = f.value.split('\n').filter(|v| !v.is_empty()).collect();
        f.options.iter().flatten().filter(|i| values.contains(&i.value.as_str())).cloned().collect()
    }

    /// Options of the open picker that match its filter.
    pub fn visible(&self) -> Vec<&PickItem> {
        self.picker.as_ref().map(|p| matching(&self.fields[p.field], &p.filter)).unwrap_or_default()
    }

    fn open_picker(&mut self, field: usize, filter: String, filtering: bool) -> FormEvent {
        let f = &self.fields[field];
        let marked = if matches!(f.kind, FieldKind::Pick { multi: true, .. }) {
            f.value.split('\n').filter(|v| !v.is_empty()).map(String::from).collect()
        } else {
            BTreeSet::new()
        };
        let load = f.options.is_none();
        self.picker = Some(Picker { field, filter, selected: 0, marked, error: None, filtering, top: Cell::new(0) });
        self.preselect();
        if load { FormEvent::Load(field) } else { FormEvent::None }
    }

    /// Enter while editing: leave edit mode, then next field, or submit on the last one.
    fn advance(&mut self) -> FormEvent {
        self.editing = false;
        if self.focus + 1 == self.fields.len() {
            return FormEvent::Submit;
        }
        self.focus += 1;
        FormEvent::None
    }

    /// Keys of a focused pick field whose popup is closed; `None` = not a pick key (`j` and `k` move).
    fn pick_field_key(&mut self, k: KeyEvent) -> Option<FormEvent> {
        let i = self.focus;
        if !matches!(self.fields[i].kind, FieldKind::Pick { .. }) {
            return None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter | KeyCode::Char(' ') if !ctrl => Some(self.open_picker(i, String::new(), false)),
            KeyCode::Backspace | KeyCode::Char('u') if ctrl || k.code == KeyCode::Backspace => {
                self.fields[i].value.clear();
                self.fields[i].shown.clear();
                Some(FormEvent::None)
            }
            KeyCode::Char('j' | 'k') if !ctrl => None,
            KeyCode::Char('/') if !ctrl => Some(self.open_picker(i, String::new(), true)),
            KeyCode::Char(c) if !ctrl => Some(self.open_picker(i, c.to_string(), true)),
            _ => None,
        }
    }

    fn picker_key(&mut self, k: KeyEvent) -> FormEvent {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Char('s') && ctrl {
            // Take the selection like Enter, then submit; a popup that could not commit stays open.
            self.commit();
            return if self.picker.is_none() { FormEvent::Submit } else { FormEvent::None };
        }
        if k.code == KeyCode::Enter {
            // A taken selection counts as Enter on the field: go on. Only Ctrl-S submits here.
            if self.commit() && self.focus + 1 < self.fields.len() {
                self.focus += 1;
            }
            return FormEvent::None;
        }
        let Some(p) = self.picker.as_mut() else { return FormEvent::None };
        let f = &self.fields[p.field];
        let vis = matching(f, &p.filter);
        let (len, current) = (vis.len(), vis.get(p.selected).map(|i| i.value.clone()));
        let multi = matches!(f.kind, FieldKind::Pick { multi: true, .. });
        if let Some(to) = move_key(k, p.filtering, p.selected, len) {
            p.selected = to;
            return FormEvent::None;
        }
        match k.code {
            KeyCode::Esc if p.filtering => p.filtering = false,
            KeyCode::Esc => self.picker = None,
            KeyCode::Backspace => {
                p.filter.pop();
                p.selected = 0;
            }
            KeyCode::Char(' ') if multi && !ctrl => {
                if let Some(v) = current
                    && !p.marked.remove(&v)
                {
                    p.marked.insert(v);
                }
            }
            KeyCode::Char('/') if !p.filtering && !ctrl => p.filtering = true,
            KeyCode::Char(c) if p.filtering && !ctrl => {
                p.filter.push(c);
                p.selected = 0;
            }
            _ => {}
        }
        FormEvent::None
    }

    /// Enter in the popup: take the selection into the field and close it. Keeps the popup when nothing can be taken.
    /// Returns whether a value was taken (a load error with no typed text only closes the popup).
    fn commit(&mut self) -> bool {
        let Some(p) = &self.picker else { return false };
        if p.error.is_some() && p.filter.is_empty() {
            self.picker = None;
            return false;
        }
        let f = &self.fields[p.field];
        let highlighted = matching(f, &p.filter).get(p.selected).copied();
        let (value, shown) = if p.error.is_some() {
            (p.filter.clone(), p.filter.clone())
        } else if matches!(f.kind, FieldKind::Pick { multi: true, .. }) {
            let values: Vec<String> = match (p.marked.is_empty(), highlighted) {
                (false, _) => p.marked.iter().cloned().collect(),
                (true, Some(i)) => vec![i.value.clone()],
                (true, None) => return false,
            };
            let shown = match values.as_slice() {
                [v] => f.options.iter().flatten().find(|i| i.value == *v).map_or(v.clone(), |i| i.label.clone()),
                _ => format!("{} selected", values.len()),
            };
            (values.join("\n"), shown)
        } else {
            let Some(i) = highlighted else { return false };
            (i.value.clone(), if i.value.is_empty() { String::new() } else { i.label.clone() })
        };
        let field = p.field;
        let roles = matches!(f.kind, FieldKind::Pick { source: Source::Roles, .. });
        let role_changed = roles && f.value != value;
        self.picker = None;
        self.fields[field].value = value;
        self.fields[field].shown = shown;
        if roles {
            for g in self.fields.iter_mut().filter(|g| matches!(g.kind, FieldKind::Pick { source: Source::RoleObjects, .. })) {
                g.options = None;
                if role_changed {
                    // An object of the old role's type must not be sent with the new role.
                    g.value.clear();
                    g.shown.clear();
                }
            }
        }
        true
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        let n = self.fields.len();
        if n == 0 {
            return match k.code {
                KeyCode::Esc => FormEvent::Cancel,
                _ => FormEvent::Submit,
            };
        }
        if self.picker.is_some() {
            return self.picker_key(k);
        }
        if let Some(ev) = self.pick_field_key(k) {
            return ev;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Move mode: j k h l are the arrow keys.
        let code = match k.code {
            KeyCode::Char('j') if !ctrl && !self.editing => KeyCode::Down,
            KeyCode::Char('k') if !ctrl && !self.editing => KeyCode::Up,
            KeyCode::Char('h') if !ctrl && !self.editing => KeyCode::Left,
            KeyCode::Char('l') if !ctrl && !self.editing => KeyCode::Right,
            c => c,
        };
        let (next, prev) = ((self.focus + 1) % n, (self.focus + n - 1) % n);
        let f = &mut self.fields[self.focus];
        let text = matches!(f.kind, FieldKind::Text | FieldKind::Secret | FieldKind::Number);
        match (self.editing, code) {
            (_, KeyCode::Char('s')) if ctrl => return FormEvent::Submit,
            (_, KeyCode::Tab | KeyCode::Down) => (self.focus, self.editing) = (next, false),
            (_, KeyCode::BackTab | KeyCode::Up) => (self.focus, self.editing) = (prev, false),
            (false, KeyCode::Esc) => return FormEvent::Cancel,
            (true, KeyCode::Esc) => self.editing = false,
            (true, KeyCode::Enter) => return self.advance(),
            // A number takes digits, and `-` only in front.
            (true, KeyCode::Char(c))
                if !ctrl
                    && f.kind == FieldKind::Number
                    && !c.is_ascii_digit()
                    && !(c == '-' && self.cursor == 0 && !f.value.starts_with('-')) => {}
            (true, _) => {
                edit_line(&mut f.value, &mut self.cursor, k);
            }
            (false, KeyCode::Enter | KeyCode::Char('i')) if text && !ctrl => {
                self.editing = true;
                self.cursor = f.value.chars().count();
            }
            (false, KeyCode::Enter) => self.focus = next,
            (false, KeyCode::Char(' ')) if f.kind == FieldKind::Bool => {
                f.value = if f.value == "true" { "false".into() } else { "true".into() }
            }
            (false, KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')) if matches!(f.kind, FieldKind::Choice(_)) => {
                if let FieldKind::Choice(c) = &f.kind
                    && !c.is_empty()
                {
                    let i = c.iter().position(|x| *x == f.value).unwrap_or(0);
                    let j = if code == KeyCode::Left { (i + c.len() - 1) % c.len() } else { (i + 1) % c.len() };
                    f.value = c[j].clone();
                }
            }
            _ => {}
        }
        FormEvent::None
    }
}

/// `k=v,k2=v2` <-> JSON object (Pulp labels).
pub fn parse_labels(s: &str) -> Result<serde_json::Value, String> {
    let mut m = serde_json::Map::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').ok_or_else(|| format!("label {part:?} must be key=value"))?;
        if k.trim().is_empty() {
            return Err("empty label key".into());
        }
        m.insert(k.trim().into(), v.trim().into());
    }
    Ok(m.into())
}

pub fn format_labels(v: &serde_json::Value) -> String {
    v.as_object()
        .map(|m| m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join(","))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    /// A character key like crossterm delivers it: uppercase letters come with SHIFT.
    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), if c.is_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE })
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn move_mode_j_k_change_field_and_i_then_j_types_j() {
        let mut f = Form::new("t", vec![Field::text("a", "A"), Field::text("b", "B")]);
        f.handle_key(ch('j'));
        assert_eq!(f.focus, 1);
        f.handle_key(ch('k'));
        f.handle_key(ch('x')); // move mode: letters and Backspace do not edit
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!((f.focus, f.get("a")), (0, ""));
        f.handle_key(ch('i'));
        assert!(f.editing);
        "jk".chars().for_each(|c| {
            f.handle_key(ch(c));
        });
        assert_eq!((f.get("a"), f.focus), ("jk", 0));
        assert!(matches!(f.handle_key(key(KeyCode::Esc)), FormEvent::None));
        assert_eq!((f.editing, f.get("a")), (false, "jk"));
        assert!(matches!(f.handle_key(key(KeyCode::Esc)), FormEvent::Cancel));
    }

    #[test]
    fn enter_edits_then_moves_on_and_submits_only_in_edit_mode() {
        let mut f = Form::new("t", vec![Field::text("a", "A"), Field::text("b", "B")]);
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.editing);
        f.handle_key(ch('1'));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert_eq!((f.focus, f.editing, f.get("a")), (1, false, "1"));
        f.handle_key(key(KeyCode::Enter));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Submit));
        assert!(!f.editing);
        // Last field is a bool: Enter in move mode moves on and never submits.
        let mut f = Form::new("t", vec![Field::text("a", "A"), Field::new("b", "B", FieldKind::Bool)]);
        f.focus = 1;
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert_eq!(f.focus, 0);
    }

    #[test]
    fn choices_change_with_h_l_space_and_arrows() {
        let mut f = Form::new("t", vec![Field::new("c", "C", FieldKind::Choice(vec!["a".into(), "b".into(), "c".into()]))]);
        let keys = [ch('l'), ch('l'), ch('h'), ch(' '), key(KeyCode::Right), key(KeyCode::Left)];
        let seen: Vec<String> = keys
            .into_iter()
            .map(|k| {
                f.handle_key(k);
                f.get("c").to_string()
            })
            .collect();
        assert_eq!(seen, ["b", "c", "b", "c", "a", "c"]);
    }

    #[test]
    fn ctrl_s_submits_and_arrows_and_tab_leave_edit_mode() {
        let mut f = Form::new("t", vec![Field::text("a", "A"), Field::text("b", "B"), Field::text("c", "C")]);
        assert!(matches!(f.handle_key(ctrl('s')), FormEvent::Submit));
        for (k, focus) in [(KeyCode::Down, 1), (KeyCode::Up, 0), (KeyCode::Tab, 1), (KeyCode::BackTab, 0)] {
            f.handle_key(ch('i'));
            f.handle_key(key(k));
            assert_eq!((f.focus, f.editing), (focus, false));
        }
        f.handle_key(ch('i'));
        assert!(matches!(f.handle_key(ctrl('s')), FormEvent::Submit));
    }

    #[test]
    fn ctrl_s_in_an_open_picker_takes_the_selection_and_submits() {
        let mut f = picker_form(true, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        f.handle_key(ch(' ')); // marks epel9
        assert!(matches!(f.handle_key(ctrl('s')), FormEvent::Submit));
        assert!(f.picker.is_none());
        assert_eq!(f.get("repo"), "/r/epel9/");
        // Nothing can be taken: the popup stays open and the form does not submit.
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(ch('/'));
        f.set_options(0, Ok(items()));
        f.handle_key(ch('z'));
        assert!(matches!(f.handle_key(ctrl('s')), FormEvent::None));
        assert!(f.picker.is_some());
    }

    #[test]
    fn a_single_text_field_form_opens_in_edit_mode() {
        let mut f = Form::new("t", vec![Field::new("pw", "Password", FieldKind::Secret)]);
        assert!(f.editing);
        "admin123".chars().for_each(|c| {
            f.handle_key(ch(c));
        });
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Submit));
        assert_eq!(f.get("pw"), "admin123");
        // Other single fields, and forms with more fields, still open in move mode.
        assert!(!Form::new("t", vec![Field::new("b", "B", FieldKind::Bool)]).editing);
        assert!(!Form::new("t", vec![Field::text("a", "A"), Field::text("b", "B")]).editing);
    }

    #[test]
    fn j_k_on_pick_field_move_instead_of_opening() {
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(ch('j')), FormEvent::None));
        assert_eq!((f.focus, f.picker.is_none()), (1, true));
        f.handle_key(ch('k'));
        assert_eq!(f.focus, 0);
    }

    #[test]
    fn editing_and_validation() {
        let mut f = Form::new(
            "t",
            vec![
                Field::text("name", "Name").required(),
                Field::new("n", "Count", FieldKind::Number),
                Field::new("b", "Flag", FieldKind::Bool),
                Field::new("c", "Choice", FieldKind::Choice(vec!["a".into(), "b".into()])),
            ],
        );
        assert!(f.validate().is_err());
        f.handle_key(ch('i'));
        for c in "repo".chars() {
            f.handle_key(ch(c));
        }
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!(f.get("name"), "rep");
        f.handle_key(key(KeyCode::Tab));
        f.handle_key(ch('i'));
        f.handle_key(ch('x')); // ignored in number field
        f.handle_key(ch('7'));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(ch(' '));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Right));
        assert_eq!((f.number("n"), f.bool("b"), f.get("c")), (Some(7), true, "b"));
        assert!(f.changed("c") && !f.changed("missing"));
        assert!(f.validate().is_ok());
        // The last field is a choice: Enter moves on, only Ctrl-S submits.
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(matches!(f.handle_key(ctrl('s')), FormEvent::Submit));
        assert!(matches!(f.handle_key(key(KeyCode::Esc)), FormEvent::Cancel));
    }

    #[test]
    fn labels_roundtrip() {
        let v = parse_labels("env=prod, team = web").unwrap();
        assert_eq!(v["team"], "web");
        assert_eq!(parse_labels(&format_labels(&v)).unwrap(), v);
        assert!(parse_labels("novalue").is_err());
    }

    fn picker_form(multi: bool, required: bool) -> Form {
        let mut f = Field::pick("repo", "Repository", Source::Repos(None));
        if multi {
            f = f.multi();
        }
        if required {
            f = f.required();
        }
        Form::new("t", vec![f, Field::text("x", "X")])
    }

    fn items() -> Vec<PickItem> {
        ["epel9", "epel9-next", "base"].map(|n| PickItem { label: n.into(), value: format!("/r/{n}/"), note: "rpm".into() }).to_vec()
    }

    #[test]
    fn opening_unloaded_picker_asks_for_load() {
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Load(0)));
        assert_eq!(f.picker.as_ref().unwrap().field, 0);
        assert!(!f.picker.as_ref().unwrap().filtering);
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(key(KeyCode::Char('e'))), FormEvent::Load(0)));
        assert_eq!(f.picker.as_ref().unwrap().filter, "e");
        assert!(f.picker.as_ref().unwrap().filtering);
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(ch('/')), FormEvent::Load(0)));
        let p = f.picker.as_ref().unwrap();
        assert_eq!((p.filter.as_str(), p.filtering), ("", true));
    }

    #[test]
    fn move_key_maps_vim_ctrl_and_arrow_keys() {
        assert_eq!(move_key(ch('j'), false, 0, 5), Some(1));
        assert_eq!(move_key(ch('k'), false, 2, 5), Some(1));
        assert_eq!(move_key(ch('g'), false, 3, 5), Some(0));
        assert_eq!(move_key(ch('G'), false, 0, 5), Some(4)); // SHIFT, as a terminal sends it
        assert_eq!(move_key(ch('j'), true, 0, 5), None); // filter mode: letters type
        assert_eq!(move_key(ch('G'), true, 0, 5), None);
        for c in ['j', 'n'] {
            assert_eq!(move_key(ctrl(c), true, 0, 5), Some(1));
        }
        for c in ['k', 'p'] {
            assert_eq!(move_key(ctrl(c), false, 2, 5), Some(1));
        }
        assert_eq!(move_key(ctrl('d'), false, 0, 30), Some(10));
        assert_eq!(move_key(ctrl('u'), true, 15, 30), Some(5));
        assert_eq!(move_key(key(KeyCode::PageDown), true, 25, 30), Some(29));
        assert_eq!(move_key(key(KeyCode::PageUp), false, 3, 30), Some(0));
        assert_eq!(move_key(key(KeyCode::Down), true, 4, 5), Some(4));
        assert_eq!(move_key(key(KeyCode::Up), false, 0, 5), Some(0));
        assert_eq!(move_key(key(KeyCode::Down), false, 0, 0), Some(0));
        assert_eq!(move_key(ch('x'), false, 0, 5), None);
    }

    #[test]
    #[allow(non_snake_case)] // the name says the keys: g and G
    fn picker_moves_with_j_k_g_G_and_arrows() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        let keys = [ch('j'), ch('j'), ch('j'), ch('k'), ch('G'), ch('g'), key(KeyCode::Down), key(KeyCode::Up)];
        let seen: Vec<usize> = keys
            .into_iter()
            .map(|k| {
                f.handle_key(k);
                f.picker.as_ref().unwrap().selected
            })
            .collect();
        assert_eq!(seen, [1, 2, 2, 1, 2, 0, 1, 0]);
        assert_eq!(f.picker.as_ref().unwrap().filter, "");
    }

    #[test]
    fn slash_filter_then_esc_keeps_filter_and_second_esc_closes() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        "/next".chars().for_each(|c| {
            f.handle_key(ch(c));
        });
        assert!(f.picker.as_ref().unwrap().filtering);
        f.handle_key(key(KeyCode::Esc));
        let p = f.picker.as_ref().unwrap();
        assert_eq!((p.filtering, p.filter.as_str(), f.visible().len()), (false, "next", 1));
        f.handle_key(ch('j')); // moves, does not type
        assert_eq!(f.picker.as_ref().unwrap().filter, "next");
        f.handle_key(key(KeyCode::Esc));
        assert!(f.picker.is_none());
        assert_eq!(f.get("repo"), "");
    }

    #[test]
    fn filter_mode_moves_with_ctrl_keys_and_types_letters() {
        let mut f = picker_form(false, true);
        f.handle_key(ch('e')); // a letter opens filter mode; all three items contain "e"
        f.set_options(0, Ok(items()));
        let keys = [ctrl('j'), ctrl('n'), ctrl('k'), ctrl('p'), key(KeyCode::Down), key(KeyCode::Up)];
        let seen: Vec<usize> = keys
            .into_iter()
            .map(|k| {
                f.handle_key(k);
                f.picker.as_ref().unwrap().selected
            })
            .collect();
        assert_eq!(seen, [1, 2, 1, 0, 1, 0]);
        f.handle_key(ch('j'));
        assert_eq!(f.picker.as_ref().unwrap().filter, "ej");
    }

    #[test]
    fn filter_and_select_single() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(ch('/'));
        f.set_options(0, Ok(items()));
        for c in "next".chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(f.visible().len(), 1);
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.picker.is_none());
        assert_eq!((f.get("repo"), f.fields[0].shown.as_str()), ("/r/epel9-next/", "epel9-next"));
        assert_eq!(f.picked("repo")[0].label, "epel9-next");
        assert_eq!(f.focus, 1);
        // Second open uses the cached options: no Load event.
        f.handle_key(key(KeyCode::BackTab));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.picker.is_some());
    }

    #[test]
    fn optional_single_gets_none_entry() {
        let mut f = picker_form(false, false);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert_eq!(f.visible()[0].label, "(none)");
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.get("repo"), "");
    }

    #[test]
    fn multi_marks_with_space() {
        let mut f = picker_form(true, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        f.handle_key(key(KeyCode::Char(' ')));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Char(' ')));
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.picked("repo").len(), 2);
        assert_eq!(f.fields[0].shown, "2 selected");
    }

    #[test]
    fn esc_closes_picker_and_keeps_value() {
        let mut f = picker_form(false, true);
        f.fields[0].value = "/r/base/".into();
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        f.handle_key(key(KeyCode::Down));
        assert!(matches!(f.handle_key(key(KeyCode::Esc)), FormEvent::None));
        assert!(f.picker.is_none());
        assert_eq!(f.get("repo"), "/r/base/");
    }

    #[test]
    fn pick_error_takes_typed_text() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Err("403 forbidden".into()));
        assert_eq!(f.picker.as_ref().unwrap().error.as_deref(), Some("403 forbidden"));
        assert!(f.picker.as_ref().unwrap().filtering);
        assert!(f.fields[0].options.is_none());
        for c in "alice".chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.get("repo"), "alice");
    }

    #[test]
    fn enter_with_no_match_keeps_popup() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(ch('/'));
        f.set_options(0, Ok(items()));
        for c in "zzz".chars() {
            f.handle_key(key(KeyCode::Char(c)));
        }
        f.handle_key(key(KeyCode::Enter));
        assert!(f.picker.is_some());
        assert_eq!(f.get("repo"), "");
    }

    #[test]
    fn changing_role_clears_object_options() {
        let mut f =
            Form::new("t", vec![Field::pick("role", "Role", Source::Roles).required(), Field::pick("obj", "Object", Source::RoleObjects)]);
        f.fields[1].options = Some(items());
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(vec![PickItem { label: "r1".into(), value: "r1".into(), note: String::new() }]));
        f.handle_key(key(KeyCode::Enter));
        assert!(f.fields[1].options.is_none());
    }

    #[test]
    fn changing_role_clears_picked_object_but_same_role_keeps_it() {
        let mut f =
            Form::new("t", vec![Field::pick("role", "Role", Source::Roles).required(), Field::pick("obj", "Object", Source::RoleObjects)]);
        let role = |n: &str| PickItem { label: n.into(), value: n.into(), note: String::new() };
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(vec![role("r1"), role("r2")]));
        f.handle_key(key(KeyCode::Enter)); // r1 (was empty)
        f.fields[1].value = "/o/1/".into();
        f.fields[1].shown = "obj1".into();
        f.fields[1].options = Some(items());
        f.focus = 0; // Enter in the popup moved on to the object field
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Enter)); // r2
        assert_eq!(f.get("role"), "r2");
        assert_eq!((f.fields[1].value.as_str(), f.fields[1].shown.as_str()), ("", ""));
        assert!(f.fields[1].options.is_none());
        f.fields[1].value = "/o/2/".into();
        f.fields[1].shown = "obj2".into();
        f.focus = 0;
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Enter)); // r2 again
        assert_eq!((f.fields[1].value.as_str(), f.fields[1].shown.as_str()), ("/o/2/", "obj2"));
    }

    #[test]
    fn excluded_values_are_not_listed() {
        let mut f = Form::new("t", vec![Field::pick("repo", "R", Source::Repos(None)).required().exclude(vec!["/r/base/".into()])]);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert!(f.visible().iter().all(|i| i.label != "base"));
        assert_eq!(f.visible().len(), 2);
    }

    #[test]
    fn form_ids_are_unique() {
        assert_ne!(picker_form(false, false).id, picker_form(false, false).id);
    }

    #[test]
    fn multi_open_premarks_current_values() {
        let mut f = picker_form(true, true);
        f.fields[0].value = "/r/base/\n/r/epel9/".into();
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.picker.as_ref().unwrap().marked.len(), 2);
        let mut f = picker_form(true, true);
        f.handle_key(key(KeyCode::Enter));
        assert!(f.picker.as_ref().unwrap().marked.is_empty());
    }

    #[test]
    fn space_opens_empty_filter_and_filters_in_single_popup() {
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(key(KeyCode::Char(' '))), FormEvent::Load(0)));
        assert_eq!(f.picker.as_ref().unwrap().filter, "");
        f.handle_key(key(KeyCode::Char(' '))); // move mode, single list: ignored
        assert_eq!(f.picker.as_ref().unwrap().filter, "");
        f.handle_key(ch('/'));
        f.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(f.picker.as_ref().unwrap().filter, " ");
    }

    #[test]
    fn backspace_clears_picked_value() {
        let mut f = picker_form(false, true);
        f.fields[0].value = "/r/base/".into();
        f.fields[0].shown = "base".into();
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!((f.fields[0].value.as_str(), f.fields[0].shown.as_str()), ("", ""));
        assert!(f.picker.is_none());
    }

    #[test]
    fn enter_in_popup_moves_on_but_never_submits() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.picker.is_none());
        assert_eq!((f.get("repo"), f.focus), ("/r/epel9/", 1));
        // A popup that cannot commit stays open and does not move on.
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.handle_key(ch('/'));
        f.set_options(0, Ok(items()));
        f.handle_key(key(KeyCode::Char('z')));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert_eq!((f.picker.is_some(), f.focus), (true, 0));
        // Last field: Enter takes the value and stays; only Ctrl-S submits.
        let mut f = Form::new("t", vec![Field::pick("repo", "Repository", Source::Repos(None)).required()]);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert_eq!((f.get("repo"), f.focus, f.picker.is_none()), ("/r/epel9/", 0, true));
    }

    #[test]
    fn prefilled_optional_pick_survives_enter_enter() {
        let g = |n: &str| PickItem { label: n.into(), value: format!("/g/{n}/"), note: String::new() };
        let mut f = Form::new("t", vec![Field::pick("guard", "Guard", Source::ContentGuards).value("/g/1/"), Field::text("x", "X")]);
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Load(0)));
        f.set_options(0, Ok(vec![g("1"), g("2")]));
        assert_eq!(f.visible()[f.picker.as_ref().unwrap().selected].value, "/g/1/");
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.get("guard"), "/g/1/");
        // Cached options: the popup opens on the current value at once.
        f.focus = 0;
        f.handle_key(key(KeyCode::Enter));
        assert_eq!(f.picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn multi_popup_opens_on_first_marked_row() {
        let mut f = picker_form(true, true);
        f.fields[0].value = "/r/base/\n/r/epel9-next/".into();
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert_eq!(f.picker.as_ref().unwrap().selected, 1);
        let mut f = picker_form(false, true);
        f.fields[0].value = "/r/gone/".into();
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert_eq!(f.picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn load_error_enter_on_empty_filter_keeps_value() {
        let mut f = Form::new("t", vec![Field::pick("repo", "Repository", Source::Repos(None)).required().value("/r/base/")]);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Err("boom".into()));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.picker.is_none());
        assert_eq!((f.get("repo"), f.focus), ("/r/base/", 0));
    }

    #[test]
    fn options_fill_shown_of_prefilled_value() {
        let mut f = picker_form(false, true);
        f.fields[0].value = "/r/base/".into();
        f.set_options(0, Ok(items()));
        assert_eq!(f.fields[0].shown, "base");
        let mut f = picker_form(true, true);
        f.fields[0].value = "/r/base/\n/r/epel9/".into();
        f.set_options(0, Ok(items()));
        assert_eq!(f.fields[0].shown, "2 selected");
        let mut f = picker_form(true, true);
        f.fields[0].value = "/r/base/".into();
        f.set_options(0, Ok(items()));
        assert_eq!(f.fields[0].shown, "base");
        // A label that is already shown stays; an unknown value shows itself.
        let mut f = picker_form(false, true);
        f.fields[0].value = "/r/gone/".into();
        f.fields[0].shown = "kept".into();
        f.set_options(0, Ok(items()));
        assert_eq!(f.fields[0].shown, "kept");
        f.fields[0].shown.clear();
        f.set_options(0, Ok(items()));
        assert_eq!(f.fields[0].display(), "[ /r/gone/ ▾ ]");
    }

    #[test]
    fn pick_display_prefers_shown() {
        let mut f = Field::pick("repo", "R", Source::Repos(None));
        f.value = "/r/base/".into();
        assert_eq!(f.display(), "[ /r/base/ ▾ ]");
        f.shown = "base".into();
        assert_eq!(f.display(), "[ base ▾ ]");
    }

    #[test]
    fn edit_line_moves_and_deletes_inside_the_text() {
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        let (mut s, mut c) = ("héllo wörld".to_string(), 11);
        let mut press = |k| assert!(edit_line(&mut s, &mut c, k));
        press(key(KeyCode::Left));
        press(ch('X'));
        press(ctrl('a'));
        press(ch('>'));
        press(alt('f'));
        press(key(KeyCode::Delete));
        assert_eq!((s.as_str(), c), (">héllowörlXd", 6));
        let (mut s, mut c) = ("one two three".to_string(), 13);
        edit_line(&mut s, &mut c, ctrl('w'));
        assert_eq!((s.as_str(), c), ("one two ", 8));
        edit_line(&mut s, &mut c, alt('b'));
        edit_line(&mut s, &mut c, ctrl('k'));
        assert_eq!((s.as_str(), c), ("one ", 4));
        edit_line(&mut s, &mut c, ctrl('b'));
        edit_line(&mut s, &mut c, ctrl('u'));
        assert_eq!((s.as_str(), c), (" ", 0));
        edit_line(&mut s, &mut c, key(KeyCode::Backspace));
        edit_line(&mut s, &mut c, ctrl('e'));
        assert_eq!((s.as_str(), c), (" ", 1));
        assert!(!edit_line(&mut s, &mut c, ctrl('d')), "Ctrl-D is not an edit key");
    }

    #[test]
    fn editing_starts_at_the_end_and_types_at_the_cursor() {
        let mut f = Form::new("t", vec![Field::text("a", "A").value("bc"), Field::new("n", "N", FieldKind::Number)]);
        f.handle_key(ch('i'));
        f.handle_key(key(KeyCode::Home));
        f.handle_key(ch('a'));
        f.handle_key(key(KeyCode::End));
        f.handle_key(ch('d'));
        assert_eq!(f.fields[0].value, "abcd");
        f.handle_key(key(KeyCode::Tab));
        f.handle_key(ch('i'));
        for c in "12-x".chars() {
            f.handle_key(ch(c));
        }
        f.handle_key(key(KeyCode::Home));
        f.handle_key(ch('-'));
        f.handle_key(ch('-'));
        assert_eq!(f.fields[1].value, "-12", "a number takes digits and one leading -");
    }
}

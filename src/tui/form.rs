//! Modal forms: typed fields, keyboard editing, validation.

use crate::pulp::RepoType;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

/// Where a picker gets its options (loaded by the app, never by the form).
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// `None` = RPM and File repositories.
    Repos(Option<RepoType>),
    Remotes(RepoType),
    Distributions(RepoType),
    ContentGuards,
    SigningServices,
    Roles,
    Users,
    Domains,
    RoleObjects,
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
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            title: title.into(),
            fields,
            focus: 0,
            error: None,
            notes: vec![],
            picker: None,
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

    fn open_picker(&mut self, field: usize, filter: String) -> FormEvent {
        let f = &self.fields[field];
        let marked = if matches!(f.kind, FieldKind::Pick { multi: true, .. }) {
            f.value.split('\n').filter(|v| !v.is_empty()).map(String::from).collect()
        } else {
            BTreeSet::new()
        };
        let load = f.options.is_none();
        self.picker = Some(Picker { field, filter, selected: 0, marked, error: None });
        self.preselect();
        if load { FormEvent::Load(field) } else { FormEvent::None }
    }

    /// Enter on a plain field: next field, or submit on the last one.
    fn advance(&mut self) -> FormEvent {
        if self.focus + 1 == self.fields.len() {
            return FormEvent::Submit;
        }
        self.focus += 1;
        FormEvent::None
    }

    /// Keys of a focused pick field whose popup is closed; `None` = not a pick key.
    fn pick_field_key(&mut self, k: KeyEvent) -> Option<FormEvent> {
        let i = self.focus;
        if !matches!(self.fields[i].kind, FieldKind::Pick { .. }) {
            return None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Enter | KeyCode::Char(' ') if !ctrl => Some(self.open_picker(i, String::new())),
            KeyCode::Backspace | KeyCode::Char('u') if ctrl || k.code == KeyCode::Backspace => {
                self.fields[i].value.clear();
                self.fields[i].shown.clear();
                Some(FormEvent::None)
            }
            KeyCode::Char(c) if !ctrl => Some(self.open_picker(i, c.to_string())),
            _ => None,
        }
    }

    fn picker_key(&mut self, k: KeyEvent) -> FormEvent {
        if k.code == KeyCode::Enter {
            // A taken selection counts as Enter on the field: go on (or submit on the last one).
            return if self.commit() { self.advance() } else { FormEvent::None };
        }
        let Some(p) = self.picker.as_mut() else { return FormEvent::None };
        let f = &self.fields[p.field];
        let vis = matching(f, &p.filter);
        let (len, current) = (vis.len(), vis.get(p.selected).map(|i| i.value.clone()));
        let multi = matches!(f.kind, FieldKind::Pick { multi: true, .. });
        let last = len.saturating_sub(1);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => p.selected = p.selected.saturating_sub(1),
            KeyCode::Down => p.selected = (p.selected + 1).min(last),
            KeyCode::PageUp => p.selected = p.selected.saturating_sub(10),
            KeyCode::PageDown => p.selected = (p.selected + 10).min(last),
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
            KeyCode::Char(c) if !ctrl => {
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
        let f = &mut self.fields[self.focus];
        match k.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Char('s') if ctrl => return FormEvent::Submit,
            KeyCode::Enter if self.focus + 1 == n => return FormEvent::Submit,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Down => self.focus = (self.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up => self.focus = (self.focus + n - 1) % n,
            KeyCode::Char('u') if ctrl => f.value.clear(),
            KeyCode::Char(' ') if f.kind == FieldKind::Bool => f.value = if f.value == "true" { "false".into() } else { "true".into() },
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if matches!(f.kind, FieldKind::Choice(_)) => {
                if let FieldKind::Choice(c) = &f.kind
                    && !c.is_empty()
                {
                    let i = c.iter().position(|x| *x == f.value).unwrap_or(0);
                    let j = if k.code == KeyCode::Left { (i + c.len() - 1) % c.len() } else { (i + 1) % c.len() };
                    f.value = c[j].clone();
                }
            }
            KeyCode::Backspace if !matches!(f.kind, FieldKind::Bool | FieldKind::Choice(_)) => {
                f.value.pop();
            }
            KeyCode::Char(c)
                if !ctrl
                    && !matches!(f.kind, FieldKind::Bool | FieldKind::Choice(_))
                    && (f.kind != FieldKind::Number || c.is_ascii_digit() || (c == '-' && f.value.is_empty())) =>
            {
                f.value.push(c)
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
        for ch in "repo".chars() {
            f.handle_key(key(KeyCode::Char(ch)));
        }
        f.handle_key(key(KeyCode::Backspace));
        assert_eq!(f.get("name"), "rep");
        f.handle_key(key(KeyCode::Tab));
        f.handle_key(key(KeyCode::Char('x'))); // ignored in number field
        f.handle_key(key(KeyCode::Char('7')));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Char(' ')));
        f.handle_key(key(KeyCode::Down));
        f.handle_key(key(KeyCode::Right));
        assert_eq!((f.number("n"), f.bool("b"), f.get("c")), (Some(7), true, "b"));
        assert!(f.changed("c") && !f.changed("missing"));
        assert!(f.validate().is_ok());
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Submit));
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
        let mut f = picker_form(false, true);
        assert!(matches!(f.handle_key(key(KeyCode::Char('e'))), FormEvent::Load(0)));
        assert_eq!(f.picker.as_ref().unwrap().filter, "e");
    }

    #[test]
    fn filter_and_select_single() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
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
    fn enter_in_popup_moves_on_and_submits_on_last() {
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert!(f.picker.is_none());
        assert_eq!((f.get("repo"), f.focus), ("/r/epel9/", 1));
        // A popup that cannot commit stays open and does not move on.
        let mut f = picker_form(false, true);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        f.handle_key(key(KeyCode::Char('z')));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::None));
        assert_eq!((f.picker.is_some(), f.focus), (true, 0));
        // Last field: Enter submits.
        let mut f = Form::new("t", vec![Field::pick("repo", "Repository", Source::Repos(None)).required()]);
        f.handle_key(key(KeyCode::Enter));
        f.set_options(0, Ok(items()));
        assert!(matches!(f.handle_key(key(KeyCode::Enter)), FormEvent::Submit));
        assert_eq!(f.get("repo"), "/r/epel9/");
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
}

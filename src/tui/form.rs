//! Modal forms: typed fields, keyboard editing, validation.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    /// Never displayed; never pre-filled from the server.
    Secret,
    Bool,
    Choice(Vec<String>),
    Number,
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
}

impl Field {
    pub fn new(key: &str, label: &str, kind: FieldKind) -> Self {
        let value = match &kind {
            FieldKind::Bool => "false".into(),
            FieldKind::Choice(c) => c.first().cloned().unwrap_or_default(),
            _ => String::new(),
        };
        Self { key: key.into(), label: label.into(), kind, initial: value.clone(), value, required: false, help: String::new() }
    }
    pub fn text(key: &str, label: &str) -> Self {
        Self::new(key, label, FieldKind::Text)
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
            _ => self.value.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: Option<String>,
    /// Lines shown above the fields (context / warnings).
    pub notes: Vec<String>,
}

pub enum FormEvent {
    None,
    Submit,
    Cancel,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<Field>) -> Self {
        Self { title: title.into(), fields, focus: 0, error: None, notes: vec![] }
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

    pub fn handle_key(&mut self, k: KeyEvent) -> FormEvent {
        let n = self.fields.len();
        if n == 0 {
            return match k.code {
                KeyCode::Esc => FormEvent::Cancel,
                _ => FormEvent::Submit,
            };
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
}

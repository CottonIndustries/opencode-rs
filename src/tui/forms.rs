//! Session form prompt: one editable row per field, submitted as a single answer.

use super::dialog;
use super::draw::{self, line, span};
use super::prompt::Editor;
use super::theme::Theme;
use crate::forms::Form;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default)]
struct When {
    key: String,
    op: String,
    value: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Field {
    pub key: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub url: String,
    pub required: bool,
    pub hidden: bool,
    pub custom: bool,
    pub options: Vec<(String, String)>,
    pub default: Value,
    when: Vec<When>,
}

impl Field {
    fn editable(&self) -> bool {
        !self.hidden && self.kind != "external"
    }

    fn option_value(&self, text: &str) -> Option<String> {
        self.options
            .iter()
            .find(|(value, label)| value == text || label == text)
            .map(|(value, _)| value.clone())
    }

    fn validate_option(&self, value: &str) -> Result<(), String> {
        if self.options.is_empty() || self.custom {
            return Ok(());
        }
        if self.options.iter().any(|(option, _)| option == value) {
            return Ok(());
        }
        Err(format!("{} must be one of: {}", self.title, self.options.iter().map(|(value, _)| value.clone()).collect::<Vec<_>>().join(", ")))
    }
}

fn parse_fields(value: &Value) -> Vec<Field> {
    match value {
        Value::Array(items) => items.iter().filter_map(parse_field).collect(),
        Value::Object(map) if map.contains_key("key") || map.contains_key("type") => parse_field(value).into_iter().collect(),
        Value::Object(map) => map.iter().filter_map(|(key, value)| object_field(key, value)).collect(),
        _ => Vec::new(),
    }
}

fn object_field(key: &str, value: &Value) -> Option<Field> {
    let object = value.as_object()?;
    Some(build_field(key, object))
}

fn parse_field(value: &Value) -> Option<Field> {
    let object = value.as_object()?;
    let key = object.get("key").and_then(Value::as_str)?;
    Some(build_field(key, object))
}

fn build_field(key: &str, object: &Map<String, Value>) -> Field {
    let string = |name: &str| object.get(name).and_then(Value::as_str).unwrap_or_default().to_string();
    let flag = |name: &str| object.get(name).and_then(Value::as_bool).unwrap_or(false);
    let options = object
        .get("options")
        .and_then(Value::as_array)
        .map(|options| {
            options
                .iter()
                .filter_map(|option| {
                    if let Some(value) = option.as_str() {
                        return Some((value.to_string(), value.to_string()));
                    }
                    let value = option.get("value").and_then(Value::as_str)?;
                    let label = option.get("label").and_then(Value::as_str).unwrap_or(value);
                    Some((value.to_string(), label.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let when = object
        .get("when")
        .and_then(Value::as_array)
        .map(|conditions| {
            conditions
                .iter()
                .filter_map(|condition| {
                    Some(When {
                        key: condition.get("key").and_then(Value::as_str)?.to_string(),
                        op: condition.get("op").and_then(Value::as_str).unwrap_or("eq").to_string(),
                        value: condition.get("value").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let title = string("title");
    let kind = string("type");
    Field {
        key: key.to_string(),
        title: if title.is_empty() { key.to_string() } else { title },
        description: string("description"),
        kind: if kind.is_empty() { "string".into() } else { kind },
        url: string("url"),
        required: flag("required"),
        hidden: flag("hidden"),
        custom: flag("custom"),
        default: object.get("default").cloned().unwrap_or(Value::Null),
        options,
        when,
    }
}

#[derive(Default)]
pub struct FormUi {
    pub id: String,
    fields: Vec<Field>,
    editors: Vec<Editor>,
    toggles: Vec<bool>,
    active: usize,
}

impl FormUi {
    /// Reset the editor when the visible form changes.
    pub fn sync(&mut self, form: &Form) {
        if self.id == form.id {
            return;
        }
        self.id = form.id.clone();
        self.fields = parse_fields(&form.fields);
        self.editors = Vec::with_capacity(self.fields.len());
        self.toggles = Vec::with_capacity(self.fields.len());
        for field in &self.fields {
            let mut editor = Editor::default();
            let mut toggle = false;
            match field.kind.as_str() {
                "boolean" => toggle = field.default.as_bool().unwrap_or(false),
                "multiselect" => {
                    let text = field.default.as_array().map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                    editor.set(&text);
                }
                "number" | "integer" => {
                    if let Some(number) = field.default.as_f64() {
                        editor.set(&number.to_string());
                    }
                }
                _ => editor.set(field.default.as_str().unwrap_or_default()),
            }
            self.editors.push(editor);
            self.toggles.push(toggle);
        }
        self.active = self.fields.iter().position(Field::editable).unwrap_or(0);
    }

    pub fn next(&mut self, delta: isize) {
        let editable: Vec<usize> = self.fields.iter().enumerate().filter(|(_, field)| field.editable()).map(|(index, _)| index).collect();
        if editable.is_empty() {
            return;
        }
        let at = editable.iter().position(|index| *index == self.active).unwrap_or(0);
        let next = (at as isize + delta).rem_euclid(editable.len() as isize) as usize;
        self.active = editable[next];
    }

    /// Toggle a focused boolean field; false when the row is not a boolean.
    pub fn toggle(&mut self) -> bool {
        if self.fields.get(self.active).is_some_and(|field| field.kind == "boolean" && field.editable()) {
            if let Some(value) = self.toggles.get_mut(self.active) {
                *value = !*value;
                return true;
            }
        }
        false
    }

    pub fn editor(&mut self) -> Option<&mut Editor> {
        let field = self.fields.get(self.active)?;
        if !field.editable() || field.kind == "boolean" {
            return None;
        }
        self.editors.get_mut(self.active)
    }

    /// Collect the current inputs into a form answer.
    pub fn answer(&self) -> Result<Value, String> {
        let mut answer = Map::new();
        for (index, field) in self.fields.iter().enumerate() {
            if field.kind == "external" {
                answer.insert(field.key.clone(), Value::Bool(true));
                continue;
            }
            if field.hidden {
                if !field.default.is_null() {
                    answer.insert(field.key.clone(), field.default.clone());
                }
                continue;
            }
            let active = field.when.iter().all(|when| {
                let Some(value) = answer.get(&when.key) else { return false };
                let hit = match value {
                    Value::Array(items) => items.iter().any(|item| item == &when.value),
                    value => value == &when.value,
                };
                if when.op == "eq" {
                    hit
                } else {
                    !hit
                }
            });
            if !active {
                continue;
            }
            let text = self.editors.get(index).map(|editor| editor.text.trim().to_string()).unwrap_or_default();
            match field.kind.as_str() {
                "boolean" => {
                    answer.insert(field.key.clone(), Value::Bool(self.toggles.get(index).copied().unwrap_or(false)));
                }
                "number" | "integer" => {
                    if text.is_empty() {
                        if field.required {
                            return Err(format!("{} is required", field.title));
                        }
                        continue;
                    }
                    if field.kind == "integer" {
                        let number: i64 = text.parse().map_err(|_| format!("{} must be an integer", field.title))?;
                        answer.insert(field.key.clone(), Value::from(number));
                    } else {
                        let number: f64 = text.parse().map_err(|_| format!("{} must be a number", field.title))?;
                        if !number.is_finite() {
                            return Err(format!("{} must be a number", field.title));
                        }
                        answer.insert(field.key.clone(), Value::from(number));
                    }
                }
                "multiselect" => {
                    let mut values = Vec::new();
                    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                        let value = field.option_value(item).unwrap_or_else(|| item.to_string());
                        field.validate_option(&value)?;
                        values.push(value);
                    }
                    if values.is_empty() {
                        if field.required {
                            return Err(format!("{} is required", field.title));
                        }
                        continue;
                    }
                    answer.insert(field.key.clone(), Value::Array(values.into_iter().map(Value::String).collect()));
                }
                _ => {
                    if text.is_empty() {
                        if field.required {
                            return Err(format!("{} is required", field.title));
                        }
                        continue;
                    }
                    let value = field.option_value(&text).unwrap_or(text);
                    field.validate_option(&value)?;
                    answer.insert(field.key.clone(), Value::String(value));
                }
            }
        }
        Ok(Value::Object(answer))
    }

    pub fn draw(&self, buf: &mut Buffer, area: Rect, theme: &Theme, form: &Form) -> Option<(u16, u16)> {
        let tokens = &theme.dialog;
        let mut body: Vec<Line<'static>> = Vec::new();
        let mut cursor: Option<(usize, usize)> = None;
        for (index, field) in self.fields.iter().enumerate() {
            if field.hidden {
                continue;
            }
            let required = if field.required { " *" } else { "" };
            body.push(line(
                vec![ratatui::text::Span::styled(format!("{}{required}", field.title), draw::bold(draw::st(tokens.text, tokens.bg)))],
                tokens.bg,
            ));
            if !field.description.is_empty() {
                body.push(line(vec![span(draw::truncate(&field.description, 52), tokens.muted, tokens.bg)], tokens.bg));
            }
            if field.kind == "external" {
                body.push(line(vec![span(format!("→ {}", field.url), tokens.muted, tokens.bg)], tokens.bg));
                continue;
            }
            if field.kind == "boolean" {
                let on = self.toggles.get(index).copied().unwrap_or(false);
                let marker = if index == self.active { "▸ " } else { "  " };
                let hint = if index == self.active { "  space toggles" } else { "" };
                body.push(line(
                    vec![
                        span(marker, tokens.text_primary.focused, tokens.bg),
                        span(if on { "[x]" } else { "[ ]" }, tokens.text, tokens.bg),
                        span(hint, tokens.muted, tokens.bg),
                    ],
                    tokens.bg,
                ));
                continue;
            }
            let text = self.editors.get(index).map(|editor| editor.text.clone()).unwrap_or_default();
            if index == self.active {
                let column = self.editors.get(index).map(|editor| draw::width(&editor.text[..editor.cursor])).unwrap_or(0);
                let mut spans = vec![span("▸ ", tokens.text_primary.focused, tokens.bg), span(text.clone(), tokens.text_formfield.focused, tokens.bg)];
                if text.is_empty() {
                    spans.push(span("type a value", tokens.muted, tokens.bg));
                }
                cursor = Some((body.len(), column));
                body.push(line(spans, tokens.bg));
            } else {
                let shown = if text.is_empty() { "—".to_string() } else { draw::truncate(&text, 48) };
                body.push(line(vec![span("  ", tokens.text, tokens.bg), span(shown, tokens.muted, tokens.bg)], tokens.bg));
            }
            if !field.options.is_empty() {
                let options = field.options.iter().map(|(value, label)| if value == label { value.clone() } else { format!("{label} ({value})") }).collect::<Vec<_>>().join(", ");
                body.push(line(vec![span("  ", tokens.text, tokens.bg), span(draw::truncate(&format!("options: {options}"), 52), tokens.muted, tokens.bg)], tokens.bg));
            }
        }
        body.push(line(vec![], tokens.bg));
        body.push(line(
            vec![
                span("enter ", tokens.text, tokens.bg),
                span("submit", tokens.muted, tokens.bg),
                span("  tab ", tokens.text, tokens.bg),
                span("next field", tokens.muted, tokens.bg),
                span("  esc ", tokens.text, tokens.bg),
                span("cancel", tokens.muted, tokens.bg),
            ],
            tokens.bg,
        ));
        let title = if form.title.is_empty() { "Form" } else { &form.title };
        let panel = dialog::draw_text(buf, area, theme, title, "esc", body, false);
        cursor.map(|(row, column)| {
            let column = column.min(panel.width.saturating_sub(dialog::INSET + 4) as usize) as u16;
            let x = panel.x + dialog::INSET + 2 + column;
            let y = panel.y + 3 + row as u16;
            (x.min(panel.x + panel.width.saturating_sub(dialog::INSET)), y)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forms::Status;
    use crate::tui::theme::{self, Mode};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn form() -> Form {
        Form {
            id: "frm_1".into(),
            session_id: "ses_1".into(),
            title: "Connect account".into(),
            metadata: Value::Null,
            fields: json!([
                { "key": "name", "title": "Name", "type": "string", "required": true },
                { "key": "remember", "title": "Remember", "type": "boolean", "default": true },
                { "key": "email", "title": "Email", "type": "string", "required": true, "when": [{ "key": "name", "op": "eq", "value": "Ada" }] },
                { "key": "port", "title": "Port", "type": "integer", "default": 8080 }
            ]),
            status: Status::Pending,
            replies: json!({}),
            message: None,
            time_created: 0,
            time_updated: 0,
        }
    }

    #[test]
    fn answer_uses_types_defaults_and_conditions() {
        let entry = form();
        let mut ui = FormUi::default();
        ui.sync(&entry);
        ui.editors[0].set("Bob");
        let answer = ui.answer().unwrap();
        assert_eq!(answer, json!({ "name": "Bob", "remember": true, "port": 8080 }));
        ui.editors[0].set("Ada");
        assert!(ui.answer().is_err(), "the conditional required field becomes active");
        ui.editors[2].set("ada@example.com");
        assert_eq!(ui.answer().unwrap(), json!({ "name": "Ada", "remember": true, "email": "ada@example.com", "port": 8080 }));
    }

    #[test]
    fn draws_title_fields_and_hint() {
        let entry = form();
        let mut ui = FormUi::default();
        ui.sync(&entry);
        let theme = theme::fallback(Mode::Dark);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                ui.draw(frame.buffer_mut(), area, &theme, &entry);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                screen.push_str(buffer[(x, y)].symbol());
            }
            screen.push('\n');
        }
        assert!(screen.contains("Connect account"));
        assert!(screen.contains("Name *") && screen.contains("Remember"));
        assert!(screen.contains("enter submit") && screen.contains("esc cancel"));
    }
}

//! A2UI component tree → Discord message JSON (D-1110). Pure functions, so the rendering rules of
//! the legacy `DiscordRenderer` (`34903f00^:crates/discord/src/renderer.rs`) are testable without
//! Discord: text from the root Column (h1/h2/h3/caption), up to 5 buttons per action row, one row
//! per select menu, at most 5 rows, labels ≤ 80 chars, custom ids ≤ 100 chars.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{json, Value};

/// One A2UI v0.9 component (`{id, component, ...}`).
#[derive(Debug, Clone, Deserialize)]
pub struct Component {
    pub id: String,
    #[serde(flatten)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "component")]
pub enum Kind {
    Text {
        text: String,
        #[serde(default)]
        variant: Option<String>,
    },
    Button {
        text: String,
        action: Action,
        #[serde(default)]
        style: Option<String>,
        #[serde(default)]
        emoji: Option<String>,
        #[serde(default)]
        disabled: bool,
    },
    Row {
        children: Vec<String>,
    },
    Column {
        children: Vec<String>,
    },
    SelectMenu {
        options: Vec<SelectOption>,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        min_values: Option<u32>,
        #[serde(default)]
        max_values: Option<u32>,
        action: Action,
    },
    TextInput {
        label: String,
        #[serde(default)]
        placeholder: Option<String>,
        #[serde(default)]
        min_length: Option<u32>,
        #[serde(default)]
        max_length: Option<u32>,
        #[serde(default = "default_true")]
        required: bool,
        #[serde(default)]
        style: Option<String>,
    },
    Form {
        title: String,
        children: Vec<String>,
        action: Action,
    },
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct SelectOption {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub emoji: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Action {
    pub name: String,
    #[serde(default)]
    pub context: Option<Value>,
}

pub fn parse_components(value: &Value) -> Result<Vec<Component>, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("コンポーネントのパースに失敗: {e}"))
}

/// `interaction:{ui_id}:{component_id}:{action_name}`, cut to Discord's 100 chars.
pub fn custom_id(ui_id: &str, component_id: &str, action_name: &str) -> String {
    let id = format!("interaction:{ui_id}:{component_id}:{action_name}");
    id.chars().take(100).collect()
}

/// The modal's custom id: `interaction:{ui_id}:modal:{action_name}`.
pub fn modal_custom_id(ui_id: &str, action_name: &str) -> String {
    custom_id(ui_id, "modal", action_name)
}

/// (ui_id, component_id, action_name) of one of our custom ids; others are `None`.
pub fn parse_custom_id(custom_id: &str) -> Option<(String, String, String)> {
    let parts: Vec<&str> = custom_id.splitn(4, ':').collect();
    match parts.as_slice() {
        ["interaction", ui, component, action] => {
            Some((ui.to_string(), component.to_string(), action.to_string()))
        }
        _ => None,
    }
}

fn find<'a>(components: &'a [Component], id: &str) -> Option<&'a Component> {
    components.iter().find(|c| c.id == id)
}

/// The ids under the root Column, or every id when there is no Column.
fn root_children(components: &[Component]) -> Vec<&str> {
    match components.iter().find_map(|c| match &c.kind {
        Kind::Column { children } => Some(children),
        _ => None,
    }) {
        Some(children) => children.iter().map(String::as_str).collect(),
        None => components.iter().map(|c| c.id.as_str()).collect(),
    }
}

/// The message text: Text components in root order, one per line.
pub fn message_text(components: &[Component]) -> String {
    root_children(components)
        .into_iter()
        .filter_map(|id| match &find(components, id)?.kind {
            Kind::Text { text, variant } => Some(match variant.as_deref() {
                Some("h1") => format!("# {text}"),
                Some("h2") => format!("## {text}"),
                Some("h3") => format!("### {text}"),
                Some("caption") => format!("-# {text}"),
                _ => text.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn button_style(style: Option<&str>) -> u8 {
    match style {
        Some("secondary") => 2,
        Some("success") => 3,
        Some("danger") => 4,
        _ => 1,
    }
}

fn button(ui_id: &str, component: &Component) -> Result<Option<Value>, String> {
    let Kind::Button {
        text,
        action,
        style,
        emoji,
        disabled,
    } = &component.kind
    else {
        return Ok(None);
    };
    let label = if text.chars().count() > 80 {
        text.chars().take(77).collect::<String>() + "..."
    } else {
        text.clone()
    };
    let mut b = json!({
        "type": 2,
        "custom_id": custom_id(ui_id, &component.id, &action.name),
        "label": label,
        "style": button_style(style.as_deref()),
        "disabled": disabled,
    });
    if let Some(emoji) = emoji {
        b["emoji"] = json!({"name": emoji});
    }
    Ok(Some(b))
}

fn select_menu(ui_id: &str, component: &Component) -> Option<Value> {
    let Kind::SelectMenu {
        options,
        placeholder,
        min_values,
        max_values,
        action,
    } = &component.kind
    else {
        return None;
    };
    let options: Vec<Value> = options
        .iter()
        .map(|o| {
            let mut v = json!({"label": o.label, "value": o.value});
            if let Some(d) = &o.description {
                v["description"] = json!(d);
            }
            if let Some(e) = &o.emoji {
                v["emoji"] = json!({"name": e});
            }
            if o.default {
                v["default"] = json!(true);
            }
            v
        })
        .collect();
    let mut menu = json!({
        "type": 3,
        "custom_id": custom_id(ui_id, &component.id, &action.name),
        "options": options,
    });
    if let Some(p) = placeholder {
        menu["placeholder"] = json!(p);
    }
    if let Some(n) = min_values {
        menu["min_values"] = json!(n);
    }
    if let Some(n) = max_values {
        menu["max_values"] = json!(n);
    }
    Some(json!({"type": 1, "components": [menu]}))
}

/// The action rows of the message. More than 5 rows is an error (Discord's limit).
pub fn action_rows(ui_id: &str, components: &[Component]) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    let mut emitted_selects: HashSet<&str> = HashSet::new();
    for child in root_children(components) {
        if emitted_selects.contains(child) {
            continue;
        }
        let Some(comp) = find(components, child) else {
            continue;
        };
        match &comp.kind {
            Kind::Row { children } => {
                let has_select = children.iter().any(|id| {
                    find(components, id).is_some_and(|c| matches!(c.kind, Kind::SelectMenu { .. }))
                });
                if has_select {
                    for id in children {
                        if emitted_selects.contains(id.as_str()) {
                            continue;
                        }
                        if let Some(row) = find(components, id).and_then(|c| select_menu(ui_id, c))
                        {
                            emitted_selects.insert(id.as_str());
                            rows.push(row);
                        }
                    }
                } else {
                    let mut buttons = Vec::new();
                    for id in children {
                        let c = find(components, id)
                            .ok_or_else(|| format!("Component not found: {id}"))?;
                        if let Some(b) = button(ui_id, c)? {
                            buttons.push(b);
                        }
                    }
                    for chunk in buttons.chunks(5) {
                        rows.push(json!({"type": 1, "components": chunk}));
                    }
                }
            }
            Kind::SelectMenu { .. } => {
                if let Some(row) = select_menu(ui_id, comp) {
                    emitted_selects.insert(child);
                    rows.push(row);
                }
            }
            _ => {}
        }
    }
    if rows.len() > 5 {
        return Err(format!("Too many action rows: {} (max 5)", rows.len()));
    }
    Ok(rows)
}

/// The rows with every button and select menu disabled (after an answer or a timeout).
pub fn disabled_rows(rows: &Value) -> Value {
    let mut rows = rows.clone();
    if let Some(rows) = rows.as_array_mut() {
        for row in rows {
            if let Some(items) = row.get_mut("components").and_then(Value::as_array_mut) {
                for item in items {
                    if matches!(item.get("type").and_then(Value::as_u64), Some(2) | Some(3)) {
                        item["disabled"] = json!(true);
                    }
                }
            }
        }
    }
    rows
}

/// The modal a Button opens when its action name is the (first) Form's action name; `None`
/// otherwise. Errors (bad tree, more than 5 fields) also give `None`: the click is then an
/// ordinary button answer, as before.
pub fn form_modal(ui_id: &str, components: &[Component], button_id: &str) -> Option<Value> {
    let Kind::Button { action, .. } = &find(components, button_id)?.kind else {
        return None;
    };
    let (title, children, form_action) = components.iter().find_map(|c| match &c.kind {
        Kind::Form {
            title,
            children,
            action,
        } => Some((title, children, action)),
        _ => None,
    })?;
    if form_action.name != action.name {
        return None;
    }
    let mut rows = Vec::new();
    for id in children {
        let Kind::TextInput {
            label,
            placeholder,
            min_length,
            max_length,
            required,
            style,
        } = &find(components, id)?.kind
        else {
            continue;
        };
        let input_style = if style.as_deref() == Some("paragraph") {
            2
        } else {
            1
        };
        let mut input = json!({
            "type": 4,
            "custom_id": id,
            "label": label,
            "style": input_style,
            "required": required,
        });
        if let Some(p) = placeholder {
            input["placeholder"] = json!(p);
        }
        if let Some(n) = min_length {
            input["min_length"] = json!(n);
        }
        if let Some(n) = max_length {
            input["max_length"] = json!(n);
        }
        rows.push(json!({"type": 1, "components": [input]}));
    }
    if rows.is_empty() || rows.len() > 5 {
        return None;
    }
    Some(json!({
        "custom_id": modal_custom_id(ui_id, &form_action.name),
        "title": title,
        "components": rows,
    }))
}

/// The declared context of a button (sent to the agent with the click).
pub fn button_context(components: &[Component], component_id: &str) -> Option<Value> {
    match &find(components, component_id)?.kind {
        Kind::Button { action, .. } => action.context.clone(),
        _ => None,
    }
}

/// The label of a component, for the readable line the agent receives.
pub fn label_of(components: &[Component], component_id: &str) -> Option<String> {
    match &find(components, component_id)?.kind {
        Kind::Button { text, .. } => Some(text.clone()),
        Kind::SelectMenu { placeholder, .. } => placeholder.clone(),
        Kind::Form { title, .. } => Some(title.clone()),
        _ => None,
    }
}

/// The labels of the chosen select options (in the order chosen).
pub fn option_labels(
    components: &[Component],
    component_id: &str,
    values: &[String],
) -> Vec<String> {
    let Some(Kind::SelectMenu { options, .. }) = find(components, component_id).map(|c| &c.kind)
    else {
        return values.to_vec();
    };
    values
        .iter()
        .map(|v| {
            options
                .iter()
                .find(|o| &o.value == v)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| v.clone())
        })
        .collect()
}

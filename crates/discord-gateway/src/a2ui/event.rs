//! A component interaction (button, select menu, modal submit) as one JSON line on the child's
//! receive path (D-1110), like `/model` interactions. Only our custom ids (`interaction:…`) are
//! forwarded; other bots' components never reach here (Discord sends an interaction only to the
//! application that owns the message).

use serde::{Deserialize, Serialize};
use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, ModalInteraction};

pub const A2UI_EVENT_KIND: &str = "a2ui_interaction";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Button,
    Select,
    Modal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InteractionEvent {
    pub event_kind: String,
    pub interaction_id: String,
    pub token: String,
    pub custom_id: String,
    pub channel_id: String,
    /// The message holding the components (none for a modal submit).
    pub message_id: Option<String>,
    pub user_id: String,
    /// The clicker's visible name (nick, then global name, then user name).
    pub user_label: Option<String>,
    pub kind: InteractionKind,
    /// The chosen values of a select menu.
    #[serde(default)]
    pub values: Vec<String>,
    /// The (field id, value) pairs of a modal submit.
    #[serde(default)]
    pub fields: Vec<(String, String)>,
}

impl InteractionEvent {
    pub fn parse(line: &str) -> Option<Self> {
        let event: Self = serde_json::from_str(line).ok()?;
        (event.event_kind == A2UI_EVENT_KIND).then_some(event)
    }
}

fn label(user: &serenity::all::User, nick: Option<&str>) -> Option<String> {
    nick.map(str::to_string)
        .or_else(|| user.global_name.clone())
        .or_else(|| Some(user.name.clone()))
}

pub fn component_line(c: &ComponentInteraction) -> Option<String> {
    if !c.data.custom_id.starts_with("interaction:") {
        return None;
    }
    let (kind, values) = match &c.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => {
            (InteractionKind::Select, values.clone())
        }
        _ => (InteractionKind::Button, Vec::new()),
    };
    let event = InteractionEvent {
        event_kind: A2UI_EVENT_KIND.to_string(),
        interaction_id: c.id.get().to_string(),
        token: c.token.clone(),
        custom_id: c.data.custom_id.clone(),
        channel_id: c.channel_id.get().to_string(),
        message_id: Some(c.message.id.get().to_string()),
        user_id: c.user.id.get().to_string(),
        user_label: label(&c.user, c.member.as_ref().and_then(|m| m.nick.as_deref())),
        kind,
        values,
        fields: Vec::new(),
    };
    serde_json::to_string(&event).ok()
}

pub fn modal_line(m: &ModalInteraction) -> Option<String> {
    if !m.data.custom_id.starts_with("interaction:") {
        return None;
    }
    let mut fields = Vec::new();
    for row in &m.data.components {
        for component in &row.components {
            if let serenity::all::ActionRowComponent::InputText(input) = component {
                fields.push((
                    input.custom_id.clone(),
                    input.value.clone().unwrap_or_default(),
                ));
            }
        }
    }
    let event = InteractionEvent {
        event_kind: A2UI_EVENT_KIND.to_string(),
        interaction_id: m.id.get().to_string(),
        token: m.token.clone(),
        custom_id: m.data.custom_id.clone(),
        channel_id: m.channel_id.get().to_string(),
        message_id: m.message.as_ref().map(|msg| msg.id.get().to_string()),
        user_id: m.user.id.get().to_string(),
        user_label: label(
            &m.user,
            m.member.as_ref().and_then(|mem| mem.nick.as_deref()),
        ),
        kind: InteractionKind::Modal,
        values: Vec::new(),
        fields,
    };
    serde_json::to_string(&event).ok()
}

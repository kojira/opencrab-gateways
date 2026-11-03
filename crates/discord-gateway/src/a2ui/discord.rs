//! The Discord REST calls A2UI needs (D-1110): post a message with components, edit its
//! components/content, answer an interaction. Kept apart from [`crate::transport::DiscordTransport`]
//! so the other transports stay as they are.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait A2uiDiscord: Send + Sync {
    /// Posts `{content, components}`; returns the message id.
    async fn send(&self, channel_id: &str, body: &Value) -> Result<String, String>;
    /// Edits the message with `body` (e.g. disabled components, appended content).
    async fn edit(&self, channel_id: &str, message_id: &str, body: &Value) -> Result<(), String>;
    /// Answers an interaction (`type` 6 = acknowledge without changes, 9 = open a modal).
    async fn respond(&self, interaction_id: &str, token: &str, body: &Value) -> Result<(), String>;
}

/// Production: serenity's HTTP client (bot token stays inside it).
pub struct SerenityA2ui {
    http: Arc<serenity::http::Http>,
}

impl SerenityA2ui {
    pub fn new(http: Arc<serenity::http::Http>) -> Self {
        Self { http }
    }
}

fn id(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>()
        .map_err(|_| format!("not a snowflake: {raw}"))
}

fn err(e: serenity::Error) -> String {
    crate::secret::redact_token(&e.to_string())
}

#[async_trait]
impl A2uiDiscord for SerenityA2ui {
    async fn send(&self, channel_id: &str, body: &Value) -> Result<String, String> {
        let channel = serenity::all::ChannelId::new(id(channel_id)?);
        let message = self
            .http
            .send_message(channel, Vec::new(), body)
            .await
            .map_err(err)?;
        Ok(message.id.get().to_string())
    }

    async fn edit(&self, channel_id: &str, message_id: &str, body: &Value) -> Result<(), String> {
        let channel = serenity::all::ChannelId::new(id(channel_id)?);
        let message = serenity::all::MessageId::new(id(message_id)?);
        self.http
            .edit_message(channel, message, body, Vec::new())
            .await
            .map(|_| ())
            .map_err(err)
    }

    async fn respond(&self, interaction_id: &str, token: &str, body: &Value) -> Result<(), String> {
        let interaction = serenity::all::InteractionId::new(id(interaction_id)?);
        self.http
            .create_interaction_response(interaction, token, body, Vec::new())
            .await
            .map_err(err)
    }
}

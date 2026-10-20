//! `/webhook` (D-1087): このセッションの活動 webhook 宛先を表示・設定・解除する。
//!
//! `/model` `/thinking` と同じ流儀（チャンネル判定・オーナー限定・ephemeral・defer →
//! edit）。違いは宛先の持ち主で、こちらは core にコマンドを送らず **gateway の DB** へ
//! 書く（webhook は gateway の概念なので core に置かない）。

use serde::{Deserialize, Serialize};
use serenity::all::{CommandOptionType, CreateCommand, CreateCommandOption};

pub(crate) const WEBHOOK_EVENT_KIND: &str = "webhook_interaction";
/// この gateway が作る webhook の名前。再利用の判定にだけ使う。
pub(crate) const WEBHOOK_NAME: &str = "opencrab activity";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct WebhookInteractionEvent {
    pub event_kind: String,
    pub interaction_id: String,
    pub application_id: String,
    pub token: String,
    pub guild_id: Option<String>,
    pub channel_id: String,
    pub user_id: String,
    pub subcommand: String,
    /// `set` の送信先チャンネル（省略時はこのチャンネル）。
    pub destination_channel_id: Option<String>,
}

pub(crate) fn webhook_command() -> CreateCommand {
    let show = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "show",
        "Show the activity webhook destination of this session",
    );
    let set = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "set",
        "Send this session's agent activity to a webhook in the chosen channel",
    )
    .add_sub_option(
        CreateCommandOption::new(
            CommandOptionType::Channel,
            "channel",
            "Destination channel. Default: this channel",
        )
        .channel_types(vec![serenity::all::ChannelType::Text]),
    );
    let clear = CreateCommandOption::new(
        CommandOptionType::SubCommand,
        "clear",
        "Stop sending this session's agent activity",
    );
    CreateCommand::new("webhook")
        .description("Configure the agent activity webhook for this session")
        .set_options(vec![show, set, clear])
}

/// `show` / `set` / `clear` のうち、こちらが扱うものだけを受ける。
pub(crate) fn parse_subcommand(subcommand: &str) -> Option<WebhookSubcommand> {
    match subcommand {
        "show" => Some(WebhookSubcommand::Show),
        "set" => Some(WebhookSubcommand::Set),
        "clear" => Some(WebhookSubcommand::Clear),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WebhookSubcommand {
    Show,
    Set,
    Clear,
}

/// `show` の応答本文。宛先 URL は伏字だけを出す。
pub(crate) fn show_text(state: Option<(&str, bool)>) -> String {
    match state {
        Some((redacted, enabled)) if enabled => {
            format!("Activity webhook: `{redacted}` (active for this session)")
        }
        Some((redacted, _)) => format!(
            "Activity webhook: `{redacted}` (disabled; use /webhook set to enable it again)"
        ),
        None => {
            "Activity webhook: not set for this session. Use /webhook set [channel].".to_string()
        }
    }
}

/// `set` の応答本文。
pub(crate) fn set_text(redacted: &str, destination: &str) -> String {
    format!("Activity webhook set to `{redacted}` (channel: {destination})")
}

/// `clear` の応答本文。
pub(crate) fn clear_text(removed: bool) -> String {
    if removed {
        "Activity webhook cleared for this session.".to_string()
    } else {
        "No activity webhook was set for this session.".to_string()
    }
}

/// オーナー以外・送信先不明などの失敗文。
pub(crate) fn error_text(message: &str) -> String {
    format!("The webhook command failed: {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_reports_unset_active_and_disabled() {
        assert!(show_text(None).contains("not set"));
        assert!(show_text(Some((
            "https://discord.com/api/webhooks/1/[redacted]",
            true
        )))
        .contains("active"));
        assert!(show_text(Some((
            "https://discord.com/api/webhooks/1/[redacted]",
            false
        )))
        .contains("disabled"));
    }

    #[test]
    fn show_never_contains_a_raw_url() {
        let text = show_text(Some((
            "https://discord.com/api/webhooks/1/[redacted]",
            true,
        )));
        assert!(!text.contains("/token"));
    }

    #[test]
    fn set_and_clear_wording_names_the_destination() {
        assert!(set_text("https://example/[redacted]", "123").contains("channel: 123"));
        assert!(clear_text(true).contains("cleared"));
        assert!(clear_text(false).contains("No activity webhook"));
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(parse_subcommand("enable").is_none());
        assert_eq!(parse_subcommand("set"), Some(WebhookSubcommand::Set));
    }
}

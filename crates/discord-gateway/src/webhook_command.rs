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

/// 応答に出す配送先チャンネル。`/webhook show` は保存済みの id から名前を引いて作る。
/// 名前が引けない（チャンネルが消えた・権限が無い）ときは id だけを出す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Destination {
    pub id: String,
    pub name: Option<String>,
}

impl Destination {
    /// 応答用の表示。名前が分かるときだけ `#名前 (id)`、無ければ id だけ。
    pub(crate) fn label(&self) -> String {
        match &self.name {
            Some(name) if !name.trim().is_empty() => format!("#{} ({})", name.trim(), self.id),
            _ => self.id.clone(),
        }
    }
}

/// 宛先行のうち応答に出す部分（URL・送信先・有効/無効）。
pub(crate) struct RouteView<'a> {
    /// 生の URL。**オーナー本人だけが見る ephemeral の応答にだけ出す**（ログには出さない）。
    pub url: &'a str,
    /// 保存されている送信先チャンネル。列を足す前の行では `None`。
    pub destination: Option<&'a Destination>,
    pub enabled: bool,
}

/// 宛先の 1 行分（URL・送信先・状態）を出す。`show` と `set` で同じ形にする。
fn route_lines(head: &str, view: &RouteView<'_>) -> String {
    let channel = match view.destination {
        Some(destination) => destination.label(),
        // 名前も id も無い行（列を足す前に設定された）。設定し直せば埋まる。
        None => "unknown (run /webhook set to record it)".to_string(),
    };
    format!("{head}\nURL: {}\nChannel: {channel}", view.url)
}

/// `show` の応答本文。ephemeral（実行者だけが見える）なので URL は生値で出す。
pub(crate) fn show_text(state: Option<RouteView<'_>>) -> String {
    match state {
        Some(view) if view.enabled => {
            route_lines("Activity webhook: active for this session", &view)
        }
        Some(view) => route_lines(
            "Activity webhook: disabled (use /webhook set to enable it again)",
            &view,
        ),
        None => {
            "Activity webhook: not set for this session. Use /webhook set [channel].".to_string()
        }
    }
}

/// `set` の応答本文（`show` と同じ形）。
pub(crate) fn set_text(view: &RouteView<'_>) -> String {
    route_lines("Activity webhook set: active for this session", view)
}

/// 設定変更をログに残す。応答と違ってログは残るので、URL は伏字だけを出す。
pub(crate) fn log_destination_set(agent_id: &str, destination: &str, url: &str) {
    tracing::info!(
        agent_id,
        destination,
        url = %crate::webhook::redact_webhook_url(url),
        "activity webhook destination set"
    );
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

    const URL: &str = "https://discord.com/api/webhooks/42/the-whole-token-value";

    fn destination() -> Destination {
        Destination {
            id: "987654321".to_string(),
            name: Some("activity-log".to_string()),
        }
    }

    fn view(enabled: bool, destination: Option<&Destination>) -> RouteView<'_> {
        RouteView {
            url: URL,
            destination,
            enabled,
        }
    }

    #[test]
    fn show_reports_unset_active_and_disabled() {
        assert!(show_text(None).contains("not set"));
        let destination = destination();
        assert!(show_text(Some(view(true, Some(&destination)))).contains("active for this session"));
        assert!(show_text(Some(view(false, Some(&destination)))).contains("disabled"));
    }

    /// 実行者のみが見える ephemeral 応答なので、URL は伏せずそのまま出して検証できるようにする。
    #[test]
    fn show_prints_the_whole_url_so_the_owner_can_check_it() {
        let destination = destination();
        let text = show_text(Some(view(true, Some(&destination))));
        assert!(text.contains(URL), "{text}");
        assert!(!text.contains("[redacted]"), "{text}");
        assert!(!text.contains('*'), "{text}");
    }

    /// 名前と id の両方を出す（`表示するならチャンネル名も`）。
    #[test]
    fn show_names_the_destination_channel_with_its_id() {
        let destination = destination();
        let text = show_text(Some(view(true, Some(&destination))));
        assert!(text.contains("#activity-log (987654321)"), "{text}");
    }

    /// 名前が引けなければ id だけ、id も保存されていなければその旨を出す。
    #[test]
    fn show_falls_back_to_the_id_then_to_an_explicit_unknown() {
        let unnamed = Destination {
            id: "987654321".to_string(),
            name: None,
        };
        let text = show_text(Some(view(true, Some(&unnamed))));
        assert!(text.contains("Channel: 987654321"), "{text}");
        let legacy = show_text(Some(view(true, None)));
        assert!(legacy.contains("run /webhook set to record it"), "{legacy}");
    }

    /// `set` は `show` と同じ形（URL・チャンネル名 + id・状態）。
    #[test]
    fn set_and_show_are_symmetric() {
        let destination = destination();
        let set = set_text(&view(true, Some(&destination)));
        let show = show_text(Some(view(true, Some(&destination))));
        for line in [
            format!("URL: {URL}"),
            "Channel: #activity-log (987654321)".to_string(),
        ] {
            assert!(set.contains(&line), "set: {set}");
            assert!(show.contains(&line), "show: {show}");
        }
    }

    /// 応答は生 URL を出すが、ログは伏字だけ（ephemeral の外に URL を出さない）。
    #[test]
    fn the_set_log_line_carries_only_the_redacted_url() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let layer = UrlCapture { sink };
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            log_destination_set("agent-1", "987654321", URL);
        });
        let captured = seen.lock().unwrap();
        assert_eq!(captured.len(), 1, "{captured:?}");
        assert!(captured[0].ends_with("/[redacted]"), "{}", captured[0]);
        assert!(
            !captured[0].contains("the-whole-token-value"),
            "{}",
            captured[0]
        );
    }

    struct UrlCapture {
        sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for UrlCapture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = UrlVisitor::default();
            event.record(&mut visitor);
            if let Some(url) = visitor.url {
                self.sink.lock().unwrap().push(url);
            }
        }
    }

    #[derive(Default)]
    struct UrlVisitor {
        url: Option<String>,
    }

    impl tracing::field::Visit for UrlVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "url" {
                self.url = Some(value.to_string());
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "url" {
                self.url = Some(format!("{value:?}"));
            }
        }
    }

    #[test]
    fn clear_wording_reports_removal() {
        assert!(clear_text(true).contains("cleared"));
        assert!(clear_text(false).contains("No activity webhook"));
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(parse_subcommand("enable").is_none());
        assert_eq!(parse_subcommand("set"), Some(WebhookSubcommand::Set));
    }
}

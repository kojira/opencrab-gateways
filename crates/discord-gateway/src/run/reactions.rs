//! System reactions (👀 🏁 ❌ 🤐) on Discord messages, best effort.

use std::sync::Arc;

use crate::map::parse_origin;
use crate::transport::{DiscordTransport, TransportOutcome};

/// system reaction を発端メッセージ（origin anchor）へ best-effort で付ける。👀（受理）と
/// ❌（配送失敗）はこちら——発端に付けるのが正（照合: 旧 message_loop も同じ側）。
pub(super) async fn react_system(transport: &Arc<dyn DiscordTransport>, origin: &str, emoji: &str) {
    let Some((channel, message)) = parse_origin(origin) else {
        tracing::debug!(%origin, "skip system reaction: origin not a discord anchor");
        return;
    };
    react_system_on(transport, &channel, &message, emoji).await;
}

pub(super) async fn react_no_reply(
    transport: &Arc<dyn DiscordTransport>,
    reply_origin: Option<&str>,
    emoji: &str,
) {
    if let Some(origin) = reply_origin {
        react_system(transport, origin, emoji).await;
    }
}

/// (channel, message) を直接指定して system reaction を付ける（失敗は warn のみ・非致命）。
/// 🏁（完了）は activity ended で core が指定した**最終生成の最後の投稿**へ付けるため、
/// 発話 id の対応表から得た自分の message id をここへ渡す。
/// legacy `add_reaction_non_fatal` と同じく、付与失敗が turn 処理を巻き込まないようにする。
pub(super) async fn react_system_on(
    transport: &Arc<dyn DiscordTransport>,
    channel: &str,
    message: &str,
    emoji: &str,
) {
    match transport.add_system_reaction(channel, message, emoji).await {
        TransportOutcome::Ok(_) => {}
        TransportOutcome::Rejected => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction rejected (non-fatal)"
            )
        }
        TransportOutcome::Indeterminate => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction outcome unknown (non-fatal)"
            )
        }
    }
}

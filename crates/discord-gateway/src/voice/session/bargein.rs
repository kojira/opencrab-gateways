//! D-1096: VC バージイン。ボットが話している間（読み上げ中・相づちの再生中・読み上げ待ちがある）に、
//! ボット以外の誰かの途中認識が [`super::speculation::FIRST_PROVISIONAL_CHARS`] 文字に達したら、
//! 発話ごとに 1 回だけ止める。
//!
//! 止めるときは、再生中と再生待ちの音声を捨て、積んだ文を読まず、止めた返事の stream でこの後に
//! 届く partial と say も読まない（文字の投稿は今のまま）。core へは `interrupt` で、相手に届いた
//! 分（流れ終わった文）を送る。割り込んだ発話は今までどおり said になる。

use super::VoiceManager;

impl VoiceManager {
    /// その guild でボットが話している（再生中・再生待ち・読み上げ待ちがある）。
    pub(super) async fn bot_speaking(&self, guild: u64) -> bool {
        self.playbacks.has_queued(guild) || !self.player.is_idle(guild).await
    }

    /// ボットの声を止め、core へ `interrupt` を送る。
    pub(super) async fn barge_in(&self, guild: u64) {
        let Some(session) = self.sessions.lock().unwrap().get(&guild).cloned() else {
            return;
        };
        let cut = self.playbacks.cut(guild);
        if let Err(error) = self.player.stop(guild).await {
            tracing::warn!(guild, error = %format!("{error:#}"), "voice playback stop failed");
        }
        if let Some(stream) = &cut.stream {
            self.cut_streams
                .lock()
                .unwrap()
                .insert(session.text_channel.clone(), stream.clone());
        }
        self.drafts.lock().unwrap().remove(&session.text_channel);
        self.clear_promoted_holds(&session.text_channel);
        let Some(client) = self.client.get() else {
            return;
        };
        let delivered_chars = cut.delivered.chars().count();
        let result = client
            .interrupt(&session.address, Some(&cut.delivered))
            .await;
        tracing::info!(
            guild,
            delivered_chars,
            ok = result.is_ok(),
            "voice barge-in; reply interrupted"
        );
    }

    /// 止めた返事の stream なら true（その partial・say は読まない）。
    pub(super) fn is_cut_stream(&self, text_channel: &str, stream_id: &str) -> bool {
        self.cut_streams
            .lock()
            .unwrap()
            .get(text_channel)
            .is_some_and(|cut| cut == stream_id)
    }
}

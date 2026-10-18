//! D-1085 P5（§10・§11）: 話している間の先行生成（provisional）。
//!
//! roleplay-demo と同じく、話している間は 200ms ごとに途中までを認識し、5 文字に達したら先行
//! 生成を始め、前の先行生成から 8 文字以上増えたら新しい先行生成に置き換える（古いものは中止）。
//! 無音 100ms で全体を認識し、本文が変わっていれば置き換える。確定ではその本文を STT し直さずに
//! said にするので、本文は先行生成と一致し、core が昇格させる。
//!
//! 先行生成の partial は再生せず、TTS キャッシュへ合成だけしておく（保留）。昇格の知らせ
//! （provisional_result）が来たら相づち、保留した文の順に流し、以後の partial と最終 say は
//! 残りだけを読む。昇格しなければ保留を捨て、通常のターンの読み上げに任せる。

use std::sync::{Arc, Mutex};

use opencrab_gate_client::client::LiveEvent;
use opencrab_gate_client::wire::ProvisionalRequest;

use super::{SpeakerWork, VoiceManager, VOICE_SYSTEM_CONTEXT, VOICE_TEXT_PREFIX};
use crate::voice::audio::{ends_like_sentence, SegmenterEvent};
use crate::voice::draft::DraftState;
use crate::voice::settings::load_settings;
use crate::voice::synthesizer::SynthesisPriority;

/// 途中認識がこの文字数に達したら最初の先行生成を始める。
pub(super) const FIRST_PROVISIONAL_CHARS: usize = 5;
/// 前の先行生成の本文からこの文字数以上増えたら、新しい先行生成に置き換える。
pub(super) const REPLACE_PROVISIONAL_CHARS: usize = 8;

/// 1 度だけ流す相づち（said の受理と昇格の知らせの先に来た方が流す）。
pub(super) type AizuchiSlot = Arc<Mutex<Option<(&'static str, Vec<u8>)>>>;

/// core に送った先行生成。
pub(super) struct Started {
    pub(super) id: String,
    /// 先行生成の本文（「（音声）」を付ける前の認識結果）。
    pub(super) text: String,
    pub(super) prelude: Option<&'static str>,
    pub(super) slot: AizuchiSlot,
}

/// 先行生成の返事の保留（provisional_id ごと）。
pub(super) struct Hold {
    pub(super) text_channel: String,
    stream_id: Option<String>,
    /// 保留中に届いた partial の最新の累積本文。
    latest_text: String,
    /// 合成に回した文（重ねて合成しないため）。
    draft: Option<DraftState>,
    slot: AizuchiSlot,
    pub(super) committed_at: Option<std::time::Instant>,
    /// 昇格したが、その時点で partial がまだ届いていなかった（以後の partial は通常どおり読む）。
    promoted: bool,
}

/// 1 話者分の先行生成の状態（話者の処理キューの中だけで使う）。
#[derive(Default)]
pub(super) struct Speculation {
    utt: Option<u64>,
    current: Option<Started>,
    /// この発話の最後の話し終わり候補（無音 100ms）の認識結果。
    onset: Option<(u64, String)>,
}

/// 後ろに新しい出来事があれば飛ばしてよい途中認識（話している間のもの）。
pub(super) fn is_stale_snapshot(work: &SpeakerWork) -> bool {
    matches!(
        work,
        SpeakerWork::Event(SegmenterEvent::Snapshot { onset: false, .. })
    )
}

fn chars(text: &str) -> usize {
    text.trim().chars().count()
}

impl VoiceManager {
    pub(super) async fn on_speech_event(
        &self,
        guild: u64,
        user: u64,
        speculation: &mut Speculation,
        event: SegmenterEvent,
    ) {
        match event {
            SegmenterEvent::Snapshot {
                utt,
                seq,
                pcm,
                onset,
                verdict,
            } => {
                if speculation.utt != Some(utt) {
                    self.cancel_current(guild, speculation).await;
                    speculation.utt = Some(utt);
                    speculation.onset = None;
                }
                let Some(text) = self.transcribe_pcm(guild, user, &pcm).await else {
                    if onset {
                        verdict.set(seq, false);
                    }
                    return;
                };
                let text = text.trim().to_string();
                if onset {
                    let sentence_like = ends_like_sentence(&text);
                    verdict.set(seq, sentence_like);
                    tracing::debug!(guild, user, seq, sentence_like, "speech end candidate");
                    speculation.onset = Some((seq, text.clone()));
                }
                let replace = match &speculation.current {
                    None if onset => !text.is_empty(),
                    None => chars(&text) >= FIRST_PROVISIONAL_CHARS,
                    Some(current) if onset => current.text != text && !text.is_empty(),
                    Some(current) => {
                        chars(&text) >= chars(&current.text) + REPLACE_PROVISIONAL_CHARS
                    }
                };
                if replace {
                    tracing::debug!(guild, user, seq, onset, "provisional trigger");
                    self.cancel_current(guild, speculation).await;
                    speculation.current = self.start_provisional(guild, user, &text).await;
                }
            }
            SegmenterEvent::End {
                utt,
                pcm,
                onset_seq,
                reason,
                quiet_packets,
            } => {
                tracing::debug!(
                    guild,
                    user,
                    ?reason,
                    trailing_quiet_ms = quiet_packets * 20,
                    "speech committed"
                );
                let reused = match (onset_seq, &speculation.onset) {
                    (Some(seq), Some((onset, text)))
                        if speculation.utt == Some(utt) && seq == *onset =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                };
                let text = match reused {
                    Some(text) => text,
                    None => self
                        .transcribe_pcm(guild, user, &pcm)
                        .await
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                };
                let current = speculation.current.take();
                speculation.utt = None;
                speculation.onset = None;
                let provisional = match current {
                    Some(current) if current.text == text => Some(current),
                    Some(current) => {
                        self.cancel_started(guild, current).await;
                        None
                    }
                    None => None,
                };
                if text.is_empty() {
                    if let Some(current) = provisional {
                        self.cancel_started(guild, current).await;
                    }
                    return;
                }
                self.commit(guild, user, &text, provisional).await;
            }
            SegmenterEvent::Discarded { .. } => {
                self.cancel_current(guild, speculation).await;
                speculation.utt = None;
                speculation.onset = None;
            }
        }
    }

    /// この本文で先行生成を始める。相づちもこの本文から選び、先行生成と said の両方に載せる。
    async fn start_provisional(&self, guild: u64, user: u64, text: &str) -> Option<Started> {
        let session = self.sessions.lock().unwrap().get(&guild).cloned()?;
        let client = self.client.get()?;
        let settings = load_settings(&self.config.settings_path).ok()?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let aizuchi = self
            .ready_aizuchi(guild, &session.text_channel, &settings, text, nanos)
            .await;
        let prelude = aizuchi.as_ref().map(|(text, _)| *text);
        let slot: AizuchiSlot = Arc::new(Mutex::new(aizuchi));
        let id = uuid::Uuid::new_v4().to_string();
        self.holds.lock().unwrap().insert(
            id.clone(),
            Hold {
                text_channel: session.text_channel.clone(),
                stream_id: None,
                latest_text: String::new(),
                draft: None,
                slot: slot.clone(),
                committed_at: None,
                promoted: false,
            },
        );
        let user_id = user.to_string();
        let request = ProvisionalRequest {
            provisional_id: id.clone(),
            author_id: user_id.clone(),
            author_label: self.label(user).await,
            caller: crate::run::caller_for(&self.config.access, &user_id),
            system_context: Some(VOICE_SYSTEM_CONTEXT.to_string()),
            agent_prelude: prelude.map(str::to_string),
            text: format!("{VOICE_TEXT_PREFIX}{text}"),
        };
        match client.provisional(&session.address, &request).await {
            Ok(()) => {
                tracing::debug!(guild, user, provisional_id = %id, text_chars = chars(text), "provisional sent");
                Some(Started {
                    id,
                    text: text.to_string(),
                    prelude,
                    slot,
                })
            }
            Err(error) => {
                self.holds.lock().unwrap().remove(&id);
                tracing::info!(guild, user, ?error, "provisional not started");
                None
            }
        }
    }

    async fn cancel_current(&self, guild: u64, speculation: &mut Speculation) {
        if let Some(current) = speculation.current.take() {
            self.cancel_started(guild, current).await;
        }
    }

    async fn cancel_started(&self, guild: u64, started: Started) {
        self.holds.lock().unwrap().remove(&started.id);
        let Some(session) = self.sessions.lock().unwrap().get(&guild).cloned() else {
            return;
        };
        let Some(client) = self.client.get() else {
            return;
        };
        let result = client
            .provisional_cancel(&session.address, &started.id)
            .await;
        tracing::debug!(guild, provisional_id = %started.id, ok = result.is_ok(), "provisional cancelled");
    }

    /// 先行生成の partial。保留中なら合成だけして再生しない。昇格した後の partial は通常の
    /// 読み上げ（[`Self::speak_partial`]）に回す。中止・不採用のものは捨てる。
    /// 昇格した先行生成のうち partial が来ないまま say が届いたもの（そのチャンネルの印）を消す。
    pub(super) fn clear_promoted_holds(&self, text_channel: &str) {
        self.holds
            .lock()
            .unwrap()
            .retain(|_, hold| !(hold.promoted && hold.text_channel == text_channel));
    }

    /// core の partial と昇格の知らせ。先行生成（`provisional_id` 付き）の partial は昇格まで
    /// 保留する（D-1085）。それ以外の出来事は無視する。
    pub async fn on_live_speech(&self, text_channel: &str, event: LiveEvent) {
        match event {
            LiveEvent::Partial {
                stream_id,
                text,
                provisional_id: Some(provisional_id),
            } => self.hold_partial(text_channel, &stream_id, &text, &provisional_id),
            LiveEvent::Partial {
                stream_id, text, ..
            } => self.speak_partial(text_channel, &stream_id, &text),
            LiveEvent::ProvisionalResult {
                provisional_id,
                promoted,
            } => {
                self.on_provisional_result(text_channel, &provisional_id, promoted)
                    .await
            }
            _ => {}
        }
    }

    fn hold_partial(&self, text_channel: &str, stream_id: &str, text: &str, provisional_id: &str) {
        let to_synthesize = {
            let mut holds = self.holds.lock().unwrap();
            match holds.get_mut(provisional_id) {
                Some(hold) if hold.promoted => {
                    let committed_at = hold.committed_at;
                    holds.remove(provisional_id);
                    drop(holds);
                    let mut draft = DraftState::new(stream_id);
                    let speech = draft.advance(text);
                    self.drafts
                        .lock()
                        .unwrap()
                        .insert(text_channel.to_string(), draft);
                    self.enqueue_speech_from(text_channel, &speech, committed_at);
                    return;
                }
                Some(hold) => {
                    if hold.stream_id.as_deref() != Some(stream_id) {
                        hold.stream_id = Some(stream_id.to_string());
                        hold.draft = Some(DraftState::new(stream_id));
                    }
                    hold.latest_text = text.to_string();
                    hold.draft
                        .as_mut()
                        .map(|draft| draft.advance(text))
                        .unwrap_or_default()
                }
                None => {
                    drop(holds);
                    let promoted = self
                        .drafts
                        .lock()
                        .unwrap()
                        .get(text_channel)
                        .is_some_and(|draft| draft.stream_id == stream_id);
                    if promoted {
                        self.speak_partial(text_channel, stream_id, text);
                    }
                    return;
                }
            }
        };
        if to_synthesize.is_empty() {
            return;
        }
        let Ok(settings) = load_settings(&self.config.settings_path) else {
            return;
        };
        let style = settings
            .tts
            .voice_for_agent(&self.config.agent_id)
            .to_string();
        for segment in to_synthesize {
            let synthesizer = self.synthesizer.clone();
            let tts = settings.tts.clone();
            let style = style.clone();
            tokio::spawn(async move {
                if let Err(error) = synthesizer
                    .synthesize(&tts, &style, &segment, SynthesisPriority::Held)
                    .await
                {
                    tracing::debug!(error = %format!("{error:#}"), "held synthesis failed");
                }
            });
        }
        tracing::debug!(%text_channel, provisional_id, "provisional partial held");
    }

    /// core の昇格の知らせ。昇格なら相づち、保留した文の順に流し、残りを読めるよう読み上げ状態を
    /// 引き継ぐ。昇格しなければ保留を捨てる。
    async fn on_provisional_result(
        &self,
        text_channel: &str,
        provisional_id: &str,
        promoted: bool,
    ) {
        let Some(mut hold) = self.holds.lock().unwrap().remove(provisional_id) else {
            return;
        };
        tracing::debug!(%text_channel, provisional_id, promoted, "provisional result");
        if !promoted {
            return;
        }
        let aizuchi = hold.slot.lock().unwrap().take();
        if let Some((aizuchi, wav)) = aizuchi {
            for guild in self.session_guilds(&hold.text_channel) {
                match self.player.play(guild, wav.clone()).await {
                    Ok(()) => tracing::debug!(guild, aizuchi, "aizuchi played"),
                    Err(error) => {
                        tracing::warn!(guild, error = %format!("{error:#}"), "aizuchi playback failed")
                    }
                }
            }
        }
        let Some(stream_id) = hold.stream_id.clone() else {
            // まだ partial が無い。以後の partial を（保留せず）読むよう印を付けて残す。
            hold.promoted = true;
            self.holds
                .lock()
                .unwrap()
                .insert(provisional_id.to_string(), hold);
            return;
        };
        let mut draft = DraftState::new(&stream_id);
        let speech = draft.advance(&hold.latest_text);
        self.drafts
            .lock()
            .unwrap()
            .insert(hold.text_channel.clone(), draft);
        self.enqueue_speech_from(&hold.text_channel, &speech, hold.committed_at);
    }
}

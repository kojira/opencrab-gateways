//! 読み上げの再生（文ごとに合成して再生キューへ積む）と、その再生状態（D-1096）。
//!
//! 再生状態は guild ごとに持ち、バージインで止めるための世代と、相手に届いた文を数えるための
//! 再生中の文を覚える。
//! - 世代: 読み上げキューに積んだ文は積んだ時点の世代を持つ。バージインで世代を進めると、古い世代の
//!   文は合成済みでも再生しない。
//! - 返事: 同じ stream の文は 1 つの返事として数える。stream が変わる（または stream の無い投稿の
//!   読み上げ）たびに新しい返事を積んだ順に足す。前の返事がまだ流れていても、その返事の届いた分は
//!   その返事に数える（後から積んだ返事で消さない）。
//! - 届いた文: 再生へ渡した文のうち、トラックが最後まで流れたもの（songbird のトラック終了）。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::VoicePlayer;
use crate::voice::settings::load_settings;
use crate::voice::synthesizer::{SynthesisPriority, Synthesizer};
use crate::voice::tts_text;

/// 読み上げキューの 1 件。
pub(super) struct Speech {
    pub(super) guild: u64,
    pub(super) text: String,
    /// 先行生成が昇格した返事の最初の文なら、その発話を確定した時刻（確定から再生までを記録する）。
    pub(super) committed_at: Option<std::time::Instant>,
    pub(super) generation: u64,
    pub(super) reply: u64,
}

/// 再生へ渡し、まだ流れ終わっていない文。
struct Playing {
    id: u64,
    reply: u64,
    text: String,
}

/// 1 つの返事（同じ stream の文のまとまり）の再生状態。
struct Reply {
    id: u64,
    /// 返事の stream（stream の無い投稿の読み上げなら None）。
    stream: Option<String>,
    /// いまの世代で読み上げキューに積み、まだ合成・再生を終えていない件数。
    queued: usize,
    /// 流れ終わった文。
    delivered: String,
}

impl Reply {
    fn in_progress(&self, playing: &VecDeque<Playing>) -> bool {
        self.queued > 0 || playing.iter().any(|playing| playing.reply == self.id)
    }
}

#[derive(Default)]
struct GuildPlayback {
    generation: u64,
    next_reply: u64,
    /// 積んだ順の返事。最後の 1 つは流れ終わっても残す（同じ stream の続きを同じ返事に数える）。
    replies: VecDeque<Reply>,
    playing: VecDeque<Playing>,
}

impl GuildPlayback {
    /// 流れ終わった返事を忘れる（最後の 1 つは残す）。
    fn forget_finished(&mut self) {
        let playing = &self.playing;
        let last = self.replies.back().map(|reply| reply.id);
        self.replies
            .retain(|reply| Some(reply.id) == last || reply.in_progress(playing));
    }
}

/// バージインで止めた返事（[`Playbacks::cut`]）。
pub(super) struct Cut {
    /// 相手に届いた分。まだ流れている返事のうち最も古いもの（いま聞こえていた返事）の、流れ終わった文。
    pub(super) delivered: String,
    /// いま聞こえていた返事の stream。
    pub(super) stream: Option<String>,
    /// 止めた返事の stream（積まれているか流れている返事すべて）。この後に届くこれらの文は読まない。
    pub(super) cut_streams: Vec<String>,
}

#[derive(Default)]
pub(super) struct Playbacks {
    guilds: Mutex<HashMap<u64, GuildPlayback>>,
    next_id: AtomicU64,
}

impl Playbacks {
    /// 読み上げキューに積む文の世代と返事の番号。`stream` が最後の返事と違う、または None なら新しい返事。
    pub(super) fn enqueue(&self, guild: u64, stream: Option<&str>) -> (u64, u64) {
        let mut guilds = self.guilds.lock().unwrap();
        let playback = guilds.entry(guild).or_default();
        let same = stream.is_some()
            && playback
                .replies
                .back()
                .is_some_and(|reply| reply.stream.as_deref() == stream);
        if !same {
            playback.next_reply += 1;
            let id = playback.next_reply;
            playback.replies.push_back(Reply {
                id,
                stream: stream.map(str::to_string),
                queued: 0,
                delivered: String::new(),
            });
            playback.forget_finished();
        }
        let reply = playback.replies.back_mut().expect("reply was just ensured");
        reply.queued += 1;
        (playback.generation, reply.id)
    }

    fn is_current(&self, guild: u64, generation: u64) -> bool {
        self.guilds
            .lock()
            .unwrap()
            .get(&guild)
            .is_some_and(|playback| playback.generation == generation)
    }

    fn done(&self, guild: u64, generation: u64, reply: u64) {
        if let Some(playback) = self.guilds.lock().unwrap().get_mut(&guild) {
            if playback.generation == generation {
                if let Some(reply) = playback.replies.iter_mut().find(|r| r.id == reply) {
                    reply.queued = reply.queued.saturating_sub(1);
                }
                playback.forget_finished();
            }
        }
    }

    /// 再生へ渡す文を覚え、その id を返す。世代が古ければ None（再生しない）。
    fn start(&self, guild: u64, generation: u64, reply: u64, text: &str) -> Option<u64> {
        let mut guilds = self.guilds.lock().unwrap();
        let playback = guilds.get_mut(&guild)?;
        if playback.generation != generation {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        playback.playing.push_back(Playing {
            id,
            reply,
            text: text.to_string(),
        });
        Some(id)
    }

    /// トラック `id` が流れ終わった。その文の返事の届いた分に足す。
    pub(super) fn finished(&self, guild: u64, id: u64) {
        let mut guilds = self.guilds.lock().unwrap();
        let Some(playback) = guilds.get_mut(&guild) else {
            return;
        };
        let Some(index) = playback.playing.iter().position(|playing| playing.id == id) else {
            return;
        };
        let Some(playing) = playback.playing.remove(index) else {
            return;
        };
        if let Some(reply) = playback.replies.iter_mut().find(|r| r.id == playing.reply) {
            tracing::debug!(
                guild,
                stream = reply.stream.as_deref().unwrap_or("-"),
                text_chars = playing.text.chars().count(),
                "TTS track finished playing"
            );
            reply.delivered.push_str(&playing.text);
        }
        playback.forget_finished();
    }

    /// いまの世代で、合成・再生を待っている文がある。
    pub(super) fn has_queued(&self, guild: u64) -> bool {
        self.guilds
            .lock()
            .unwrap()
            .get(&guild)
            .is_some_and(|playback| playback.replies.iter().any(|reply| reply.queued > 0))
    }

    /// 世代を進めて積んだ文と再生中の文を捨て、止めた返事を返す。
    ///
    /// 積まれているか流れている返事が無ければ（最後まで流れ終わった後に相づちだけが鳴っているなど）、
    /// 止めた返事は無い。届いた分は空、stream は None で、どの stream も止めない。
    pub(super) fn cut(&self, guild: u64) -> Cut {
        let mut guilds = self.guilds.lock().unwrap();
        let playback = guilds.entry(guild).or_default();
        let (in_progress, idle): (Vec<Reply>, Vec<Reply>) = {
            let playing = &playback.playing;
            std::mem::take(&mut playback.replies)
                .into_iter()
                .partition(|reply| reply.in_progress(playing))
        };
        playback.generation += 1;
        playback.playing.clear();
        playback.replies = idle.into_iter().collect();
        let Some(heard) = in_progress.first() else {
            return Cut {
                delivered: String::new(),
                stream: None,
                cut_streams: Vec::new(),
            };
        };
        Cut {
            delivered: heard.delivered.clone(),
            stream: heard.stream.clone(),
            cut_streams: in_progress
                .iter()
                .filter_map(|reply| reply.stream.clone())
                .collect(),
        }
    }

    pub(super) fn forget(&self, guild: u64) {
        self.guilds.lock().unwrap().remove(&guild);
    }
}

pub(super) async fn speak_serially(
    mut queue: mpsc::UnboundedReceiver<Speech>,
    settings_path: PathBuf,
    agent_id: String,
    player: Arc<dyn VoicePlayer>,
    synthesizer: Arc<Synthesizer>,
    pending_speech: Arc<AtomicUsize>,
    playbacks: Arc<Playbacks>,
) {
    while let Some(speech) = queue.recv().await {
        let (guild, generation, reply) = (speech.guild, speech.generation, speech.reply);
        if playbacks.is_current(guild, generation) {
            speak_one(
                speech,
                &settings_path,
                &agent_id,
                player.as_ref(),
                &synthesizer,
                &playbacks,
            )
            .await;
        }
        playbacks.done(guild, generation, reply);
        pending_speech.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn speak_one(
    speech: Speech,
    settings_path: &std::path::Path,
    agent_id: &str,
    player: &dyn VoicePlayer,
    synthesizer: &Arc<Synthesizer>,
    playbacks: &Arc<Playbacks>,
) {
    let Speech {
        guild,
        text,
        committed_at,
        generation,
        reply,
    } = speech;
    let settings = match load_settings(settings_path) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(guild, error = %format!("{error:#}"), "TTS playback failed");
            return;
        }
    };
    synthesizer.refresh_if_changed(&settings.tts);
    let voice = settings.tts.choice_for_agent(agent_id);
    // 文ごとに合成し、合成できた文から再生キューへ積む（play は songbird へ積むだけ）。
    // 1 文の失敗は warn にして次の文へ進む。止められた（世代が古い）ら残りは読まない。
    let mut committed_at = committed_at;
    for sentence in tts_text::split_sentences(&text) {
        let committed_at = committed_at.take();
        let result = async {
            let text_chars = sentence.chars().count();
            let wav = synthesizer
                .synthesize(&settings.tts, &voice, &sentence, SynthesisPriority::Live)
                .await?;
            // 合成中に止められたら、合成した音声は再生しない。
            let Some(id) = playbacks.start(guild, generation, reply, &sentence) else {
                return anyhow::Ok(false);
            };
            let finished = playbacks.clone();
            player
                .play_speech(guild, wav, Box::new(move || finished.finished(guild, id)))
                .await?;
            tracing::debug!(guild, text_chars, "TTS audio enqueued to the player");
            if let Some(committed_at) = committed_at {
                tracing::debug!(
                    guild,
                    commit_to_play_ms = committed_at.elapsed().as_millis() as u64,
                    "promoted reply first played"
                );
            }
            anyhow::Ok(true)
        }
        .await;
        match result {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(guild, "speech dropped; interrupted");
                return;
            }
            Err(error) => {
                tracing::warn!(guild, error = %format!("{error:#}"), "TTS playback failed")
            }
        }
    }
}

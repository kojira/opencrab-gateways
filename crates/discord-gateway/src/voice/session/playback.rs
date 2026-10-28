//! 読み上げの再生（文ごとに合成して再生キューへ積む）と、その再生状態（D-1096）。
//!
//! 再生状態は guild ごとに持ち、バージインで止めるための世代と、相手に届いた文を数えるための
//! 再生中の文を覚える。
//! - 世代: 読み上げキューに積んだ文は積んだ時点の世代を持つ。バージインで世代を進めると、古い世代の
//!   文は合成済みでも再生しない。
//! - 返事: 同じ stream の文は 1 つの返事として数える。stream が変わる（または stream の無い投稿の
//!   読み上げ）たびに新しい返事にし、届いた文はその返事の分だけを数える。
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

#[derive(Default)]
struct GuildPlayback {
    generation: u64,
    reply: u64,
    /// いまの返事の stream（stream の無い投稿の読み上げなら None）。
    stream: Option<String>,
    /// いまの世代で読み上げキューに積み、まだ合成・再生を終えていない件数。
    queued: usize,
    playing: VecDeque<Playing>,
    /// いまの返事のうち、流れ終わった文。
    delivered: String,
}

/// バージインで止めた返事（[`Playbacks::cut`]）。
pub(super) struct Cut {
    /// 相手に届いた分（流れ終わった文をつなげたもの）。
    pub(super) delivered: String,
    /// 止めた返事の stream。この後に届く同じ stream の文は読まない。
    pub(super) stream: Option<String>,
}

#[derive(Default)]
pub(super) struct Playbacks {
    guilds: Mutex<HashMap<u64, GuildPlayback>>,
    next_id: AtomicU64,
}

impl Playbacks {
    /// 読み上げキューに積む文の世代と返事の番号。`stream` が前と違う、または None なら新しい返事。
    pub(super) fn enqueue(&self, guild: u64, stream: Option<&str>) -> (u64, u64) {
        let mut guilds = self.guilds.lock().unwrap();
        let playback = guilds.entry(guild).or_default();
        if stream.is_none() || playback.stream.as_deref() != stream {
            playback.reply += 1;
            playback.stream = stream.map(str::to_string);
            playback.delivered.clear();
        }
        playback.queued += 1;
        (playback.generation, playback.reply)
    }

    fn is_current(&self, guild: u64, generation: u64) -> bool {
        self.guilds
            .lock()
            .unwrap()
            .get(&guild)
            .is_some_and(|playback| playback.generation == generation)
    }

    fn done(&self, guild: u64, generation: u64) {
        if let Some(playback) = self.guilds.lock().unwrap().get_mut(&guild) {
            if playback.generation == generation {
                playback.queued = playback.queued.saturating_sub(1);
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

    /// トラック `id` が流れ終わった。いまの返事の文なら届いた分に足す。
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
        if playing.reply == playback.reply {
            playback.delivered.push_str(&playing.text);
        }
    }

    /// いまの世代で、合成・再生を待っている文がある。
    pub(super) fn has_queued(&self, guild: u64) -> bool {
        self.guilds
            .lock()
            .unwrap()
            .get(&guild)
            .is_some_and(|playback| playback.queued > 0)
    }

    /// 世代を進めて積んだ文と再生中の文を捨て、止めた返事を返す。
    ///
    /// いまの返事の文が積まれても再生中でもなければ（最後まで流れ終わった後に相づちだけが
    /// 鳴っているなど）、止めた返事は無い。届いた分は空、stream は None を返す。
    pub(super) fn cut(&self, guild: u64) -> Cut {
        let mut guilds = self.guilds.lock().unwrap();
        let playback = guilds.entry(guild).or_default();
        let reply_in_progress = playback.queued > 0 || !playback.playing.is_empty();
        playback.generation += 1;
        playback.queued = 0;
        playback.playing.clear();
        if !reply_in_progress {
            return Cut {
                delivered: String::new(),
                stream: None,
            };
        }
        Cut {
            delivered: std::mem::take(&mut playback.delivered),
            stream: playback.stream.take(),
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
        let (guild, generation) = (speech.guild, speech.generation);
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
        playbacks.done(guild, generation);
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
    let voice = settings.tts.voice_for_agent(agent_id);
    // 文ごとに合成し、合成できた文から再生キューへ積む（play は songbird へ積むだけ）。
    // 1 文の失敗は warn にして次の文へ進む。止められた（世代が古い）ら残りは読まない。
    let mut committed_at = committed_at;
    for sentence in tts_text::split_sentences(&text) {
        let committed_at = committed_at.take();
        let result = async {
            let text_chars = sentence.chars().count();
            let wav = synthesizer
                .synthesize(&settings.tts, voice, &sentence, SynthesisPriority::Live)
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

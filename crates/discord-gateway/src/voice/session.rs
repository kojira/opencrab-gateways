//! VC セッション管理。join/leave、発話→STT→said、投稿→TTS→再生を受け持つ。
//! songbird への依存は [`VoicePlayer`] の裏（[`super::songbird_player`]）に閉じる。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use opencrab_gate_client::client::{InstanceClient, PostRefuse, SaidOutcome};
use opencrab_gate_client::wire::{LiveInboundScope, SaidContext};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::audio::{downmix_48k_stereo_to_16k_mono, pcm_to_wav, rms};
use super::settings::load_settings;
use super::{clients, tts_text};
use crate::config::AccessConfig;
use crate::map::{parse_address, voice_origin_for};
use crate::transport::{DiscordTransport, TransportOutcome};

/// これより小さい RMS（16kHz モノラル）の区間は無音・雑音として STT へ送らない。
const MIN_SEGMENT_RMS: f64 = 250.0;
/// said 本文の先頭に付け、音声由来の発言だと読み手（LLM・人）に分かるようにする。
const VOICE_TEXT_PREFIX: &str = "（音声）";

/// VC への接続と再生。production は songbird、テストは記録用の偽物。
#[async_trait::async_trait]
pub trait VoicePlayer: Send + Sync {
    /// VC へ入り、受信音声を `manager.process_segment` へ渡し始める。
    async fn join(
        &self,
        guild: u64,
        channel: u64,
        manager: Arc<VoiceManager>,
    ) -> anyhow::Result<()>;
    async fn leave(&self, guild: u64) -> anyhow::Result<()>;
    /// WAV を再生キューの末尾へ積む（順番に流れる）。
    async fn play(&self, guild: u64, wav: Vec<u8>) -> anyhow::Result<()>;
}

pub struct VoiceManagerConfig {
    pub agent_id: String,
    pub self_bot_id: String,
    pub access: AccessConfig,
    /// この instance が受け持つ binding address（`discord-{agent}-{guild}-{channel}`）。
    pub addresses: Vec<String>,
    pub settings_path: PathBuf,
}

#[derive(Clone)]
struct ActiveSession {
    vc_channel: u64,
    text_channel: String,
    address: String,
}

/// 1 話者分の発話キュー（48kHz ステレオ PCM の区間）。
type SegmentQueue = mpsc::UnboundedSender<Vec<i16>>;

struct Speech {
    guild: u64,
    text: String,
}

pub struct VoiceManager {
    config: VoiceManagerConfig,
    player: Arc<dyn VoicePlayer>,
    transport: Arc<dyn DiscordTransport>,
    client: OnceLock<Arc<InstanceClient>>,
    sessions: Mutex<HashMap<u64, ActiveSession>>,
    labels: Mutex<HashMap<u64, Option<String>>>,
    http: reqwest::Client,
    speech: mpsc::UnboundedSender<Speech>,
    /// (guild, 話者) ごとの発話キュー。同じ話者の STT→said は区切った順に 1 本ずつ流す。
    segments: Mutex<HashMap<(u64, u64), SegmentQueue>>,
    /// binding address ごとの、直近に said が受理された人間の発言者。channel_id を省略した
    /// join_voice の呼びかけ手（core の invoke は発言者を運ばない）。メモリだけに持つ。
    speakers: Mutex<HashMap<String, String>>,
}

impl VoiceManager {
    pub fn new(
        config: VoiceManagerConfig,
        player: Arc<dyn VoicePlayer>,
        transport: Arc<dyn DiscordTransport>,
    ) -> Arc<Self> {
        let (speech, queue) = mpsc::unbounded_channel();
        let http = clients::http_client();
        tokio::spawn(speak_serially(
            queue,
            config.settings_path.clone(),
            config.agent_id.clone(),
            player.clone(),
            http.clone(),
        ));
        Arc::new(Self {
            config,
            player,
            transport,
            client: OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
            labels: Mutex::new(HashMap::new()),
            http,
            speech,
            segments: Mutex::new(HashMap::new()),
            speakers: Mutex::new(HashMap::new()),
        })
    }

    /// core 接続（said の送り先）。invoke handler を先に作るため、生成後に結びつける。
    pub fn attach_client(&self, client: Arc<InstanceClient>) {
        let _ = self.client.set(client);
    }

    pub async fn join(
        self: &Arc<Self>,
        binding_id: &str,
        vc_channel_id: Option<&str>,
        text_channel_id: Option<&str>,
    ) -> Result<Value, String> {
        load_settings(&self.config.settings_path).map_err(|e| format!("{e:#}"))?;
        let (guild, own_channel) = self.conversation(binding_id).await?;
        let text_channel = text_channel_id.unwrap_or(&own_channel).to_string();
        let address =
            crate::map::address_for(&self.config.agent_id, &guild.to_string(), &text_channel);
        if !self.config.addresses.contains(&address) || self.bound(&address).await.is_none() {
            return Err(format!(
                "text channel {text_channel} is not a bound conversation in this guild"
            ));
        }
        let vc_channel = match vc_channel_id {
            Some(id) => parse_id(id).ok_or("channel_id must be a channel id")?,
            None => self.caller_vc(guild, &own_channel).await?,
        };
        self.player
            .join(guild, vc_channel, self.clone())
            .await
            .map_err(|e| format!("joining the voice channel failed: {e:#}"))?;
        self.sessions.lock().unwrap().insert(
            guild,
            ActiveSession {
                vc_channel,
                text_channel: text_channel.clone(),
                address,
            },
        );
        tracing::info!(guild, vc_channel, %text_channel, "joined voice channel");
        Ok(json!({
            "ok": true,
            "status": "joined",
            "vc_channel_id": vc_channel.to_string(),
            "text_channel_id": text_channel,
            "note": "VC の発言は話者ごとに文字起こしされ、このテキストチャンネルの会話として届く。このチャンネルへの発言は読み上げられる。",
        }))
    }

    /// said として受理された人間の発言者を、その会話（address）の直近の呼びかけ手として覚える。
    pub fn note_speaker(&self, address: &str, user_id: &str) {
        self.speakers
            .lock()
            .unwrap()
            .insert(address.to_string(), user_id.to_string());
    }

    /// この会話で直近に発言した人が、今この guild で入っている VC。見つからなければ理由を返し、
    /// 別の VC を選ぶことはしない。
    async fn caller_vc(&self, guild: u64, own_channel: &str) -> Result<u64, String> {
        let address =
            crate::map::address_for(&self.config.agent_id, &guild.to_string(), own_channel);
        let speaker = self
            .speakers
            .lock()
            .unwrap()
            .get(&address)
            .cloned()
            .ok_or(
                "no recent human speaker in this conversation; pass channel_id to choose the voice channel",
            )?;
        let state = match self
            .transport
            .get_voice_state(&guild.to_string(), &speaker)
            .await
        {
            TransportOutcome::Ok(state) => state,
            TransportOutcome::Rejected => {
                return Err(format!(
                    "the caller (user {speaker}) is not in a voice channel of this server"
                ))
            }
            TransportOutcome::Indeterminate => {
                return Err(format!(
                    "could not look up the voice channel of the caller (user {speaker})"
                ))
            }
        };
        if let Some(state_guild) = state["guild_id"].as_str() {
            if state_guild != guild.to_string() {
                return Err(format!(
                    "the caller (user {speaker}) is in a voice channel of another server"
                ));
            }
        }
        state["channel_id"]
            .as_str()
            .and_then(parse_id)
            .ok_or_else(|| {
                format!("the caller (user {speaker}) is not in a voice channel of this server")
            })
    }

    pub async fn leave(&self, binding_id: &str) -> Result<Value, String> {
        let (guild, _) = self.conversation(binding_id).await?;
        let session = self.sessions.lock().unwrap().remove(&guild);
        self.segments
            .lock()
            .unwrap()
            .retain(|(segment_guild, _), _| *segment_guild != guild);
        self.player
            .leave(guild)
            .await
            .map_err(|e| format!("leaving the voice channel failed: {e:#}"))?;
        tracing::info!(guild, "left voice channel");
        Ok(json!({
            "ok": true,
            "status": "left",
            "vc_channel_id": session.map(|s| s.vc_channel.to_string()),
        }))
    }

    /// 受信側が確定した 1 発話を、その話者のキューへ積む。長い発話は区切られて複数区間になるため、
    /// 同じ話者の区間は 1 つずつ STT→said にして順序を保つ。別の話者は別キューなので待たせない。
    pub fn enqueue_segment(self: &Arc<Self>, guild: u64, user: u64, pcm_48k_stereo: Vec<i16>) {
        let mut segments = self.segments.lock().unwrap();
        let queue = segments
            .entry((guild, user))
            .or_insert_with(|| self.spawn_segment_queue(guild, user));
        // 消費側が止まっていたら（panic 等）キューを作り直して取りこぼさない。
        if let Err(mpsc::error::SendError(pcm)) = queue.send(pcm_48k_stereo) {
            let queue = self.spawn_segment_queue(guild, user);
            let _ = queue.send(pcm);
            segments.insert((guild, user), queue);
        }
    }

    fn spawn_segment_queue(self: &Arc<Self>, guild: u64, user: u64) -> SegmentQueue {
        let (queue, pending) = mpsc::unbounded_channel();
        tokio::spawn(transcribe_serially(
            pending,
            Arc::downgrade(self),
            guild,
            user,
        ));
        queue
    }

    /// 確定した 1 発話を STT にかけ、VC に結びついたテキストチャンネルの said にする。
    pub async fn process_segment(&self, guild: u64, user: u64, pcm_48k_stereo: Vec<i16>) {
        let user_id = user.to_string();
        if user_id == self.config.self_bot_id {
            return;
        }
        let Some(session) = self.sessions.lock().unwrap().get(&guild).cloned() else {
            return;
        };
        let mono = downmix_48k_stereo_to_16k_mono(&pcm_48k_stereo);
        if rms(&mono) < MIN_SEGMENT_RMS {
            return;
        }
        let settings = match load_settings(&self.config.settings_path) {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "voice speech dropped");
                return;
            }
        };
        let wav = pcm_to_wav(&mono, 16_000, 1);
        let text = match clients::transcribe(&self.http, &settings.stt, wav).await {
            Ok(text) if !text.is_empty() => text,
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(user, error = %format!("{error:#}"), "STT failed");
                return;
            }
        };
        let Some(client) = self.client.get() else {
            return;
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let origin = voice_origin_for(&session.text_channel, &guild.to_string(), &user_id, nanos);
        let label = self.label(user).await;
        let context = SaidContext {
            caller: crate::run::caller_for(&self.config.access, &user_id),
            start_turn: true,
            system_context: None,
            reply_target: None,
            live_inbound_scope: LiveInboundScope::All,
        };
        let body = format!("{VOICE_TEXT_PREFIX}{text}");
        let outcome = client
            .post_said_with_context(
                &session.address,
                &origin,
                &user_id,
                label.as_deref(),
                &context,
                &body,
                &[],
            )
            .await;
        match outcome {
            Ok(SaidOutcome::Accepted { seq }) => {
                self.note_speaker(&session.address, &user_id);
                tracing::info!(address = %session.address, seq, "voice said accepted")
            }
            Ok(other) => {
                tracing::info!(address = %session.address, ?other, "voice said not accepted")
            }
            Err(PostRefuse::NotReady) => tracing::info!("voice said dropped; binding not ready"),
            Err(PostRefuse::Busy) => tracing::info!("voice said refused; binding busy"),
        }
    }

    /// テキストチャンネルへ投稿できた本文を、そのチャンネルに結びついた VC で読み上げる。
    /// 合成・再生は順番に行い、失敗は warn だけにする（投稿は妨げない）。
    pub fn speak_posted(&self, text_channel: &str, text: &str) {
        let guilds: Vec<u64> = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, session)| session.text_channel == text_channel)
            .map(|(guild, _)| *guild)
            .collect();
        let text = tts_text::clean_for_tts(text);
        if text.is_empty() {
            return;
        }
        for guild in guilds {
            let _ = self.speech.send(Speech {
                guild,
                text: text.clone(),
            });
        }
    }

    async fn conversation(&self, binding_id: &str) -> Result<(u64, String), String> {
        let client = self.client.get().ok_or("voice is not connected to core")?;
        for address in &self.config.addresses {
            if client.binding_for_address(address).await.as_deref() != Some(binding_id) {
                continue;
            }
            let (guild, channel) = parse_address(&self.config.agent_id, address)
                .ok_or("this conversation is not a Discord channel")?;
            let guild = parse_id(&guild).ok_or("voice needs a guild conversation, not a DM")?;
            return Ok((guild, channel));
        }
        Err("this conversation is not a Discord channel binding of this gateway".into())
    }

    async fn bound(&self, address: &str) -> Option<String> {
        self.client.get()?.binding_for_address(address).await
    }

    async fn label(&self, user: u64) -> Option<String> {
        if let Some(hit) = self.labels.lock().unwrap().get(&user) {
            return hit.clone();
        }
        let label = match self.transport.get_user(&user.to_string()).await {
            TransportOutcome::Ok(v) => v["global_name"]
                .as_str()
                .or_else(|| v["username"].as_str())
                .map(str::to_string),
            _ => None,
        };
        self.labels.lock().unwrap().insert(user, label.clone());
        label
    }
}

fn parse_id(raw: &str) -> Option<u64> {
    raw.parse::<u64>().ok().filter(|id| *id != 0)
}

async fn transcribe_serially(
    mut pending: mpsc::UnboundedReceiver<Vec<i16>>,
    manager: Weak<VoiceManager>,
    guild: u64,
    user: u64,
) {
    while let Some(pcm) = pending.recv().await {
        let Some(manager) = manager.upgrade() else {
            return;
        };
        manager.process_segment(guild, user, pcm).await;
    }
}

async fn speak_serially(
    mut queue: mpsc::UnboundedReceiver<Speech>,
    settings_path: PathBuf,
    agent_id: String,
    player: Arc<dyn VoicePlayer>,
    http: reqwest::Client,
) {
    while let Some(Speech { guild, text }) = queue.recv().await {
        let result = async {
            let settings = load_settings(&settings_path)?;
            let voice = settings.tts.voice_for_agent(&agent_id);
            let wav = clients::synthesize(&http, &settings.tts, &text, voice).await?;
            player.play(guild, wav).await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(guild, error = %format!("{error:#}"), "TTS playback failed");
        }
    }
}

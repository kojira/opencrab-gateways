//! production の [`VoicePlayer`]。songbird 0.6（DAVE 対応）で VC に入り、受信音声を
//! 話者別に区切って [`VoiceManager::process_segment`] へ渡し、WAV を再生キューへ積む。

use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use songbird::driver::{Channels, DecodeConfig, DecodeMode, SampleRate};
use songbird::events::context_data::VoiceTick;
use songbird::id::{ChannelId, GuildId};
use songbird::{CoreEvent, Event, EventContext, EventHandler, Songbird};

use super::receiver::{FinishedSpeech, SsrcSegments};
use super::session::{VoiceManager, VoicePlayer};

/// 受信音声を 48kHz ステレオ PCM に復号させる songbird 設定。
pub fn songbird_config() -> songbird::Config {
    songbird::Config::default().decode_mode(DecodeMode::Decode(DecodeConfig::new(
        Channels::Stereo,
        SampleRate::Hz48000,
    )))
}

pub struct SongbirdPlayer {
    songbird: Arc<Songbird>,
}

impl SongbirdPlayer {
    pub fn new(songbird: Arc<Songbird>) -> Self {
        Self { songbird }
    }
}

fn guild_id(guild: u64) -> anyhow::Result<GuildId> {
    Ok(GuildId(NonZeroU64::new(guild).context("guild id 0")?))
}

#[async_trait::async_trait]
impl VoicePlayer for SongbirdPlayer {
    async fn join(
        &self,
        guild: u64,
        channel: u64,
        manager: Arc<VoiceManager>,
    ) -> anyhow::Result<()> {
        let channel = ChannelId(NonZeroU64::new(channel).context("channel id 0")?);
        let call = self.songbird.get_or_insert(guild_id(guild)?);
        let receiver = Receiver {
            guild,
            manager,
            segments: Arc::new(Mutex::new(SsrcSegments::default())),
        };
        // 受信ハンドラは接続前に張る。接続完了後に張ると、その間の発話（VoiceTick）を取りこぼす。
        // 再 join（VC 移動・注入先変更）では同じ Call が再利用されるので、古いハンドラは外して張り替える。
        let connecting = {
            let mut call = call.lock().await;
            call.remove_all_global_events();
            call.add_global_event(CoreEvent::SpeakingStateUpdate.into(), receiver.clone());
            call.add_global_event(CoreEvent::VoiceTick.into(), receiver.clone());
            call.add_global_event(CoreEvent::ClientDisconnect.into(), receiver);
            call.join(channel).await
        };
        connecting
            .context("songbird join (does the bot have Connect permission?)")?
            .await
            .context("songbird join (does the bot have Connect permission?)")?;
        Ok(())
    }

    async fn leave(&self, guild: u64) -> anyhow::Result<()> {
        self.songbird
            .remove(guild_id(guild)?)
            .await
            .context("not in a voice channel")
    }

    async fn play(&self, guild: u64, wav: Vec<u8>) -> anyhow::Result<()> {
        let call = self
            .songbird
            .get(guild_id(guild)?)
            .context("not in a voice channel")?;
        let mut call = call.lock().await;
        call.enqueue_input(songbird::input::Input::from(wav)).await;
        Ok(())
    }
}

#[derive(Clone)]
struct Receiver {
    guild: u64,
    manager: Arc<VoiceManager>,
    segments: Arc<Mutex<SsrcSegments>>,
}

impl Receiver {
    fn on_tick(&self, tick: &VoiceTick) -> Vec<FinishedSpeech> {
        let mut segments = self.segments.lock().unwrap();
        let mut finished = Vec::new();
        for (ssrc, data) in &tick.speaking {
            if let Some(pcm) = &data.decoded_voice {
                finished.extend(segments.on_frame(*ssrc, pcm));
            }
        }
        for ssrc in &tick.silent {
            finished.extend(segments.on_silent(*ssrc));
        }
        finished
    }

    fn dispatch(&self, finished: Vec<FinishedSpeech>) {
        for (user, pcm) in finished {
            self.manager.enqueue_segment(self.guild, user, pcm);
        }
    }
}

#[async_trait::async_trait]
impl EventHandler for Receiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        let finished = match ctx {
            EventContext::SpeakingStateUpdate(speaking) => {
                if let Some(user) = speaking.user_id {
                    self.segments
                        .lock()
                        .unwrap()
                        .on_speaking(speaking.ssrc, user.0);
                }
                Vec::new()
            }
            EventContext::VoiceTick(tick) => self.on_tick(tick),
            EventContext::ClientDisconnect(gone) => {
                self.segments.lock().unwrap().on_disconnect(gone.user_id.0)
            }
            _ => Vec::new(),
        };
        self.dispatch(finished);
        None
    }
}

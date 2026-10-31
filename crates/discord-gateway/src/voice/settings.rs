//! VC の STT / TTS 接続先設定（D-1072）。gate/discord 配下の `voice/settings.json`（0600）。
//!
//! join・各発話・各読み上げのたびに読み直す（再起動不要）。ファイルが無いときは
//! 「未設定」エラーにし、暗黙の既定エンドポイントへは切り替えない。
//! placement と同じディレクトリに置かないのは、配備ツールが `gate/discord/*.json` を
//! すべて placement として読むため。

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use super::dictionary::{Readings, TtsDictionary};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSettings {
    pub stt: SttSettings,
    pub tts: TtsSettings,
}

/// OpenAI 互換 `/audio/transcriptions` の接続先。`base_url` は `/v1` まで含める。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SttSettings {
    pub base_url: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsSettings {
    /// 既定の読み上げ: `voicevox` か `zen-tts`（D-1103）。bot ごとの指定（`agents`）が優先する。
    pub provider: String,
    /// VOICEVOX の接続先。
    pub base_url: String,
    /// VOICEVOX のスタイル ID（10進）。
    pub default_voice: String,
    /// agent ごとの VOICEVOX スタイル ID。無い agent は `default_voice`。
    #[serde(default)]
    pub agent_voices: BTreeMap<String, String>,
    /// ZEN-TTS サーバー（kojira/zen-tts-server）の接続先と既定値（D-1103）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zen: Option<ZenSettings>,
    /// bot（agent）ごとの読み上げ。`agent_voices` より優先する（D-1103）。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AgentTts>,
    /// 読み上げ辞書（共通と bot ごと。D-1106）。
    #[serde(default, skip_serializing_if = "TtsDictionary::is_empty")]
    pub dictionary: TtsDictionary,
}

/// ZEN-TTS の接続先と、bot が指定しなかったときの声・感情・抑揚。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ZenSettings {
    pub base_url: String,
    pub default_voice: String,
    /// 感情の配合（`neutral` / `happy` / `sad` / `angry` / `apologetic` → 0..=100）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emotion: Option<serde_json::Map<String, serde_json::Value>>,
    /// 抑揚の強さ（1 = 標準）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
}

/// 1 つの bot の読み上げ。省いた項目はその provider の既定値。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTts {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emotion: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
}

pub const PROVIDER_VOICEVOX: &str = "voicevox";
pub const PROVIDER_ZEN_TTS: &str = "zen-tts";
/// ZEN-TTS の感情名（zen-tts の `emotionNames`）。
pub const ZEN_EMOTIONS: [&str; 5] = ["neutral", "happy", "sad", "angry", "apologetic"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsEngine {
    Voicevox,
    ZenTts,
}

/// ある bot が読み上げに使うエンジンと声（合成とキャッシュのキーはこれで決まる）。
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceChoice {
    pub engine: TtsEngine,
    pub voice: String,
    /// ZEN-TTS の感情の配合（VOICEVOX は常に None）。
    pub emotion: Option<serde_json::Value>,
    /// ZEN-TTS の抑揚（VOICEVOX は常に None）。
    pub strength: Option<f64>,
    /// この bot の読み上げ辞書（D-1106）。合成とキャッシュのキーは置き換えた本文で決まる。
    pub readings: Readings,
}

impl VoiceChoice {
    pub fn voicevox(voice: &str) -> Self {
        Self {
            engine: TtsEngine::Voicevox,
            voice: voice.to_string(),
            emotion: None,
            strength: None,
            readings: Readings::default(),
        }
    }

    pub fn zen(voice: &str, emotion: Option<serde_json::Value>, strength: Option<f64>) -> Self {
        Self {
            engine: TtsEngine::ZenTts,
            voice: voice.to_string(),
            emotion,
            strength,
            readings: Readings::default(),
        }
    }

    pub fn with_readings(mut self, readings: Readings) -> Self {
        self.readings = readings;
        self
    }

    /// 辞書を通した、エンジンに渡す本文。
    pub fn reading(&self, text: &str) -> String {
        self.readings.apply(text)
    }
}

impl TtsSettings {
    /// VOICEVOX のスタイル ID（bot の VOICEVOX 指定、`agent_voices`、既定の順）。
    pub fn voice_for_agent(&self, agent_id: &str) -> &str {
        if let Some(agent) = self.agents.get(agent_id) {
            if agent.provider == PROVIDER_VOICEVOX {
                if let Some(voice) = &agent.voice {
                    return voice;
                }
            }
        }
        self.agent_voices
            .get(agent_id)
            .map(String::as_str)
            .unwrap_or(&self.default_voice)
    }

    /// `agent_id` の bot が読み上げに使うエンジンと声。
    pub fn choice_for_agent(&self, agent_id: &str) -> VoiceChoice {
        self.engine_choice(agent_id)
            .with_readings(self.dictionary.for_agent(agent_id))
    }

    fn engine_choice(&self, agent_id: &str) -> VoiceChoice {
        let agent = self.agents.get(agent_id);
        let provider = agent.map_or(self.provider.as_str(), |a| a.provider.as_str());
        match (provider, &self.zen) {
            (PROVIDER_ZEN_TTS, Some(zen)) => {
                let voice = agent
                    .and_then(|a| a.voice.clone())
                    .unwrap_or_else(|| zen.default_voice.clone());
                let emotion = agent
                    .and_then(|a| a.emotion.clone())
                    .or_else(|| zen.emotion.clone())
                    .map(serde_json::Value::Object);
                let strength = agent.and_then(|a| a.strength).or(zen.strength);
                VoiceChoice::zen(&voice, emotion, strength)
            }
            _ => VoiceChoice::voicevox(self.voice_for_agent(agent_id)),
        }
    }

    /// 設定にある全ての声（先読み用。重複なし）。
    pub fn all_choices(&self) -> Vec<VoiceChoice> {
        let mut choices = Vec::new();
        let mut push = |choice: VoiceChoice| {
            if !choices.contains(&choice) {
                choices.push(choice);
            }
        };
        push(self.choice_for_agent(""));
        push(VoiceChoice::voicevox(&self.default_voice));
        for voice in self.agent_voices.values() {
            push(VoiceChoice::voicevox(voice));
        }
        for agent in self.agents.keys() {
            push(self.choice_for_agent(agent));
        }
        choices
    }

    fn validate(&self) -> anyhow::Result<()> {
        self.dictionary.validate()?;
        check_provider("tts.provider", &self.provider)?;
        check_http_url("tts.base_url", &self.base_url)?;
        check_speaker("tts.default_voice", &self.default_voice)?;
        for (agent, voice) in &self.agent_voices {
            if agent.trim().is_empty() {
                bail!("tts.agent_voices has an empty agent id");
            }
            check_speaker("tts.agent_voices", voice)?;
        }
        if let Some(zen) = &self.zen {
            check_http_url("tts.zen.base_url", &zen.base_url)?;
            check_zen_voice("tts.zen.default_voice", &zen.default_voice)?;
            check_emotion("tts.zen.emotion", zen.emotion.as_ref())?;
            check_strength("tts.zen.strength", zen.strength)?;
        }
        if self.provider == PROVIDER_ZEN_TTS && self.zen.is_none() {
            bail!("tts.provider zen-tts needs tts.zen");
        }
        for (agent, tts) in &self.agents {
            if agent.trim().is_empty() {
                bail!("tts.agents has an empty agent id");
            }
            check_provider("tts.agents.provider", &tts.provider)?;
            let zen = tts.provider == PROVIDER_ZEN_TTS;
            if zen && self.zen.is_none() {
                bail!("tts.agents.{agent} uses zen-tts but tts.zen is missing");
            }
            if let Some(voice) = &tts.voice {
                if zen {
                    check_zen_voice("tts.agents.voice", voice)?;
                } else {
                    check_speaker("tts.agents.voice", voice)?;
                }
            }
            check_emotion("tts.agents.emotion", tts.emotion.as_ref())?;
            check_strength("tts.agents.strength", tts.strength)?;
        }
        Ok(())
    }
}

fn check_provider(field: &str, value: &str) -> anyhow::Result<()> {
    if value != PROVIDER_VOICEVOX && value != PROVIDER_ZEN_TTS {
        bail!("{field} must be voicevox or zen-tts");
    }
    Ok(())
}

/// zen-tts の voice id（`marine` など）。一覧はサーバーが持つので形だけ確かめる。
fn check_zen_voice(field: &str, value: &str) -> anyhow::Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !ok {
        bail!("{field} must be a ZEN-TTS voice id");
    }
    Ok(())
}

fn check_emotion(
    field: &str,
    emotion: Option<&serde_json::Map<String, serde_json::Value>>,
) -> anyhow::Result<()> {
    for (name, value) in emotion.into_iter().flatten() {
        if !ZEN_EMOTIONS.contains(&name.as_str()) {
            bail!("{field} has an unknown emotion: {name}");
        }
        match value.as_f64() {
            Some(n) if (0.0..=100.0).contains(&n) => {}
            _ => bail!("{field}.{name} must be a number from 0 to 100"),
        }
    }
    Ok(())
}

fn check_strength(field: &str, strength: Option<f64>) -> anyhow::Result<()> {
    if strength.is_some_and(|s| !s.is_finite() || s < 0.0) {
        bail!("{field} must be a number of 0 or more");
    }
    Ok(())
}

impl VoiceSettings {
    pub fn from_json(raw: &str) -> anyhow::Result<Self> {
        let settings: Self = serde_json::from_str(raw).context("voice settings JSON")?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        check_http_url("stt.base_url", &self.stt.base_url)?;
        if self.stt.model.trim().is_empty() {
            bail!("stt.model is empty");
        }
        if self
            .stt
            .language
            .as_deref()
            .is_some_and(|l| l.trim().is_empty())
        {
            bail!("stt.language is empty");
        }
        self.tts.validate()
    }
}

fn check_http_url(field: &str, value: &str) -> anyhow::Result<()> {
    let rest = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"));
    match rest {
        Some(host) if !host.is_empty() && !value.chars().any(char::is_whitespace) => Ok(()),
        _ => bail!("{field} must be an http(s) URL"),
    }
}

fn check_speaker(field: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 9 {
        bail!("{field} must be a VOICEVOX style id (decimal)");
    }
    Ok(())
}

/// placement（`.../gate/discord/<agent>.json`）から設定ファイルの位置を決める。
pub fn settings_path_for_placement(placement: &Path) -> PathBuf {
    placement
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("voice")
        .join("settings.json")
}

pub fn load_settings(path: &Path) -> anyhow::Result<VoiceSettings> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("voice is not configured (no settings file)")
        }
        Err(error) => return Err(error).context("read voice settings"),
    };
    VoiceSettings::from_json(&raw)
}

/// 検証してから同じディレクトリの一時ファイル（0600）へ書き、rename で置き換える。
pub fn save_settings(path: &Path, settings: &VoiceSettings) -> anyhow::Result<()> {
    settings.validate()?;
    let dir = path.parent().context("settings path has no parent")?;
    std::fs::create_dir_all(dir).context("create voice settings dir")?;
    let tmp = dir.join(format!(".settings.{}.tmp", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(settings)?;
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).context("create temp settings")?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path).context("replace voice settings")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

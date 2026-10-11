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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsSettings {
    /// 現在は `voicevox` だけ。
    pub provider: String,
    pub base_url: String,
    /// VOICEVOX のスタイル ID（10進）。
    pub default_voice: String,
    /// agent ごとのスタイル ID。無い agent は `default_voice`。
    #[serde(default)]
    pub agent_voices: BTreeMap<String, String>,
}

impl TtsSettings {
    pub fn voice_for_agent(&self, agent_id: &str) -> &str {
        self.agent_voices
            .get(agent_id)
            .map(String::as_str)
            .unwrap_or(&self.default_voice)
    }
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
        if self.tts.provider != "voicevox" {
            bail!("tts.provider must be voicevox");
        }
        check_http_url("tts.base_url", &self.tts.base_url)?;
        check_speaker("tts.default_voice", &self.tts.default_voice)?;
        for (agent, voice) in &self.tts.agent_voices {
            if agent.trim().is_empty() {
                bail!("tts.agent_voices has an empty agent id");
            }
            check_speaker("tts.agent_voices", voice)?;
        }
        Ok(())
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

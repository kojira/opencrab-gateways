//! TTS キャッシュ（D-1085）。同じ（スタイル・話速・本文）の音声は VOICEVOX で合成し直さない。
//!
//! - キー: VOICEVOX は `sha256("voicevox" ‖ 0 ‖ スタイル ID ‖ 0 ‖ 話速 ‖ 0 ‖ 本文)`。
//!   ZEN-TTS（D-1103）は `sha256("zen-tts" ‖ 0 ‖ voice ‖ 0 ‖ 感情（キー順）‖ 0 ‖ 抑揚 ‖ 0 ‖ 本文)`。
//! - メモリ: バイト数で上限を掛けた LRU。ディスク: `voice/tts-cache/<key>.wav`（0600、一時ファイル
//!   から rename）。上限を超えたら更新時刻の古い順に消す。起動時にディレクトリを走査して索引を作る。
//! - 有効性: キーに入らない音の違い（エンジンの版・合成の固定値）は `cache-version.json` に記録し、
//!   どちらかが変わったら全部捨てる。エンジンの版を確かめるまで（読めないときも）キャッシュは使わない。

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::clients::{SYNTHESIS_CONSTANTS_VERSION, TTS_SPEED_SCALE};
use super::settings::{TtsEngine, VoiceChoice};

/// メモリに置く音声の上限（バイト）。24kHz モノラルでおよそ 22 分。
pub(crate) const MEMORY_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// ディスクに置く音声の上限（バイト）。
pub(crate) const DISK_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const VERSION_FILE: &str = "cache-version.json";
const TMP_PREFIX: &str = ".tmp-";

/// キャッシュのキー（16 進の sha256）。
pub(crate) fn cache_key(style: &str, speed: f64, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"voicevox");
    hasher.update([0]);
    hasher.update(style.as_bytes());
    hasher.update([0]);
    hasher.update(format!("{speed}").as_bytes());
    hasher.update([0]);
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `choice` の声で `text` を読んだ音声のキー（[`VoiceChoice`] ごとに別の音声）。
pub(crate) fn choice_cache_key(choice: &VoiceChoice, text: &str) -> String {
    let text = &choice.reading(text);
    match choice.engine {
        TtsEngine::Voicevox => cache_key(&choice.voice, TTS_SPEED_SCALE, text),
        TtsEngine::ZenTts => {
            let mut hasher = Sha256::new();
            hasher.update(b"zen-tts");
            hasher.update([0]);
            hasher.update(choice.voice.as_bytes());
            hasher.update([0]);
            // Sorted by name, so the same blend written in another order is the same audio.
            if let Some(serde_json::Value::Object(emotion)) = &choice.emotion {
                let mut parts: Vec<(&String, String)> =
                    emotion.iter().map(|(k, v)| (k, v.to_string())).collect();
                parts.sort();
                for (name, value) in parts {
                    hasher.update(name.as_bytes());
                    hasher.update(b"=");
                    hasher.update(value.as_bytes());
                    hasher.update(b";");
                }
            }
            hasher.update([0]);
            if let Some(strength) = choice.strength {
                hasher.update(format!("{strength}").as_bytes());
            }
            hasher.update([0]);
            hasher.update(text.as_bytes());
            hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct CacheVersion {
    engine_version: String,
    synthesis_constants_version: u32,
}

struct MemoryEntry {
    wav: Vec<u8>,
    used: u64,
}

struct DiskEntry {
    bytes: u64,
    /// 古い順に消すための順序（更新時刻、同時刻は書いた順）。
    order: (SystemTime, u64),
}

#[derive(Default)]
struct State {
    /// 確かめたエンジンの版。None の間は読みも書きもしない。
    verified: Option<String>,
    memory: HashMap<String, MemoryEntry>,
    memory_bytes: u64,
    disk: HashMap<String, DiskEntry>,
    disk_bytes: u64,
    tick: u64,
}

pub(crate) struct TtsCache {
    dir: PathBuf,
    memory_limit: u64,
    disk_limit: u64,
    state: Mutex<State>,
}

impl TtsCache {
    /// `dir` を走査して索引を作る。エンジンの版を [`Self::verify`] で確かめるまでは使わない。
    pub(crate) fn open(dir: PathBuf, memory_limit: u64, disk_limit: u64) -> Self {
        let mut state = State::default();
        if let Err(error) = scan(&dir, &mut state) {
            tracing::warn!(dir = %dir.display(), error = %format!("{error:#}"), "TTS cache scan failed");
        }
        Self {
            dir,
            memory_limit,
            disk_limit,
            state: Mutex::new(state),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.state.lock().unwrap().verified.is_some()
    }

    /// エンジンの版を確かめて使えるようにする。記録と版か合成の固定値が違えば全部捨てる。
    pub(crate) fn verify(&self, engine_version: &str) -> anyhow::Result<()> {
        let current = CacheVersion {
            engine_version: engine_version.to_string(),
            synthesis_constants_version: SYNTHESIS_CONSTANTS_VERSION,
        };
        let mut state = self.state.lock().unwrap();
        if state.verified.as_deref() == Some(engine_version) {
            return Ok(());
        }
        state.verified = None;
        let recorded = std::fs::read(self.dir.join(VERSION_FILE))
            .ok()
            .and_then(|raw| serde_json::from_slice::<CacheVersion>(&raw).ok());
        if recorded.as_ref() != Some(&current) {
            let keys: Vec<String> = state.disk.keys().cloned().collect();
            for key in keys {
                remove_disk(&self.dir, &mut state, &key);
            }
            state.memory.clear();
            state.memory_bytes = 0;
            write_atomic(
                &self.dir,
                VERSION_FILE,
                &serde_json::to_vec(&current).context("cache version JSON")?,
            )?;
            tracing::info!(engine_version, "TTS cache reset for this engine version");
        }
        state.verified = Some(engine_version.to_string());
        Ok(())
    }

    /// エンジンの版が読めないときなど。確かめ直すまで読みも書きもしない。
    pub(crate) fn disable(&self) {
        self.state.lock().unwrap().verified = None;
    }

    pub(crate) fn get(&self, key: &str) -> Option<Vec<u8>> {
        {
            let mut state = self.state.lock().unwrap();
            state.verified.as_ref()?;
            state.tick += 1;
            let tick = state.tick;
            if let Some(entry) = state.memory.get_mut(key) {
                entry.used = tick;
                return Some(entry.wav.clone());
            }
            if !state.disk.contains_key(key) {
                return None;
            }
        }
        let path = self.dir.join(format!("{key}.wav"));
        let wav = match std::fs::read(&path) {
            Ok(wav) => wav,
            Err(error) => {
                tracing::warn!(error = %error, "TTS cache entry unreadable; dropped");
                let mut state = self.state.lock().unwrap();
                remove_disk(&self.dir, &mut state, key);
                return None;
            }
        };
        let mut state = self.state.lock().unwrap();
        state.verified.as_ref()?;
        self.remember(&mut state, key, wav.clone());
        Some(wav)
    }

    pub(crate) fn insert(&self, key: &str, wav: &[u8]) {
        let mut state = self.state.lock().unwrap();
        if state.verified.is_none() {
            return;
        }
        match write_atomic(&self.dir, &format!("{key}.wav"), wav) {
            Ok(()) => {
                state.tick += 1;
                let order = (SystemTime::now(), state.tick);
                let bytes = wav.len() as u64;
                if let Some(old) = state
                    .disk
                    .insert(key.to_string(), DiskEntry { bytes, order })
                {
                    state.disk_bytes -= old.bytes;
                }
                state.disk_bytes += bytes;
                while state.disk_bytes > self.disk_limit {
                    let Some(oldest) = state
                        .disk
                        .iter()
                        .min_by_key(|(_, entry)| entry.order)
                        .map(|(key, _)| key.clone())
                    else {
                        break;
                    };
                    remove_disk(&self.dir, &mut state, &oldest);
                }
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "TTS cache write failed");
            }
        }
        self.remember(&mut state, key, wav.to_vec());
    }

    fn remember(&self, state: &mut State, key: &str, wav: Vec<u8>) {
        state.tick += 1;
        let used = state.tick;
        let bytes = wav.len() as u64;
        if let Some(old) = state
            .memory
            .insert(key.to_string(), MemoryEntry { wav, used })
        {
            state.memory_bytes -= old.wav.len() as u64;
        }
        state.memory_bytes += bytes;
        while state.memory_bytes > self.memory_limit {
            let Some(oldest) = state
                .memory
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = state.memory.remove(&oldest) {
                state.memory_bytes -= entry.wav.len() as u64;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn usage(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap();
        (state.memory_bytes, state.disk_bytes)
    }
}

fn is_entry_name(name: &str) -> Option<&str> {
    let key = name.strip_suffix(".wav")?;
    (key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit())).then_some(key)
}

fn scan(dir: &Path, state: &mut State) -> anyhow::Result<()> {
    create_private_dir(dir)?;
    for entry in std::fs::read_dir(dir).context("read TTS cache dir")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with(TMP_PREFIX) {
            // 書きかけで終わった一時ファイル。
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        let Some(key) = is_entry_name(name) else {
            continue;
        };
        let metadata = entry.metadata()?;
        let bytes = metadata.len();
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        state.disk.insert(
            key.to_string(),
            DiskEntry {
                bytes,
                order: (modified, 0),
            },
        );
        state.disk_bytes += bytes;
    }
    Ok(())
}

fn remove_disk(dir: &Path, state: &mut State, key: &str) {
    if let Some(entry) = state.disk.remove(key) {
        state.disk_bytes -= entry.bytes;
        let _ = std::fs::remove_file(dir.join(format!("{key}.wav")));
    }
}

fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir).context("create TTS cache dir")
}

/// 同じディレクトリの一時ファイル（0600）へ書き、rename で置き換える。
fn write_atomic(dir: &Path, name: &str, body: &[u8]) -> anyhow::Result<()> {
    create_private_dir(dir)?;
    let tmp = dir.join(format!("{TMP_PREFIX}{}", uuid::Uuid::new_v4()));
    let result = (|| -> anyhow::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).context("create TTS cache temp file")?;
        file.write_all(body)?;
        std::fs::rename(&tmp, dir.join(name)).context("replace TTS cache file")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

//! 読み上げ辞書（D-1106）。共通の辞書と bot ごとの辞書。項目は zen-tts の辞書の形
//! （zen-tts README「辞書の形式」）で、ZEN-TTS にはそのまま `dict` として渡し（読みと抑揚）、
//! VOICEVOX では本文の言葉を読みに置き換える（抑揚の項目は使わない）。

use std::collections::{BTreeMap, HashSet};

use anyhow::bail;
use serde::{Deserialize, Serialize};

pub const MAX_SURFACE_CHARS: usize = 64;
pub const MAX_READING_CHARS: usize = 128;

/// 1 語（zen-tts の辞書の項目）。`reading` が空なら、VOICEVOX ではその言葉を読まない。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DictionaryEntry {
    pub surface: String,
    pub reading: String,
    /// アクセント型（何拍目の後で下がるか。0 = 平板）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<i64>,
    /// `split`（この語の前で句を切る）か `join`（前の句とつなげる）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength: Option<f64>,
    /// 拍ごとの相対的な高さ（-3〜3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contour: Option<Vec<f64>>,
    /// 拍ごとに足す高さ（半音）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<Vec<f64>>,
    /// 拍ごとの長さの倍率。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<Vec<f64>>,
}

/// `tts.dictionary`。
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsDictionary {
    /// どの bot にも効く辞書。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<DictionaryEntry>,
    /// bot（agent）ごとの辞書。同じ `surface` は共通の辞書より優先する。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, Vec<DictionaryEntry>>,
}

impl TtsDictionary {
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.agents.is_empty()
    }

    /// `agent_id` の bot が使う辞書（共通に bot の辞書を重ね、長い言葉から順）。
    pub fn for_agent(&self, agent_id: &str) -> Dictionary {
        let mut merged: BTreeMap<&str, &DictionaryEntry> = self
            .shared
            .iter()
            .map(|e| (e.surface.as_str(), e))
            .collect();
        for entry in self.agents.get(agent_id).into_iter().flatten() {
            merged.insert(&entry.surface, entry);
        }
        let mut entries: Vec<DictionaryEntry> = merged.into_values().cloned().collect();
        entries.sort_by(|a, b| {
            b.surface
                .chars()
                .count()
                .cmp(&a.surface.chars().count())
                .then_with(|| a.surface.cmp(&b.surface))
        });
        Dictionary(entries)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        check_entries("tts.dictionary.shared", &self.shared)?;
        for (agent, entries) in &self.agents {
            if agent.trim().is_empty() {
                bail!("tts.dictionary.agents has an empty agent id");
            }
            check_entries(&format!("tts.dictionary.agents.{agent}"), entries)?;
        }
        Ok(())
    }
}

fn check_entries(field: &str, entries: &[DictionaryEntry]) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for entry in entries {
        if entry.surface.is_empty() {
            bail!("{field} has an empty surface");
        }
        if entry.surface.chars().count() > MAX_SURFACE_CHARS {
            bail!("{field}: a surface is longer than {MAX_SURFACE_CHARS} chars");
        }
        if entry.reading.chars().count() > MAX_READING_CHARS {
            bail!("{field}: a reading is longer than {MAX_READING_CHARS} chars");
        }
        if !seen.insert(entry.surface.as_str()) {
            bail!("{field} has the surface {:?} twice", entry.surface);
        }
        if let Some(phrase) = &entry.phrase {
            if phrase != "split" && phrase != "join" {
                bail!("{field}: phrase must be split or join");
            }
        }
        let numbers = entry.strength.into_iter().chain(
            [&entry.contour, &entry.pitch, &entry.length]
                .into_iter()
                .flatten()
                .flatten()
                .copied(),
        );
        if numbers.into_iter().any(|n| !n.is_finite()) {
            bail!("{field}: intonation values must be numbers");
        }
        if entry
            .contour
            .iter()
            .flatten()
            .any(|n| !(-3.0..=3.0).contains(n))
        {
            bail!("{field}: contour values must be from -3 to 3");
        }
    }
    Ok(())
}

/// ある bot が使う辞書（長い言葉から順）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dictionary(Vec<DictionaryEntry>);

impl Dictionary {
    pub fn entries(&self) -> &[DictionaryEntry] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// VOICEVOX 用: 言葉を読みに置き換える。左から 1 回だけで、置き換えた読みはもう置き換えない。
    pub fn replace_readings(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'outer: while let Some(c) = rest.chars().next() {
            for entry in &self.0 {
                if let Some(after) = rest.strip_prefix(entry.surface.as_str()) {
                    out.push_str(&entry.reading);
                    rest = after;
                    continue 'outer;
                }
            }
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }
}

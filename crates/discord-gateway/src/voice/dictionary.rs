//! 読み上げ辞書（D-1106）。共通の辞書と bot ごとの辞書で、合成の直前に本文の言葉を読みに置き換える。
//! エンジン（VOICEVOX / ZEN-TTS）に依らない。

use std::collections::{BTreeMap, HashSet};

use anyhow::bail;
use serde::{Deserialize, Serialize};

pub const MAX_WORD_CHARS: usize = 64;
pub const MAX_READING_CHARS: usize = 128;

/// 1 語。`reading` が空なら、その言葉は読まない。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DictionaryEntry {
    pub word: String,
    pub reading: String,
}

/// `tts.dictionary`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TtsDictionary {
    /// どの bot にも効く辞書。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shared: Vec<DictionaryEntry>,
    /// bot（agent）ごとの辞書。同じ言葉は共通の辞書より優先する。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, Vec<DictionaryEntry>>,
}

impl TtsDictionary {
    pub fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.agents.is_empty()
    }

    /// `agent_id` の bot が使う置き換え（長い言葉から順）。
    pub fn for_agent(&self, agent_id: &str) -> Readings {
        let mut merged: BTreeMap<&str, &str> = self
            .shared
            .iter()
            .map(|e| (e.word.as_str(), e.reading.as_str()))
            .collect();
        for entry in self.agents.get(agent_id).into_iter().flatten() {
            merged.insert(&entry.word, &entry.reading);
        }
        let mut entries: Vec<(String, String)> = merged
            .into_iter()
            .map(|(w, r)| (w.to_string(), r.to_string()))
            .collect();
        entries.sort_by(|a, b| {
            b.0.chars()
                .count()
                .cmp(&a.0.chars().count())
                .then_with(|| a.0.cmp(&b.0))
        });
        Readings(entries)
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
        if entry.word.is_empty() {
            bail!("{field} has an empty word");
        }
        if entry.word.chars().count() > MAX_WORD_CHARS {
            bail!("{field}: a word is longer than {MAX_WORD_CHARS} chars");
        }
        if entry.reading.chars().count() > MAX_READING_CHARS {
            bail!("{field}: a reading is longer than {MAX_READING_CHARS} chars");
        }
        if !seen.insert(entry.word.as_str()) {
            bail!("{field} has the word {:?} twice", entry.word);
        }
    }
    Ok(())
}

/// ある bot の置き換え（長い言葉から順）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readings(Vec<(String, String)>);

impl Readings {
    /// 左から 1 回だけ置き換える。置き換えた読みは、もう一度は置き換えない。
    pub fn apply(&self, text: &str) -> String {
        if self.0.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'outer: while let Some(c) = rest.chars().next() {
            for (word, reading) in &self.0 {
                if let Some(after) = rest.strip_prefix(word.as_str()) {
                    out.push_str(reading);
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

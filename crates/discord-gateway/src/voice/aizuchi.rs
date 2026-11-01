//! 相づち（D-1085 P4、§9）。発話が確定したら、返事の前に TTS キャッシュ済みの短い相づちを流す。
//!
//! 相づちは確定した STT 本文の語尾で候補の組を選び、同じチャンネルで直前に流した相づちは避ける。
//! 流す相づちは said の `agent_prelude` で core へ送り、会話履歴にエージェント自身の発言として
//! 残す。LLM はそれを直前の自分の発言として見るので、返事が同じ言葉から始まらない。
//!
//! D-1101: 相づちの言葉と on/off は bot ごとの設定（instance config の `aizuchi`）。設定が無ければ
//! 下の既定の組を使う。
//!
//! D-1107: 相づちの 1 項目は、言葉と任意の言い方（ZEN-TTS のイントネーション）。同じ言葉でも言い方が
//! 違えば別の項目で、選ぶ・先読みする・キャッシュするのも項目ごと。言い方はその相づちを合成するとき
//! だけ、bot の辞書に 1 語の辞書項目として重ねる（普段の読み上げには効かない）。言い方の無い項目は
//! 今までどおり文字列で保存する。

use serde::{Deserialize, Serialize};

use super::dictionary::{Dictionary, DictionaryEntry};
use super::settings::{TtsEngine, VoiceChoice};

/// 問いかけ（か・の・？で終わる）への既定の相づち。
pub const DEFAULT_QUESTION: &[&str] = &["えっと、", "うーん、"];
/// 問いかけ以外の発話への既定の相づち。
pub const DEFAULT_STATEMENT: &[&str] = &["うん、", "へえ、", "なるほど、", "そっか、", "ああ、"];

/// 相づち 1 語の最大文字数。
pub const MAX_WORD_CHARS: usize = 20;
/// 1 つの組に置ける相づちの最大数。
pub const MAX_WORDS: usize = 20;

/// 相づち以外に、起動時・設定変更時・join 時に先に合成してキャッシュへ入れる本文。
const COMMON_PHRASES: &[&str] = &[
    "うん。",
    "そうだね。",
    "なるほど。",
    "ありがとう。",
    "こんにちは。",
];

/// 言い方の拍ごとの高さ（半音）の範囲。設定画面は 1 半音ずつ ±6 まで。
pub const MAX_PITCH_SEMITONES: f64 = 6.0;
/// 言い方の拍ごとの長さの倍率の範囲。
pub const LENGTH_RANGE: std::ops::RangeInclusive<f64> = 0.5..=2.0;

/// 相づちの 1 項目（D-1107）。言い方が無ければ文字列、あればオブジェクトで保存する。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "WordRepr", into = "WordRepr")]
pub struct AizuchiWord {
    pub text: String,
    pub intonation: Option<AizuchiIntonation>,
}

/// 相づちの言い方（zen-tts の辞書項目の `reading` / `accent` / `pitch` / `length`）。
#[derive(Debug, Clone, PartialEq)]
pub struct AizuchiIntonation {
    /// 言葉（末尾の「、」「。」を除いたもの）の読み。
    pub reading: String,
    /// 何拍目の後で下がるか（0 = 平板）。None は ZEN-TTS に任せる。
    pub accent: Option<i64>,
    /// 拍ごとに足す高さ（半音）。
    pub pitch: Option<Vec<f64>>,
    /// 拍ごとの長さの倍率。
    pub length: Option<Vec<f64>>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum WordRepr {
    Plain(String),
    Toned(TonedWord),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TonedWord {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reading: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    accent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pitch: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    length: Option<Vec<f64>>,
}

impl TryFrom<WordRepr> for AizuchiWord {
    type Error = String;

    fn try_from(repr: WordRepr) -> Result<Self, String> {
        let toned = match repr {
            WordRepr::Plain(text) => return Ok(Self::plain(&text)),
            WordRepr::Toned(toned) => toned,
        };
        if toned.reading.is_none()
            && toned.accent.is_none()
            && toned.pitch.is_none()
            && toned.length.is_none()
        {
            return Ok(Self::plain(&toned.text));
        }
        let reading = toned
            .reading
            .filter(|r| !r.trim().is_empty())
            .ok_or("an aizuchi word with an intonation needs its reading")?;
        Ok(Self {
            text: toned.text,
            intonation: Some(AizuchiIntonation {
                reading,
                accent: toned.accent,
                pitch: toned.pitch,
                length: toned.length,
            }),
        })
    }
}

impl From<AizuchiWord> for WordRepr {
    fn from(word: AizuchiWord) -> Self {
        match word.intonation {
            None => WordRepr::Plain(word.text),
            Some(i) => WordRepr::Toned(TonedWord {
                text: word.text,
                reading: Some(i.reading),
                accent: i.accent,
                pitch: i.pitch,
                length: i.length,
            }),
        }
    }
}

impl PartialEq<str> for AizuchiWord {
    /// 言い方の無い、この言葉の項目か。
    fn eq(&self, other: &str) -> bool {
        self.intonation.is_none() && self.text == other
    }
}

impl PartialEq<&str> for AizuchiWord {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl From<&str> for AizuchiWord {
    fn from(text: &str) -> Self {
        Self::plain(text)
    }
}

impl From<&AizuchiWord> for AizuchiWord {
    fn from(word: &AizuchiWord) -> Self {
        word.clone()
    }
}

impl AizuchiWord {
    /// 言い方の無い項目。
    pub fn plain(text: &str) -> Self {
        Self {
            text: text.to_string(),
            intonation: None,
        }
    }

    /// 言い方を当てる言葉（前後の空白と末尾の「、」「。」などを除く）。
    pub fn surface(&self) -> &str {
        self.text
            .trim()
            .trim_end_matches(|c: char| matches!(c, '、' | '。' | ',' | '.' | '…'))
    }

    /// 言い方を 1 語の辞書項目にしたもの（言い方が無ければ None）。
    pub fn dictionary_entry(&self) -> Option<DictionaryEntry> {
        let i = self.intonation.as_ref()?;
        Some(DictionaryEntry {
            surface: self.surface().to_string(),
            reading: i.reading.clone(),
            accent: i.accent,
            phrase: None,
            strength: None,
            contour: None,
            pitch: i.pitch.clone(),
            length: i.length.clone(),
        })
    }

    /// この相づちを `base`（bot の声と辞書）で読むときの声。ZEN-TTS で言い方があれば、その 1 語を
    /// 辞書に重ねる（同じ言葉は言い方が勝つ）。VOICEVOX では言い方を使わない。
    pub fn voice_choice(&self, base: &VoiceChoice) -> VoiceChoice {
        match (base.engine, self.dictionary_entry()) {
            (TtsEngine::ZenTts, Some(entry)) => {
                let dictionary = base
                    .dictionary
                    .with_overrides(&[entry])
                    .unwrap_or_else(|_| base.dictionary.clone());
                base.clone().with_dictionary(dictionary)
            }
            _ => base.clone(),
        }
    }

    fn validate_intonation(&self, label: &str) -> anyhow::Result<()> {
        let Some(entry) = self.dictionary_entry() else {
            return Ok(());
        };
        if entry.surface.is_empty() {
            anyhow::bail!("aizuchi.{label}: a word with an intonation needs letters");
        }
        Dictionary::default()
            .with_overrides(std::slice::from_ref(&entry))
            .map_err(|e| anyhow::anyhow!("aizuchi.{label}: {e:#}"))?;
        if entry.accent.is_some_and(|a| a < 0) {
            anyhow::bail!("aizuchi.{label}: accent must not be negative");
        }
        if entry
            .pitch
            .iter()
            .flatten()
            .any(|p| !(-MAX_PITCH_SEMITONES..=MAX_PITCH_SEMITONES).contains(p))
        {
            anyhow::bail!("aizuchi.{label}: pitch must be within ±{MAX_PITCH_SEMITONES} semitones");
        }
        if entry
            .length
            .iter()
            .flatten()
            .any(|l| !LENGTH_RANGE.contains(l))
        {
            anyhow::bail!("aizuchi.{label}: length must be from 0.5 to 2");
        }
        Ok(())
    }
}

/// bot ごとの相づち設定（instance config の `aizuchi`）。項目が無ければ既定値。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AizuchiConfig {
    /// false なら相づちを流さない。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 問いかけへの相づち。空なら問いかけには流さない。
    #[serde(default = "default_question")]
    pub question: Vec<AizuchiWord>,
    /// 問いかけ以外への相づち。空なら問いかけ以外には流さない。
    #[serde(default = "default_statement")]
    pub statement: Vec<AizuchiWord>,
}

fn default_enabled() -> bool {
    true
}
fn default_question() -> Vec<AizuchiWord> {
    DEFAULT_QUESTION
        .iter()
        .map(|s| AizuchiWord::plain(s))
        .collect()
}
fn default_statement() -> Vec<AizuchiWord> {
    DEFAULT_STATEMENT
        .iter()
        .map(|s| AizuchiWord::plain(s))
        .collect()
}

impl Default for AizuchiConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            question: default_question(),
            statement: default_statement(),
        }
    }
}

impl AizuchiConfig {
    /// 各語は空でなく（前後の空白を除いて）、`MAX_WORD_CHARS` 文字以下、制御文字なし。言い方は
    /// 辞書項目として正しく、高さ・長さが範囲内。各組は `MAX_WORDS` 語以下（空は可）。同じ言葉が
    /// 言い方を変えて何度出てもよい。
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, words) in [("question", &self.question), ("statement", &self.statement)] {
            if words.len() > MAX_WORDS {
                anyhow::bail!("aizuchi.{label} must have at most {MAX_WORDS} words");
            }
            for item in words {
                let word = &item.text;
                if word.trim().is_empty() {
                    anyhow::bail!("aizuchi.{label} words must be nonempty");
                }
                if word.chars().count() > MAX_WORD_CHARS {
                    anyhow::bail!("aizuchi.{label} words must be at most {MAX_WORD_CHARS} chars");
                }
                if word.chars().any(char::is_control) {
                    anyhow::bail!("aizuchi.{label} words must not contain control characters");
                }
                item.validate_intonation(label)?;
            }
        }
        Ok(())
    }

    /// 先に合成してキャッシュへ入れる項目: 有効ならこの bot の相づち全部（言い方ごと）と、
    /// よく使う短い返事。
    pub fn prewarm_phrases(&self) -> Vec<AizuchiWord> {
        let mut phrases: Vec<AizuchiWord> = Vec::new();
        let words = self
            .enabled
            .then(|| self.question.iter().chain(self.statement.iter()))
            .into_iter()
            .flatten()
            .cloned();
        for phrase in words.chain(COMMON_PHRASES.iter().map(|s| AizuchiWord::plain(s))) {
            if !phrases.contains(&phrase) {
                phrases.push(phrase);
            }
        }
        phrases
    }
}

/// 確定した発話の本文から相づちを選ぶ。無効のとき、2 文字未満の発話、その種類の組が空のときは
/// 打たない。`previous` は同じチャンネルで直前に流した相づち（言い方ごとの項目）で、それは選ばない。
/// `pick` は候補の中から一つを選ぶための任意の数（呼び出し側は時刻などから作る）。
pub(crate) fn choose_aizuchi(
    config: &AizuchiConfig,
    text: &str,
    previous: Option<&AizuchiWord>,
    pick: usize,
) -> Option<AizuchiWord> {
    if !config.enabled {
        return None;
    }
    let trimmed = text.trim().trim_end_matches(|c: char| {
        matches!(c, '。' | '、' | '.' | ',' | '…') || c.is_whitespace()
    });
    if trimmed.chars().count() < 2 {
        return None;
    }
    let set = match trimmed.chars().last() {
        Some('か' | 'の' | '？' | '?') => &config.question,
        _ => &config.statement,
    };
    if set.is_empty() {
        return None;
    }
    let mut candidates: Vec<&AizuchiWord> = set
        .iter()
        .filter(|candidate| Some(*candidate) != previous)
        .collect();
    // 組が 1 語だけなら、直前と同じでもその語を使う（避ける相手が無い）。
    if candidates.is_empty() {
        candidates = set.iter().collect();
    }
    Some(candidates[pick % candidates.len()].clone())
}

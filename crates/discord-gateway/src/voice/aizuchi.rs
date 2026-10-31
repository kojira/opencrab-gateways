//! 相づち（D-1085 P4、§9）。発話が確定したら、返事の前に TTS キャッシュ済みの短い相づちを流す。
//!
//! 相づちは確定した STT 本文の語尾で候補の組を選び、同じチャンネルで直前に流した相づちは避ける。
//! 流す相づちは said の `agent_prelude` で core へ送り、会話履歴にエージェント自身の発言として
//! 残す。LLM はそれを直前の自分の発言として見るので、返事が同じ言葉から始まらない。
//!
//! D-1101: 相づちの言葉と on/off は bot ごとの設定（instance config の `aizuchi`）。設定が無ければ
//! 下の既定の組を使う。

use serde::{Deserialize, Serialize};

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

/// bot ごとの相づち設定（instance config の `aizuchi`）。項目が無ければ既定値。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AizuchiConfig {
    /// false なら相づちを流さない。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 問いかけへの相づち。空なら問いかけには流さない。
    #[serde(default = "default_question")]
    pub question: Vec<String>,
    /// 問いかけ以外への相づち。空なら問いかけ以外には流さない。
    #[serde(default = "default_statement")]
    pub statement: Vec<String>,
}

fn default_enabled() -> bool {
    true
}
fn default_question() -> Vec<String> {
    DEFAULT_QUESTION.iter().map(|s| s.to_string()).collect()
}
fn default_statement() -> Vec<String> {
    DEFAULT_STATEMENT.iter().map(|s| s.to_string()).collect()
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
    /// 各語は空でなく（前後の空白を除いて）、`MAX_WORD_CHARS` 文字以下、制御文字なし。
    /// 各組は `MAX_WORDS` 語以下（空は可）。
    pub fn validate(&self) -> anyhow::Result<()> {
        for (label, words) in [("question", &self.question), ("statement", &self.statement)] {
            if words.len() > MAX_WORDS {
                anyhow::bail!("aizuchi.{label} must have at most {MAX_WORDS} words");
            }
            for word in words {
                if word.trim().is_empty() {
                    anyhow::bail!("aizuchi.{label} words must be nonempty");
                }
                if word.chars().count() > MAX_WORD_CHARS {
                    anyhow::bail!("aizuchi.{label} words must be at most {MAX_WORD_CHARS} chars");
                }
                if word.chars().any(char::is_control) {
                    anyhow::bail!("aizuchi.{label} words must not contain control characters");
                }
            }
        }
        Ok(())
    }

    /// 先に合成してキャッシュへ入れる本文: 有効ならこの bot の相づち全部と、よく使う短い返事。
    pub fn prewarm_phrases(&self) -> Vec<String> {
        let mut phrases: Vec<String> = Vec::new();
        let words = self
            .enabled
            .then(|| self.question.iter().chain(self.statement.iter()))
            .into_iter()
            .flatten()
            .cloned();
        for phrase in words.chain(COMMON_PHRASES.iter().map(|s| s.to_string())) {
            if !phrases.contains(&phrase) {
                phrases.push(phrase);
            }
        }
        phrases
    }
}

/// 確定した発話の本文から相づちを選ぶ。無効のとき、2 文字未満の発話、その種類の組が空のときは
/// 打たない。`previous` は同じチャンネルで直前に流した相づちで、それは選ばない。`pick` は候補の中から
/// 一つを選ぶための任意の数（呼び出し側は時刻などから作る）。
pub(crate) fn choose_aizuchi(
    config: &AizuchiConfig,
    text: &str,
    previous: Option<&str>,
    pick: usize,
) -> Option<String> {
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
    let mut candidates: Vec<&String> = set
        .iter()
        .filter(|candidate| Some(candidate.as_str()) != previous)
        .collect();
    // 組が 1 語だけなら、直前と同じでもその語を使う（避ける相手が無い）。
    if candidates.is_empty() {
        candidates = set.iter().collect();
    }
    Some(candidates[pick % candidates.len()].clone())
}

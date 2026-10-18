//! 相づち（D-1085 P4）。発話が確定したら、返事の前に TTS キャッシュ済みの短い相づちを流す。
//!
//! 相づちは確定した STT 本文の語尾で選ぶ（問いかけなら「えっと、」、それ以外は「うん、」）。
//! LLM には said の system_context で「もう言った」と伝え、返事が相づちから始まらないようにする。

/// 問いかけ以外の発話への相づち。
pub(crate) const AIZUCHI_DEFAULT: &str = "うん、";
/// 問いかけ（か・の・？で終わる）への相づち。
pub(crate) const AIZUCHI_QUESTION: &str = "えっと、";

/// 起動時・設定変更時・join 時に、設定にある全スタイルで先に合成してキャッシュへ入れる本文。
pub(crate) const PREWARM_PHRASES: &[&str] = &[
    AIZUCHI_DEFAULT,
    AIZUCHI_QUESTION,
    "うん。",
    "そうだね。",
    "なるほど。",
    "ありがとう。",
    "こんにちは。",
];

/// 確定した発話の本文から相づちを選ぶ。2 文字未満の発話には相づちを打たない。
pub(crate) fn choose_aizuchi(text: &str) -> Option<&'static str> {
    let trimmed = text.trim().trim_end_matches(|c: char| {
        matches!(c, '。' | '、' | '.' | ',' | '…') || c.is_whitespace()
    });
    if trimmed.chars().count() < 2 {
        return None;
    }
    match trimmed.chars().last() {
        Some('か' | 'の' | '？' | '?') => Some(AIZUCHI_QUESTION),
        _ => Some(AIZUCHI_DEFAULT),
    }
}

/// 相づちを選んだ発話の system_context に足す 1 行。
pub(crate) fn aizuchi_context_line(aizuchi: &str) -> String {
    format!("この返事の直前に、あなたはすでに『{aizuchi}』と言った。それで始めず続きとして話す。")
}

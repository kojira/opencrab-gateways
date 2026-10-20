//! 相づち（D-1085 P4、§9）。発話が確定したら、返事の前に TTS キャッシュ済みの短い相づちを流す。
//!
//! 相づちは確定した STT 本文の語尾で候補の組を選び、同じチャンネルで直前に流した相づちは避ける。
//! 流す相づちは said の `agent_prelude` で core へ送り、会話履歴にエージェント自身の発言として
//! 残す。LLM はそれを直前の自分の発言として見るので、返事が同じ言葉から始まらない。

/// 問いかけ（か・の・？で終わる）への相づち。
pub(crate) const AIZUCHI_QUESTION: &[&str] = &["えっと、", "うーん、"];
/// 問いかけ以外の発話への相づち。
pub(crate) const AIZUCHI_STATEMENT: &[&str] =
    &["うん、", "へえ、", "なるほど、", "そっか、", "ああ、"];

/// 相づち以外に、起動時・設定変更時・join 時に先に合成してキャッシュへ入れる本文。
const COMMON_PHRASES: &[&str] = &[
    "うん。",
    "そうだね。",
    "なるほど。",
    "ありがとう。",
    "こんにちは。",
];

/// 起動時・設定変更時・join 時に、設定にある全スタイルで先に合成してキャッシュへ入れる本文
/// （相づちの全候補と、よく使う短い返事）。
pub(crate) const PREWARM_PHRASES: &[&str] = &[
    AIZUCHI_QUESTION[0],
    AIZUCHI_QUESTION[1],
    AIZUCHI_STATEMENT[0],
    AIZUCHI_STATEMENT[1],
    AIZUCHI_STATEMENT[2],
    AIZUCHI_STATEMENT[3],
    AIZUCHI_STATEMENT[4],
    COMMON_PHRASES[0],
    COMMON_PHRASES[1],
    COMMON_PHRASES[2],
    COMMON_PHRASES[3],
    COMMON_PHRASES[4],
];

/// 確定した発話の本文から相づちを選ぶ。2 文字未満の発話には相づちを打たない。
/// `previous` は同じチャンネルで直前に流した相づちで、それは選ばない。`pick` は候補の中から
/// 一つを選ぶための任意の数（呼び出し側は時刻などから作る）。
pub(crate) fn choose_aizuchi(
    text: &str,
    previous: Option<&str>,
    pick: usize,
) -> Option<&'static str> {
    let trimmed = text.trim().trim_end_matches(|c: char| {
        matches!(c, '。' | '、' | '.' | ',' | '…') || c.is_whitespace()
    });
    if trimmed.chars().count() < 2 {
        return None;
    }
    let set = match trimmed.chars().last() {
        Some('か' | 'の' | '？' | '?') => AIZUCHI_QUESTION,
        _ => AIZUCHI_STATEMENT,
    };
    let candidates: Vec<&'static str> = set
        .iter()
        .copied()
        .filter(|candidate| Some(*candidate) != previous)
        .collect();
    candidates.get(pick % candidates.len()).copied()
}

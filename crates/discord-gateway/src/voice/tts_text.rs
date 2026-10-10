//! 投稿本文を読み上げ向けに整える（旧 crates/discord voice_session.rs の clean_for_tts を移植）。
//! コードブロック・URL・メンション・Markdown 記号は読み上げに向かないので置換・除去する。

/// 1 回の読み上げの上限文字数。超えた分は「以下省略」にする。
pub const MAX_TTS_CHARS: usize = 300;

pub fn clean_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if !in_code_block {
                out.push_str("（コードは省略） ");
            }
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        out.push_str(line);
        out.push(' ');
    }
    let mut cleaned = String::with_capacity(out.len());
    for word in out.split(' ') {
        if word.starts_with("http://") || word.starts_with("https://") {
            cleaned.push_str("リンク");
        } else {
            cleaned.push_str(word);
        }
        cleaned.push(' ');
    }
    let mut result = String::with_capacity(cleaned.len());
    let mut chars = cleaned.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            // <@123> <#123> <@&123> <:name:id> を飛ばす。閉じなければ元の文字を残す。
            let mut consumed = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '>' {
                    closed = true;
                    break;
                }
                consumed.push(c2);
                if consumed.len() > 64 {
                    break;
                }
            }
            if !closed {
                result.push('<');
                result.push_str(&consumed);
            }
            continue;
        }
        if matches!(c, '*' | '_' | '`' | '#' | '~' | '|') {
            continue;
        }
        result.push(c);
    }
    let trimmed = result.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > MAX_TTS_CHARS {
        let cut: String = trimmed.chars().take(MAX_TTS_CHARS).collect();
        format!("{cut}、以下省略")
    } else {
        trimmed
    }
}

/// 読み上げ本文を文単位に分ける。「。！？!?」と改行で区切り、終止記号は直前の文に付ける
/// （「！？」のような連続も 1 つの文末として残す）。空白だけの区間は捨てる。
/// 文ごとに合成・再生すると、長文でも最初の文の再生を全文の合成完了まで待たずに始められる。
pub fn split_sentences(text: &str) -> Vec<String> {
    fn is_terminator(c: char) -> bool {
        matches!(c, '。' | '！' | '？' | '!' | '?')
    }
    let mut sentences = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut flush = |current: &mut String| {
        let trimmed = current.trim();
        if !trimmed.is_empty() {
            sentences.push(trimmed.to_string());
        }
        current.clear();
    };
    while let Some(c) = chars.next() {
        if c == '\n' || c == '\r' {
            flush(&mut current);
            continue;
        }
        current.push(c);
        if is_terminator(c) && !chars.peek().copied().is_some_and(is_terminator) {
            flush(&mut current);
        }
    }
    flush(&mut current);
    sentences
}

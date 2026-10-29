//! 投稿本文を読み上げ向けに整える（旧 crates/discord voice_session.rs の clean_for_tts を移植）。
//! コードブロック・URL・メンション・Markdown 記号は読み上げに向かないので置換・除去する。

/// 読み上げ向けに整える。長さの上限は掛けない（長い返事も最後まで読む。止めたいときは話しかけて
/// 割り込む、D-1096）。
pub fn clean_for_tts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if !in_code_block {
                out.push_str("（コードは省略）\n");
            }
            in_code_block = !in_code_block;
            continue;
        }
        if in_code_block {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    let mut cleaned = String::with_capacity(out.len());
    for line in out.lines() {
        cleaned.push_str(&replace_urls(line));
        cleaned.push('\n');
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
    // 改行は文の区切りとして残す（split_sentences が行ごとに分ける）。行内の空白は 1 つにまとめ、空行は捨てる。
    result
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `http://` / `https://` で始まる URL を「リンク」に置き換える。日本語の文では URL が前後の文字に
/// 空白なしで続くので、語頭だけでなく文中のどこでも探す。URL は空白か、URL に使えない文字
/// （ASCII 以外の文字・全角記号）の直前で終わる。末尾の `)` `]` `>` `"` `'` は文の記号として残す。
fn replace_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = find_scheme(rest) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let mut end = tail
            .char_indices()
            .find(|&(_, c)| c.is_whitespace() || !c.is_ascii())
            .map_or(tail.len(), |(i, _)| i);
        while end > 0 && matches!(tail.as_bytes()[end - 1], b')' | b']' | b'>' | b'"' | b'\'') {
            end -= 1;
        }
        out.push_str("リンク");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn find_scheme(text: &str) -> Option<usize> {
    match (text.find("http://"), text.find("https://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
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

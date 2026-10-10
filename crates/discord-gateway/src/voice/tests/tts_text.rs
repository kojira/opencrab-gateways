use super::super::tts_text::{clean_for_tts, split_sentences, MAX_TTS_CHARS};

#[test]
fn strips_code_blocks_and_replaces_urls() {
    let input = "結果です。\n```rust\nfn main() {}\n```\n詳細は https://example.com/x を見てください **重要**";
    let out = clean_for_tts(input);
    assert!(!out.contains("fn main"), "{out}");
    assert!(out.contains("（コードは省略）"), "{out}");
    assert!(!out.contains("https://"), "{out}");
    assert!(out.contains("リンク"), "{out}");
    assert!(!out.contains("**"), "{out}");
}

#[test]
fn strips_mentions_and_custom_emoji() {
    let out = clean_for_tts("<@123456> さん、<#987> を見て <:smile:42>");
    assert_eq!(out, "さん、 を見て");
}

#[test]
fn caps_length() {
    let long = "あ".repeat(MAX_TTS_CHARS + 50);
    let out = clean_for_tts(&long);
    assert!(out.ends_with("、以下省略"));
    assert_eq!(
        out.chars().count(),
        MAX_TTS_CHARS + "、以下省略".chars().count()
    );
}

#[test]
fn plain_text_passes_through() {
    assert_eq!(
        clean_for_tts("こんにちは。元気です。"),
        "こんにちは。元気です。"
    );
}

#[test]
fn only_code_or_markup_becomes_empty_or_placeholder() {
    assert_eq!(clean_for_tts("**__~~||"), "");
}

#[test]
fn split_sentences_keeps_each_terminator_with_its_sentence() {
    assert_eq!(
        split_sentences("こんにちは。元気です！本当?はい!"),
        vec!["こんにちは。", "元気です！", "本当?", "はい!"]
    );
}

#[test]
fn split_sentences_keeps_consecutive_terminators_together() {
    assert_eq!(
        split_sentences("えっ！？まさか。。そう?!"),
        vec!["えっ！？", "まさか。。", "そう?!"]
    );
}

#[test]
fn split_sentences_splits_at_newlines_and_trims() {
    assert_eq!(
        split_sentences("一行目\n二行目。 三行目\n\n"),
        vec!["一行目", "二行目。", "三行目"]
    );
}

#[test]
fn split_sentences_keeps_trailing_text_without_terminator() {
    assert_eq!(
        split_sentences("終わった。まだ続く"),
        vec!["終わった。", "まだ続く"]
    );
}

#[test]
fn split_sentences_drops_empty_and_whitespace_only_segments() {
    assert!(split_sentences("").is_empty());
    assert_eq!(split_sentences("  \n 。"), vec!["。"]);
    assert!(split_sentences(" \n \t ").is_empty());
}

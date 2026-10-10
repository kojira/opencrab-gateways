use super::super::tts_text::{clean_for_tts, MAX_TTS_CHARS};

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

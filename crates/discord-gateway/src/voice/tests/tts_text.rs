use super::super::tts_text::{clean_for_tts, split_sentences};

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
fn replaces_url_glued_to_preceding_text() {
    let out = clean_for_tts("公式第1話：https://comic-days.com/episode/123 です");
    assert!(!out.contains("http"), "{out}");
    assert_eq!(out, "公式第1話：リンク です");
}

#[test]
fn replaces_url_after_sentence_end_without_space() {
    assert_eq!(
        clean_for_tts("載ってた。https://jules.google/docs/changelog/ ただ"),
        "載ってた。リンク ただ"
    );
}

#[test]
fn url_ends_at_full_width_punctuation() {
    assert_eq!(clean_for_tts("（https://a.example/x）"), "（リンク）");
    assert_eq!(
        clean_for_tts("見て→https://a.example/x、続き"),
        "見て→リンク、続き"
    );
}

#[test]
fn url_ends_at_cjk_text() {
    assert_eq!(
        clean_for_tts("https://a.example/xを見てね"),
        "リンクを見てね"
    );
}

#[test]
fn url_drops_trailing_ascii_closers() {
    assert_eq!(
        clean_for_tts("詳しくは(https://a.example/x)と\"https://b.example/y\"へ"),
        "詳しくは(リンク)と\"リンク\"へ"
    );
}

#[test]
fn replaces_two_urls_on_one_line() {
    assert_eq!(
        clean_for_tts("Aは:https://a.example/1、Bはhttp://b.example/2です"),
        "Aは:リンク、Bはリンクです"
    );
}

#[test]
fn bare_domain_without_scheme_is_kept() {
    assert_eq!(clean_for_tts("agents.md を見て"), "agents.md を見て");
}

#[test]
fn strips_mentions_and_custom_emoji() {
    let out = clean_for_tts("<@123456> さん、<#987> を見て <:smile:42>");
    assert_eq!(out, "さん、 を見て");
}

#[test]
fn long_text_is_not_capped() {
    let long = "あ".repeat(1000);
    assert_eq!(clean_for_tts(&long), long);
}

#[test]
fn plain_text_passes_through() {
    assert_eq!(
        clean_for_tts("こんにちは。元気です。"),
        "こんにちは。元気です。"
    );
}

#[test]
fn keeps_line_breaks_and_collapses_whitespace_within_lines() {
    assert_eq!(clean_for_tts("a\n\n b  c\nd"), "a\nb c\nd");
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
fn split_sentences_does_not_split_at_commas() {
    assert_eq!(
        split_sentences("今日は、晴れです。"),
        vec!["今日は、晴れです。"]
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

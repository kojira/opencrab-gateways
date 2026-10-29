use super::super::tts_text::{clean_for_tts, split_sentences};

#[test]
fn strips_code_blocks_and_drops_urls() {
    let input = "結果です。\n```rust\nfn main() {}\n```\n詳細は https://example.com/x を見てください **重要**";
    let out = clean_for_tts(input);
    assert!(!out.contains("fn main"), "{out}");
    assert!(out.contains("（コードは省略）"), "{out}");
    assert!(!out.contains("http"), "{out}");
    assert!(!out.contains("リンク"), "{out}");
    assert!(!out.contains("**"), "{out}");
    assert_eq!(
        out,
        "結果です。\n（コードは省略）\n詳細は を見てください 重要"
    );
}

#[test]
fn drops_url_glued_to_preceding_text() {
    assert_eq!(
        clean_for_tts("公式第1話：https://comic-days.com/episode/123 です"),
        "公式第1話： です"
    );
}

#[test]
fn drops_url_after_sentence_end_without_space() {
    assert_eq!(
        clean_for_tts("載ってた。https://jules.google/docs/changelog/ ただ"),
        "載ってた。 ただ"
    );
}

#[test]
fn drops_url_and_its_now_empty_full_width_parentheses() {
    assert_eq!(
        clean_for_tts("詳しくは（https://a.example/x）を見て"),
        "詳しくはを見て"
    );
    assert_eq!(clean_for_tts("（https://a.example/x）"), "");
}

#[test]
fn drops_url_and_its_now_empty_ascii_parentheses() {
    assert_eq!(
        clean_for_tts("詳しくは(https://a.example/x)と"),
        "詳しくはと"
    );
}

#[test]
fn parentheses_that_held_more_than_the_url_are_kept() {
    assert_eq!(
        clean_for_tts("公式（無料：https://a.example/x）で"),
        "公式（無料：）で"
    );
}

#[test]
fn empty_parentheses_not_left_by_a_url_are_kept() {
    assert_eq!(clean_for_tts("顔文字（）です"), "顔文字（）です");
}

#[test]
fn url_ends_at_full_width_punctuation() {
    assert_eq!(
        clean_for_tts("見て→https://a.example/x、続き"),
        "見て→、続き"
    );
}

#[test]
fn url_ends_at_cjk_text() {
    assert_eq!(clean_for_tts("https://a.example/xを見てね"), "を見てね");
}

#[test]
fn url_keeps_trailing_ascii_quote() {
    assert_eq!(clean_for_tts("と\"https://b.example/y\"へ"), "と\"\"へ");
}

#[test]
fn drops_two_urls_on_one_line() {
    assert_eq!(
        clean_for_tts("Aは:https://a.example/1、Bはhttp://b.example/2です"),
        "Aは:、Bはです"
    );
}

#[test]
fn line_that_is_only_a_url_disappears() {
    assert_eq!(
        clean_for_tts("前の行\nhttps://a.example/x\n次の行"),
        "前の行\n次の行"
    );
}

#[test]
fn drops_space_separated_url() {
    assert_eq!(
        clean_for_tts("ここ https://a.example/x を見て"),
        "ここ を見て"
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

//! D-1097: an owner @mentioning the bot in a channel it does not have asks the daemon for that
//! channel; nobody else, and nothing without a mention, does.

use super::owner_mention::wants_channel;
use crate::config::AccessConfig;
use crate::map::parse_event_line;

const BOT: &str = "555555555555555555";
const OWNER: &str = "222222222222222222";

fn access() -> AccessConfig {
    AccessConfig {
        owners: vec![OWNER.into()],
        trusted_users: vec!["333333333333333333".into()],
        ..AccessConfig::default()
    }
}

fn message(author: &str, mentions: &[&str]) -> crate::map::IncomingMessage {
    let line = serde_json::json!({"id":"7","channel_id":"200","guild_id":"100",
        "author":{"id":author},"content":"hi","mentions":mentions});
    parse_event_line(&line.to_string()).unwrap()
}

#[test]
fn d1097_mentions_are_parsed_from_the_message_line() {
    assert_eq!(message(OWNER, &[BOT]).mentions, vec![BOT]);
    let old = parse_event_line(r#"{"id":"7","channel_id":"200","author":{"id":"1"}}"#).unwrap();
    assert!(
        old.mentions.is_empty(),
        "lines without the field still parse"
    );
}

#[test]
fn d1097_only_an_owner_mention_in_an_unknown_channel_asks() {
    let address = "discord-agent-100-200";
    assert!(wants_channel(
        &access(),
        BOT,
        &message(OWNER, &[BOT]),
        false
    ));
    assert!(
        !wants_channel(&access(), BOT, &message(OWNER, &[]), false),
        "no mention"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message(OWNER, &["999999999999999999"]),
            false
        ),
        "someone else mentioned"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message("333333333333333333", &[BOT]),
            false
        ),
        "a trusted user is not an owner"
    );
    assert!(
        !wants_channel(
            &access(),
            BOT,
            &message("444444444444444444", &[BOT]),
            false
        ),
        "a stranger"
    );
    assert!(
        !wants_channel(&access(), BOT, &message(OWNER, &[BOT]), true),
        "the channel is already bound"
    );
    let _ = address;
}

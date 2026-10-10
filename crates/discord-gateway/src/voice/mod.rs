//! Discord VC（D-1072）。音声は gateway の外へ出さず、core には文字（said）だけを渡す。
//!
//! - 受信: songbird の VoiceTick を話者ごとに区切り（[`receiver`]）、STT で文字にして said にする。
//! - 読み上げ: VC に結びついたテキストチャンネルへ投稿した本文を TTS にして VC で流す。
//! - 設定: [`settings`] の JSON ファイル（0600）。設定画面は [`settings_http`]。

pub mod audio;
pub mod clients;
pub mod receiver;
pub mod session;
pub mod settings;
pub mod settings_http;
pub mod songbird_player;
pub mod tts_text;

#[cfg(test)]
mod tests;

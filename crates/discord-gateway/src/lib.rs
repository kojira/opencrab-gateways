//! Discord gateway V3 profile（DESIGN-DISCORD-GATE.md v17 フェーズ1）。
//!
//! 1 bot credential = 1 V3 instance = 1 gateway process = exact 1 agent（設計 §0）。
//! gate wire（hello/bind/said/say/invoke）は [`opencrab_gate_client`] を Nostr gateway と同一に
//! 経由する。core への Discord 固有追加はゼロで、能力は hello の `GatewayOperationDeclaration`
//! として宣言する（reply/reaction/resolve）。会話の e/u/c 採番は core conversation.rs の汎用機構を
//! 再利用し、gateway は said に生 origin/author を載せるだけ（core に Discord 語彙を足さない）。
//!
//! 秘密（bot token）は gateway process env のみ（[`secret`]）。QC は fixture 注入 + dry-run の
//! オフライン E2E（[`harness`]）で実配線を回す（Nostr の偽watch 相当）。

pub mod admin;
pub mod admin_settings;
pub mod attachment;
pub mod channel_request;
pub mod config;
pub mod core_provisioning;
pub mod daemon;
pub mod harness;
pub mod legacy_import;
pub mod map;
pub mod model;
pub mod ops;
pub mod post;
pub mod receive;
mod reply_files;
pub mod run;
pub mod secret;
pub mod secret_store;
pub mod settings_http;
pub mod store;
pub mod subject_grant;
pub mod thinking;
pub mod transport;
pub mod typing;
pub mod voice;
// D-1087: 活動 webhook（宛先はセッション単位で gateway DB に持つ）。core は webhook を
// 知らない。供給口は `webhook::source` の 1 module に閉じる。
pub mod webhook;
pub mod webhook_command;

#[cfg(test)]
mod model_interaction_contract_tests;

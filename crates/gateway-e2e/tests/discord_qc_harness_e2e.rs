//! Discord gateway フェーズ1 のオフライン E2E（DESIGN-DISCORD-GATE v17）。
//!
//! トークン・ネットワーク・serenity 接続なしで実配線を通す決定的 E2E。

#[path = "discord_qc_harness/support/mod.rs"]
mod support;
use support::*;

include!("discord_qc_harness/owner_mention.rs");
include!("discord_qc_harness/daemon_world.rs");
include!("discord_qc_harness/daemon_e2e.rs");
include!("discord_qc_harness/daemon_bindings.rs");

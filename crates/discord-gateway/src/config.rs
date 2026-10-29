//! operator と gateway が共有する配置。HTTP bind は無い。**秘密（bot token）は載せない**
//! （設計 §1.3・§3.1: canonical config は bot の nonsecret identity / intents / transport 設定だけ）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct Placement {
    pub core_socket: String,
    /// Core-owned inbox shared by this co-located gateway. Optional only for
    /// backward-compatible attachment-free placements.
    #[serde(default)]
    pub attachment_spool_root: Option<String>,
    /// Core-owned outbox of workspace files to attach (D-1079). When absent, `reply` does not
    /// declare `files`.
    #[serde(default)]
    pub attachment_outbox_root: Option<String>,
    /// Gateway store DB (D-1087). The instance process opens it to read/write the per-session
    /// activity webhook routes; the daemon owns the schema. Absent = activity webhook commands
    /// answer "not configured" instead of guessing a path.
    #[serde(default)]
    pub database_path: Option<String>,
    /// D-1091: the environment this operator declares for the instance's turns (name → value).
    /// Core passes it verbatim to `execute_shell` children of that instance, so a CLI the agent
    /// runs acts as the same account the gateway acts as. Absent = nothing is declared and the
    /// hello frame is unchanged. A placement that declares secrets here is credential material:
    /// keep the file mode 0600. The bot token itself is still env-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_env: Option<std::collections::BTreeMap<String, String>>,
    /// D-1097: the VC settings file. The daemon writes the shared one here; absent = the
    /// `voice/settings.json` next to the placement (instance mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_settings: Option<String>,
    pub instances: Vec<InstancePlacement>,
}

impl Placement {
    /// The VC settings file for the placement at `placement_path`.
    pub fn voice_settings_path(&self, placement_path: &std::path::Path) -> std::path::PathBuf {
        match &self.voice_settings {
            Some(path) => std::path::PathBuf::from(path),
            None => crate::voice::settings::settings_path_for_placement(placement_path),
        }
    }

    /// D-1091: the declaration to put on this instance's hello. Absent placement entry = empty
    /// map = the previous frame (no `run_env` key at all).
    pub fn declared_run_env(&self) -> std::collections::BTreeMap<String, String> {
        self.run_env.clone().unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstancePlacement {
    pub instance_id: String,
    pub revision: u64,
    /// この agent の whitelisted channel session（`discord-{agent}-{guild}-{channel}`）の集合。
    /// gateway はこれらの say を drain し bind を待つ。実受理は core の ack が正（binding_for_address）。
    pub addresses: Vec<String>,
    pub config_b64: String,
    /// D-1097: the config core holds for this instance (fixed and neutral). The hello digest is
    /// its digest. Absent = the digest of `config_b64` (placements written before D-1097).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_config_b64: Option<String>,
}

impl InstancePlacement {
    /// The `config_digest` this instance presents on hello.
    pub fn hello_digest(&self) -> anyhow::Result<String> {
        let core = self.core_config_b64.as_deref().unwrap_or(&self.config_b64);
        Ok(config_digest(&decode_config_b64(core)?))
    }
}

/// instance の非秘密 config。bot token は含めない（env 注入のみ）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// core `agents.id`。受信 Discord message の (guild,channel) から binding address を組む。
    pub agent_id: String,
    /// bot 自身の Discord user id（10進 snowflake）。自分の投稿除外と hello の author identity。
    pub self_bot_id: String,
    /// bot の表示名（非秘密・任意）。
    #[serde(default)]
    pub name: Option<String>,
    /// application id（非秘密・任意）。
    #[serde(default)]
    pub application_id: Option<String>,
    /// 配送モード（say | tool_driven）。
    #[serde(default)]
    pub delivery_mode: Option<String>,
    /// Gateway-owned caller classification. IDs are authenticated Discord user snowflakes;
    /// shared layers receive only the resulting generic caller role.
    #[serde(default)]
    pub access: AccessConfig,
    /// system reaction（受理／完了／失敗）の絵文字。profile data（D17-12・instance config_b64）で
    /// 上書きし、無ければ現行と同じ既定値（👀🏁❌）。core に Discord 語彙は足さない。
    #[serde(default)]
    pub system_reactions: SystemReactions,
}

/// gateway が自動付与する system reaction の絵文字集合（設計 v17 §6.3・D17-12）。
///
/// - `accepted`（👀）: said を core が受理した時点で発端メッセージへ付ける（受理サイン＝gateway の責務）。
/// - `completed`（🏁）: activity ended で core が指定した最終生成の最後の投稿へ 1 件付ける。
/// - `failed`（❌）: 発端メッセージへの返信配送が失敗した時点で付ける。
/// - `no_reply`（🤐）: ターンが沈黙（say 無し）で終えた時点で発端メッセージへ付ける
///   （`CompletedNoReply` の reply_origin が Single のときだけ・裁定A で真の沈黙だけに立つ）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub co_agents: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub trusted_users: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemReactions {
    #[serde(default = "default_accepted")]
    pub accepted: String,
    #[serde(default = "default_completed")]
    pub completed: String,
    #[serde(default = "default_failed")]
    pub failed: String,
    #[serde(default = "default_no_reply")]
    pub no_reply: String,
}

fn default_accepted() -> String {
    "👀".to_string()
}
fn default_completed() -> String {
    "🏁".to_string()
}
fn default_failed() -> String {
    "❌".to_string()
}
fn default_no_reply() -> String {
    "🤐".to_string()
}

impl Default for SystemReactions {
    fn default() -> Self {
        Self {
            accepted: default_accepted(),
            completed: default_completed(),
            failed: default_failed(),
            no_reply: default_no_reply(),
        }
    }
}

impl Placement {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let place: Placement = serde_json::from_str(&text)?;
        place.validate()?;
        Ok(place)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.core_socket.is_empty() || !self.core_socket.starts_with('/') {
            anyhow::bail!("core_socket must be an absolute path");
        }
        if let Some(root) = &self.attachment_spool_root {
            if !std::path::Path::new(root).is_absolute() {
                anyhow::bail!("attachment_spool_root must be an absolute path");
            }
        }
        if let Some(root) = &self.attachment_outbox_root {
            if !std::path::Path::new(root).is_absolute() {
                anyhow::bail!("attachment_outbox_root must be an absolute path");
            }
        }
        if let Some(run_env) = &self.run_env {
            for (name, value) in run_env {
                if name.trim().is_empty() {
                    anyhow::bail!("run_env names must not be empty");
                }
                if value.is_empty() {
                    anyhow::bail!("run_env value for {name} must not be empty");
                }
            }
        }
        if let Some(path) = &self.database_path {
            if !std::path::Path::new(path).is_absolute() {
                anyhow::bail!("database_path must be an absolute path");
            }
        }
        if let Some(path) = &self.voice_settings {
            if !std::path::Path::new(path).is_absolute() {
                anyhow::bail!("voice_settings must be an absolute path");
            }
        }
        if self.instances.len() != 1 {
            anyhow::bail!("discord-gateway requires exactly one instance per process");
        }
        let mut seen = std::collections::BTreeSet::new();
        for inst in &self.instances {
            if inst.revision == 0 {
                anyhow::bail!("revision must be positive");
            }
            if inst.addresses.is_empty() {
                anyhow::bail!("addresses must be nonempty");
            }
            if inst.addresses.iter().any(|a| a.is_empty()) {
                anyhow::bail!("address must be nonempty");
            }
            parse_uuid(&inst.instance_id)?;
            if !seen.insert(inst.instance_id.clone()) {
                anyhow::bail!("duplicate instance_id is a double live; refuse startup");
            }
            let bytes = decode_config_b64(&inst.config_b64)?;
            let cfg = parse_instance_config(&bytes)?;
            validate_instance_config(&cfg)?;
        }
        Ok(())
    }
}

pub fn decode_config_b64(config_b64: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(config_b64)
        .map_err(|_| anyhow::anyhow!("config_b64 must be standard padded base64"))
}

pub fn canonicalize_config_b64(config_b64: &str) -> anyhow::Result<String> {
    use base64::Engine as _;
    let config = parse_instance_config(&decode_config_b64(config_b64)?)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&config)?))
}

pub fn config_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex_lower(&Sha256::digest(bytes))
}

pub fn parse_instance_config(bytes: &[u8]) -> anyhow::Result<InstanceConfig> {
    let cfg: InstanceConfig = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("instance config is not valid JSON object: {e}"))?;
    validate_instance_config(&cfg)?;
    Ok(cfg)
}

fn validate_instance_config(cfg: &InstanceConfig) -> anyhow::Result<()> {
    if cfg.agent_id.trim().is_empty() {
        anyhow::bail!("agent_id must be nonempty");
    }
    if !is_snowflake(&cfg.self_bot_id) {
        anyhow::bail!("self_bot_id must be a nonempty decimal snowflake");
    }
    if cfg
        .name
        .as_deref()
        .map(str::trim)
        .is_some_and(|n| n.is_empty())
    {
        anyhow::bail!("name, if present, must be nonempty");
    }
    match cfg.delivery_mode.as_deref() {
        None | Some("say") | Some("tool_driven") => {}
        Some(_) => anyhow::bail!("delivery_mode must be say or tool_driven"),
    }
    for user_id in cfg
        .access
        .owners
        .iter()
        .chain(cfg.access.co_agents.keys())
        .chain(cfg.access.trusted_users.iter())
    {
        if !is_snowflake(user_id) {
            anyhow::bail!("access user ids must be decimal snowflakes");
        }
    }
    if cfg
        .access
        .co_agents
        .values()
        .any(|agent_id| agent_id.trim().is_empty())
    {
        anyhow::bail!("access co-agent ids must be nonempty");
    }
    let sr = &cfg.system_reactions;
    for (label, emoji) in [
        ("accepted", &sr.accepted),
        ("completed", &sr.completed),
        ("failed", &sr.failed),
        ("no_reply", &sr.no_reply),
    ] {
        if emoji.trim().is_empty() {
            anyhow::bail!("system_reactions.{label} must be a nonempty emoji");
        }
    }
    Ok(())
}

/// D-1097: whether `address` is a channel address of `agent_id` (`discord-{agent}-{guild}-{channel}`).
pub fn parse_address_owner(agent_id: &str, address: &str) -> bool {
    crate::map::parse_address(agent_id, address).is_some()
}

/// 10進 snowflake（u64 に収まる非空数字列）。生 ID は会話へ出さないが config の妥当性検査には使う。
pub fn is_snowflake(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) && raw.parse::<u64>().is_ok()
}

pub fn parse_uuid(raw: &str) -> anyhow::Result<String> {
    let parsed = uuid::Uuid::parse_str(raw)?;
    let canonical = parsed.to_string();
    if canonical != raw {
        anyhow::bail!("instance_id must be canonical lowercase UUID");
    }
    Ok(canonical)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> serde_json::Value {
        serde_json::json!({
            "agent_id": "agent-x",
            "self_bot_id": "111111111111111111",
            "name": "crab",
        })
    }

    fn encode(v: &serde_json::Value) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(v).unwrap())
    }

    #[test]
    fn s5_secret_shaped_unknown_config_fields_are_rejected() {
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            br#"{"agent_id":"a","self_bot_id":"123456789012345678","token":"leak"}"#,
        );
        let bytes = decode_config_b64(&encoded).unwrap();
        assert!(serde_json::from_slice::<InstanceConfig>(&bytes).is_err());
    }

    #[test]
    fn valid_placement_passes_and_has_no_token_field() {
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            attachment_spool_root: None,
            attachment_outbox_root: None,
            database_path: None,
            run_env: None,
            voice_settings: None,
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec!["discord-agent-x-100-200".into()],
                config_b64: encode(&sample_config()),
                core_config_b64: None,
            }],
        };
        p.validate().unwrap();
        // config には token フィールドが無い（あっても serde が無視する＝InstanceConfig に無い）。
        let cfg =
            parse_instance_config(&decode_config_b64(&encode(&sample_config())).unwrap()).unwrap();
        assert_eq!(cfg.agent_id, "agent-x");
        assert_eq!(cfg.self_bot_id, "111111111111111111");
    }

    #[test]
    fn attachment_outbox_root_must_be_absolute() {
        let place = |root: &str| Placement {
            core_socket: "/tmp/g.sock".into(),
            attachment_spool_root: None,
            attachment_outbox_root: Some(root.into()),
            database_path: None,
            run_env: None,
            voice_settings: None,
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec!["discord-agent-x-100-200".into()],
                config_b64: encode(&sample_config()),
                core_config_b64: None,
            }],
        };
        place("/data/attachments/outbox").validate().unwrap();
        assert!(place("data/attachments/outbox").validate().is_err());
    }

    #[test]
    fn empty_addresses_fail_loud() {
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            attachment_spool_root: None,
            attachment_outbox_root: None,
            database_path: None,
            run_env: None,
            voice_settings: None,
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec![],
                config_b64: encode(&sample_config()),
                core_config_b64: None,
            }],
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn non_snowflake_self_bot_id_fails() {
        let mut v = sample_config();
        v["self_bot_id"] = serde_json::json!("not-a-number");
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn access_ids_are_gateway_owned_and_validated() {
        let mut valid = sample_config();
        valid["access"] = serde_json::json!({
            "owners": ["100"],
            "co_agents": {"200": "agent-b"},
            "trusted_users": ["300"]
        });
        let config = parse_instance_config(&serde_json::to_vec(&valid).unwrap()).unwrap();
        assert_eq!(config.access.owners, ["100"]);
        assert_eq!(config.access.co_agents["200"], "agent-b");
        assert_eq!(config.access.trusted_users, ["300"]);

        valid["access"]["owners"] = serde_json::json!(["not-a-snowflake"]);
        assert!(parse_instance_config(&serde_json::to_vec(&valid).unwrap()).is_err());
    }

    #[test]
    fn dm_address_with_empty_guild_component_is_accepted() {
        // DM は guild 成分が空（discord-{agent}--{channel}）。address 非空なら通す（設計 §3.2/D17-03）。
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            attachment_spool_root: None,
            attachment_outbox_root: None,
            database_path: None,
            run_env: None,
            voice_settings: None,
            instances: vec![InstancePlacement {
                instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
                revision: 1,
                addresses: vec!["discord-agent-x--200".into()],
                config_b64: encode(&sample_config()),
                core_config_b64: None,
            }],
        };
        p.validate().unwrap();
    }

    #[test]
    fn system_reactions_default_to_legacy_emojis() {
        // config に system_reactions が無ければ現行既定（👀🏁❌）。
        let cfg = parse_instance_config(&serde_json::to_vec(&sample_config()).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "👀");
        assert_eq!(cfg.system_reactions.completed, "🏁");
        assert_eq!(cfg.system_reactions.failed, "❌");
        assert_eq!(cfg.system_reactions.no_reply, "🤐");
    }

    #[test]
    fn system_reactions_can_be_overridden_via_profile_data() {
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({
            "accepted": "🫡", "completed": "✅", "failed": "💥", "no_reply": "🙊",
        });
        let cfg = parse_instance_config(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "🫡");
        assert_eq!(cfg.system_reactions.completed, "✅");
        assert_eq!(cfg.system_reactions.failed, "💥");
        assert_eq!(cfg.system_reactions.no_reply, "🙊");
    }

    #[test]
    fn partial_system_reactions_fall_back_per_field() {
        // 一部だけ指定 → 残りは既定。
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({ "completed": "✅" });
        let cfg = parse_instance_config(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(cfg.system_reactions.accepted, "👀");
        assert_eq!(cfg.system_reactions.completed, "✅");
        assert_eq!(cfg.system_reactions.failed, "❌");
        assert_eq!(cfg.system_reactions.no_reply, "🤐");
    }

    #[test]
    fn empty_system_reaction_emoji_fails() {
        let mut v = sample_config();
        v["system_reactions"] = serde_json::json!({ "accepted": "" });
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn bad_delivery_mode_fails() {
        let mut v = sample_config();
        v["delivery_mode"] = serde_json::json!("weird");
        assert!(parse_instance_config(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn multiple_instances_are_rejected_before_starting_receivers() {
        let instance = |instance_id: &str| InstancePlacement {
            instance_id: instance_id.into(),
            revision: 1,
            addresses: vec!["discord-agent-x-100-200".into()],
            config_b64: encode(&sample_config()),
            core_config_b64: None,
        };
        let p = Placement {
            core_socket: "/tmp/g.sock".into(),
            attachment_spool_root: None,
            attachment_outbox_root: None,
            database_path: None,
            run_env: None,
            voice_settings: None,
            instances: vec![
                instance("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
                instance("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"),
            ],
        };

        assert_eq!(
            p.validate().unwrap_err().to_string(),
            "discord-gateway requires exactly one instance per process"
        );
    }

    fn placement_without_run_env(core_socket: &str) -> serde_json::Value {
        serde_json::json!({
            "core_socket": core_socket,
            "instances": [{
                "instance_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                "revision": 1,
                "addresses": ["discord-agent-x-100-200"],
                "config_b64": encode(&sample_config()),
            }],
        })
    }

    /// D-1091: 配置に宣言が無ければ何も宣言しない（hello は従来と同じ frame）。
    #[test]
    fn a_placement_without_a_declaration_declares_nothing() {
        let place: Placement =
            serde_json::from_value(placement_without_run_env("/tmp/g.sock")).unwrap();
        place.validate().unwrap();
        assert!(place.declared_run_env().is_empty());
        assert!(place.run_env.is_none());
    }

    /// D-1091: 配置が宣言を持てば、そのまま（名前→値）をこの instance の宣言として使う。
    #[test]
    fn a_placement_declaration_is_passed_through_verbatim() {
        let mut value = placement_without_run_env("/tmp/g.sock");
        value["run_env"] = serde_json::json!({"BINDING_ACCOUNT": "alpha"});
        let place: Placement = serde_json::from_value(value).unwrap();
        place.validate().unwrap();
        let declared = place.declared_run_env();
        assert_eq!(
            declared.get("BINDING_ACCOUNT").map(String::as_str),
            Some("alpha")
        );
    }

    /// D-1091: 空の名前・空の値は起動前に落とす（黙って宣言したつもりにさせない）。
    #[test]
    fn an_empty_declaration_name_or_value_fails_loud() {
        for run_env in [
            serde_json::json!({"  ": "value"}),
            serde_json::json!({"NAME": ""}),
        ] {
            let mut value = placement_without_run_env("/tmp/g.sock");
            value["run_env"] = run_env;
            let place: Placement = serde_json::from_value(value).unwrap();
            assert!(place.validate().is_err(), "{place:?}");
        }
    }
}

#[cfg(test)]
mod core_config_tests {
    use super::*;

    /// D-1097 D2: a placement without `core_config_b64` keeps the previous hello digest (the
    /// digest of `config_b64`), so a placement written before the change still connects.
    #[test]
    fn d1097_hello_digest_falls_back_to_the_discord_config() {
        use base64::Engine as _;
        let config = base64::engine::general_purpose::STANDARD
            .encode(br#"{"agent_id":"a","self_bot_id":"111111111111111111"}"#);
        let place = InstancePlacement {
            instance_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
            revision: 1,
            addresses: vec!["discord-a-1-2".into()],
            config_b64: config.clone(),
            core_config_b64: None,
        };
        assert_eq!(
            place.hello_digest().unwrap(),
            config_digest(&decode_config_b64(&config).unwrap())
        );
        let neutral = base64::engine::general_purpose::STANDARD.encode(br#"{"kind":"x"}"#);
        let place = InstancePlacement {
            core_config_b64: Some(neutral.clone()),
            ..place
        };
        assert_eq!(
            place.hello_digest().unwrap(),
            config_digest(&decode_config_b64(&neutral).unwrap())
        );
        let broken = InstancePlacement {
            core_config_b64: Some("not base64".into()),
            ..place
        };
        assert!(broken.hello_digest().is_err());
    }
}

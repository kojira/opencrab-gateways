//! discord-gateway 独立 binary。serenity Gateway ⇄ V3 UDS。HTTP listen しない。
//! 1 process = exact 1 agent（設計 §0）。bot token は env のみ、起動直後に process env から消す。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use opencrab_discord_gateway::config::{decode_config_b64, Placement};
use opencrab_discord_gateway::harness::HarnessOverrides;
use opencrab_discord_gateway::run::spawn_instance_with_voice;
use opencrab_discord_gateway::secret::take_bot_token;

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("runtime")?;
    rt.block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab_discord_gateway=info".parse()?)
                .add_directive("opencrab_gate_client=info".parse()?),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .context("usage: discord-gateway daemon <config.json> | instance <placement.json> | settings <daemon-config.json> <host:port> | voice-settings <settings.json> <port>")?;
    if first == "settings" {
        // D-1097 D4: talks to the daemon's admin socket only; holds no key and no token.
        let usage = "usage: discord-gateway settings <daemon-config.json> <host:port>";
        let path = args.next().map(PathBuf::from).context(usage)?;
        let bind =
            opencrab_discord_gateway::settings_http::parse_bind(&args.next().context(usage)?)?;
        let config = opencrab_discord_gateway::daemon::DaemonConfig::load(&path)?;
        return opencrab_discord_gateway::settings_http::run(config, bind).await;
    }
    if first == "voice-settings" {
        let usage = "usage: discord-gateway voice-settings <settings.json> <port>";
        let path = args.next().map(PathBuf::from).context(usage)?;
        let port = args.next().context(usage)?.parse::<u16>().context(usage)?;
        return opencrab_discord_gateway::voice::settings_http::run(path, port).await;
    }
    if first == "daemon" {
        let path = args
            .next()
            .map(PathBuf::from)
            .context("usage: discord-gateway daemon <config.json>")?;
        return opencrab_discord_gateway::daemon::run(
            opencrab_discord_gateway::daemon::DaemonConfig::load(&path)?,
        )
        .await;
    }
    let path = if first == "instance" {
        args.next()
            .map(PathBuf::from)
            .context("usage: discord-gateway instance <placement.json>")?
    } else {
        PathBuf::from(first)
    };
    // bot token を env から 1 回だけ受け、直後に process env から消す（設計 §1.3・§5）。
    let token = take_bot_token().map(Arc::new);
    let raw_placement: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let control = raw_placement
        .get("control_socket")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let start_nonce = raw_placement
        .get("start_nonce")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let place = Placement::load(&path)?;
    let socket = PathBuf::from(&place.core_socket);
    let attachment_spool_root = place.attachment_spool_root.as_deref().map(PathBuf::from);
    let attachment_outbox_root = place.attachment_outbox_root.as_deref().map(PathBuf::from);
    // D-1087: この gateway の DB（宛先の保存先）。daemon が書く配置から受け取る。
    let activity_store_path = place.database_path.as_deref().map(PathBuf::from);
    // QC ハーネス差し替えは env からのみ（既定 OFF＝production 挙動）。
    let overrides = HarnessOverrides::from_env();

    if token.is_none() && !overrides.dry_run {
        anyhow::bail!("DISCORD_BOT_TOKEN が未設定（production は token 必須・dry-run 以外）");
    }

    let mut inst = place
        .instances
        .first()
        .cloned()
        .context("validated placement has no instance")?;
    inst.channel_socket = place.channel_socket.as_deref().map(PathBuf::from);
    let bytes = decode_config_b64(&inst.config_b64)?;
    // VC 設定は placement が示す共有ファイル（daemon 配下）か、無ければ placement の隣の
    // voice/settings.json（instance モード）。無ければ VC は「未設定」を返す。
    let voice_settings = place.voice_settings_path(&path);
    let client = spawn_instance_with_voice(
        socket,
        &inst,
        &bytes,
        token,
        overrides,
        (attachment_spool_root, attachment_outbox_root),
        Some(voice_settings),
        activity_store_path,
        // D-1091: only what the placement declares; absent means the frame is unchanged.
        place.declared_run_env(),
    )?;
    if let (Some(control), Some(nonce)) = (control, start_nonce) {
        // Ready once core accepted the hello (D-1099: there is nothing else to wait for).
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            while !client.connection_live().await {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            if let Ok(mut stream) = tokio::net::UnixStream::connect(control).await {
                let message = serde_json::json!({"nonce":nonce,"pid":std::process::id()});
                let _ = stream.write_all(message.to_string().as_bytes()).await;
            }
        });
    }

    tracing::info!("discord-gateway running");
    std::future::pending::<()>().await;
    Ok(())
}

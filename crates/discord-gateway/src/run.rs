//! 1 instance の UDS client・受信ループ・say consumer を束ねる（設計 §0・§4.3・§5・§6）。
//!
//! Nostr gateway と違い timeline/bundle 車線は無い。1 channel = 1 binding、1 Message Create = 1 said。
//! 受信は serenity（または fake fixture）を 1 本回し、ack 済み binding の channel だけ said にする。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use opencrab_gate_client::wire::{LiveInboundScope, SaidCaller, SaidContext};
use opencrab_gate_client::{InvokeHandler, SayPolicy};

use crate::attachment::AttachmentSpool;
use crate::config::{
    config_digest, parse_instance_config, AccessConfig, InstanceConfig, InstancePlacement,
    SystemReactions,
};
use crate::harness::HarnessOverrides;
use crate::map::{
    address_for, address_of, map_message, parse_address, parse_event_line, parse_origin,
};
use crate::model::{InteractionAction, ModelInteractionEvent, COMMAND_TIMEOUT};
use crate::ops::{
    operation_declarations, targets_for, BindingDeliveryTargets, DiscordInvokeHandler,
};
use crate::post::{deliver_say, SayDelivery};
use crate::receive::{run_fake_events_once, run_serenity_receive, OnLine};
use crate::run::model_command::{
    edit_model_response, handle_model_interaction, model_command_address,
    resolve_model_command_transport,
};
use crate::transport::{DiscordTransport, DryRunTransport, SerenityTransport, TransportOutcome};
use crate::voice::session::{VoiceManager, VoiceManagerConfig};
use crate::voice::songbird_player::{songbird_config, SongbirdPlayer};

const RETRY: Duration = Duration::from_millis(200);

/// 1 instance を起動する。hello に能力宣言を載せて接続し、受信ループと say consumer を spawn する。
pub fn spawn_instance(
    socket: PathBuf,
    place: &InstancePlacement,
    config_bytes: &[u8],
    token: Option<Arc<String>>,
    overrides: HarnessOverrides,
    attachment_spool_root: Option<PathBuf>,
) -> anyhow::Result<Arc<InstanceClient>> {
    spawn_instance_with_voice(
        socket,
        place,
        config_bytes,
        token,
        overrides,
        (attachment_spool_root, None),
        None,
        None,
        // D-1091: callers that have no placement-wide declaration send none.
        std::collections::BTreeMap::new(),
    )
}

/// [`spawn_instance`] に VC（D-1072）を加える。`voice_settings` は VC 設定ファイルの位置で、
/// production（serenity 接続あり）のときだけ songbird を登録する。outbox は D-1079 の送信添付、
/// `activity_store_path` は D-1087 の宛先 DB（どちらも任意）。
#[allow(clippy::too_many_arguments)]
pub fn spawn_instance_with_voice(
    socket: PathBuf,
    place: &InstancePlacement,
    config_bytes: &[u8],
    token: Option<Arc<String>>,
    overrides: HarnessOverrides,
    (attachment_spool_root, attachment_outbox_root): (Option<PathBuf>, Option<PathBuf>),
    voice_settings: Option<PathBuf>,
    activity_store_path: Option<PathBuf>,
    run_env: std::collections::BTreeMap<String, String>,
) -> anyhow::Result<Arc<InstanceClient>> {
    if overrides.is_active() {
        tracing::warn!(
            fake_events = ?overrides.fake_events,
            dry_run = overrides.dry_run,
            instance = %place.instance_id,
            "QC harness overrides ACTIVE — this is NOT a production path"
        );
    }
    let cfg = parse_instance_config(config_bytes)?;
    let digest = config_digest(config_bytes);

    // transport: dry-run（REST を叩かずログ）か production（serenity REST・token 保持）。
    // say も invoke も同一 transport を通すので dry-run 分岐は 1 箇所。
    let (transport, serenity_http): (Arc<dyn DiscordTransport>, Option<Arc<serenity::http::Http>>) =
        if overrides.dry_run {
            (Arc::new(DryRunTransport), None)
        } else {
            let token = token
                .clone()
                .ok_or_else(|| anyhow::anyhow!("production は bot token（env）が必須"))?;
            let serenity = Arc::new(SerenityTransport::new(&token));
            let http = serenity.http();
            (serenity, Some(http))
        };

    let songbird = (serenity_http.is_some() && overrides.fake_events.is_none())
        .then(|| songbird::Songbird::serenity_from_config(songbird_config()));
    let voice = voice_settings
        .zip(songbird.clone())
        .map(|(settings_path, songbird)| {
            VoiceManager::new(
                VoiceManagerConfig {
                    agent_id: cfg.agent_id.clone(),
                    self_bot_id: cfg.self_bot_id.clone(),
                    access: cfg.access.clone(),
                    addresses: place.addresses.clone(),
                    settings_path,
                },
                Arc::new(SongbirdPlayer::new(songbird)),
                transport.clone(),
            )
        });
    let delivery_targets: BindingDeliveryTargets =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let invoke_handler = DiscordInvokeHandler::with_voice(
        transport.clone(),
        Arc::clone(&delivery_targets),
        voice.clone(),
    )
    .with_outbox_root(attachment_outbox_root.clone());

    // D-1091: the environment the operator declared for this instance (empty = the previous
    // frame). Names only in the log; the values never reach a log line.
    if !run_env.is_empty() {
        tracing::debug!(
            instance = %place.instance_id,
            declared = ?run_env.keys().collect::<Vec<_>>(),
            "binding env declared"
        );
    }
    let client = InstanceClient::spawn_with_operations_and_env(
        socket,
        place.instance_id.clone(),
        place.revision,
        cfg.self_bot_id.clone(),
        digest,
        SayPolicy::AcceptToLiveQueue,
        Some(operation_declarations(attachment_outbox_root.is_some())),
        opencrab_gate_client::RuntimeCapabilities {
            final_delivery: opencrab_gate_client::FinalDelivery::OperationDriven,
        },
        Arc::new(invoke_handler) as Arc<dyn InvokeHandler>,
        run_env,
    );
    if let Some(voice) = &voice {
        voice.attach_client(client.clone());
    }

    let attachment_spool = attachment_spool_root
        .as_deref()
        .map(AttachmentSpool::new)
        .transpose()?
        .map(Arc::new);
    // D-1087: 活動 webhook の宛先は gateway DB にだけ持つ。開けなければ「未設定」として
    // 動かす（コマンドは理由を返し、配送は何もしない）。
    let activity_store = match activity_store_path.as_deref() {
        Some(path) => match crate::webhook::RouteStore::open(path) {
            Ok(store) => Some(store),
            Err(error) => {
                tracing::warn!(error = %error, "activity webhook store unavailable; /webhook is disabled");
                None
            }
        },
        None => None,
    };
    supervise(Supervision {
        client: client.clone(),
        addresses: place.addresses.clone(),
        cfg,
        transport,
        token,
        overrides,
        delivery_targets,
        attachment_spool,
        serenity_http,
        songbird: voice.as_ref().and(songbird),
        voice,
        activity_store,
    });
    Ok(client)
}

struct Supervision {
    client: Arc<InstanceClient>,
    addresses: Vec<String>,
    cfg: InstanceConfig,
    transport: Arc<dyn DiscordTransport>,
    token: Option<Arc<String>>,
    overrides: HarnessOverrides,
    delivery_targets: BindingDeliveryTargets,
    attachment_spool: Option<Arc<AttachmentSpool>>,
    serenity_http: Option<Arc<serenity::http::Http>>,
    songbird: Option<Arc<songbird::Songbird>>,
    voice: Option<Arc<VoiceManager>>,
    activity_store: Option<crate::webhook::RouteStore>,
}

fn supervise(supervision: Supervision) {
    let Supervision {
        client,
        addresses,
        cfg,
        transport,
        token,
        overrides,
        delivery_targets,
        attachment_spool,
        serenity_http,
        songbird,
        voice,
        activity_store,
    } = supervision;
    // 受信ループ（1 本）: fixture か serenity。ack 済み binding の channel だけ said にする。
    // 👀 は受信時ではなく say consumer 側（activity started）で付けるので、受信は transport/
    // reactions を持たない（R2）。
    let on_line = build_on_line(
        client.clone(),
        cfg.agent_id.clone(),
        cfg.self_bot_id.clone(),
        cfg.access.clone(),
        addresses.clone(),
        attachment_spool,
        serenity_http,
        voice.clone(),
        activity_store.clone(),
    );
    tokio::spawn(async move {
        if let Some(fixture) = overrides.fake_events {
            if let Err(e) = run_fake_events_once(&fixture, on_line).await {
                tracing::error!(error = %e, "fake events failed");
            }
        } else if let Some(token) = token {
            if let Err(e) = run_serenity_receive(&token, on_line, songbird).await {
                tracing::error!(error = %e, "serenity receive ended");
            }
        } else {
            tracing::error!("no fake_events fixture and no token; receive loop idle");
        }
    });

    // D-1087: 活動の配送口。core の `tool_activity` / `subtask_lifecycle` を受信ループが
    // ここへ渡し、宛先があるときだけ送る（宛先は gateway の DB、core は知らない）。
    let activity =
        activity_store.map(|store| Arc::new(crate::webhook::ActivityDelivery::new(store)));
    // say consumer（address ごと）: core の say を channel の通常投稿として配送する。
    for address in addresses {
        spawn_say_consumer(
            client.clone(),
            address,
            transport.clone(),
            cfg.agent_id.clone(),
            cfg.system_reactions.clone(),
            Arc::clone(&delivery_targets),
            voice.clone(),
            activity.clone(),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn build_on_line(
    client: Arc<InstanceClient>,
    agent_id: String,
    self_bot_id: String,
    access: AccessConfig,
    configured_addresses: Vec<String>,
    attachment_spool: Option<Arc<AttachmentSpool>>,
    serenity_http: Option<Arc<serenity::http::Http>>,
    voice: Option<Arc<VoiceManager>>,
    activity_store: Option<crate::webhook::RouteStore>,
) -> OnLine {
    let model_cache = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    Arc::new(move |line: String| {
        let client = client.clone();
        let agent_id = agent_id.clone();
        let self_bot_id = self_bot_id.clone();
        let access = access.clone();
        let configured_addresses = configured_addresses.clone();
        let attachment_spool = attachment_spool.clone();
        let serenity_http = serenity_http.clone();
        let model_cache = model_cache.clone();
        let voice = voice.clone();
        let activity_store = activity_store.clone();
        tokio::spawn(async move {
            let gate = (
                &*client,
                agent_id.as_str(),
                &access,
                &configured_addresses[..],
            );
            if thinking_command::dispatch(gate, serenity_http.as_deref(), &line).await {
                return;
            }
            if webhook_command::dispatch(
                gate,
                activity_store.as_ref(),
                serenity_http.as_deref(),
                &line,
            )
            .await
            {
                return;
            }
            if let Ok(event) = serde_json::from_str::<ModelInteractionEvent>(&line) {
                if event.event_kind == "model_interaction" {
                    if let Some(http) = serenity_http {
                        handle_model_interaction(
                            &client,
                            &agent_id,
                            &access,
                            &configured_addresses,
                            &http,
                            &model_cache,
                            event,
                        )
                        .await;
                    }
                    return;
                }
            }
            handle_incoming(
                &client,
                &agent_id,
                &self_bot_id,
                &access,
                &line,
                attachment_spool.as_deref(),
                voice.as_deref(),
            )
            .await;
        });
    })
}

/// system reaction を発端メッセージ（origin anchor）へ best-effort で付ける。👀（受理）と
/// ❌（配送失敗）はこちら——発端に付けるのが正（照合: 旧 message_loop も同じ側）。
async fn react_system(transport: &Arc<dyn DiscordTransport>, origin: &str, emoji: &str) {
    let Some((channel, message)) = parse_origin(origin) else {
        tracing::debug!(%origin, "skip system reaction: origin not a discord anchor");
        return;
    };
    react_system_on(transport, &channel, &message, emoji).await;
}

async fn react_no_reply(
    transport: &Arc<dyn DiscordTransport>,
    reply_origin: Option<&str>,
    emoji: &str,
) {
    if let Some(origin) = reply_origin {
        react_system(transport, origin, emoji).await;
    }
}

/// (channel, message) を直接指定して system reaction を付ける（失敗は warn のみ・非致命）。
/// 🏁（完了）は activity ended で core が指定した**最終生成の最後の投稿**へ付けるため、
/// 発話 id の対応表から得た自分の message id をここへ渡す。
/// legacy `add_reaction_non_fatal` と同じく、付与失敗が turn 処理を巻き込まないようにする。
async fn react_system_on(
    transport: &Arc<dyn DiscordTransport>,
    channel: &str,
    message: &str,
    emoji: &str,
) {
    match transport.add_system_reaction(channel, message, emoji).await {
        TransportOutcome::Ok(_) => {}
        TransportOutcome::Rejected => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction rejected (non-fatal)"
            )
        }
        TransportOutcome::Indeterminate => {
            tracing::warn!(
                channel,
                message,
                emoji,
                "system reaction outcome unknown (non-fatal)"
            )
        }
    }
}

pub(crate) fn caller_for(access: &AccessConfig, author_id: &str) -> SaidCaller {
    if access.owners.iter().any(|id| id == author_id) {
        SaidCaller::Owner
    } else if let Some(agent_id) = access.co_agents.get(author_id) {
        SaidCaller::CoAgent {
            agent_id: agent_id.clone(),
        }
    } else if access.trusted_users.iter().any(|id| id == author_id) {
        SaidCaller::TrustedUser
    } else {
        SaidCaller::Agent
    }
}

/// 受信 1 件を said へ。自分の投稿と非 ack channel は core へ送らない（§4.3・§5.1）。
/// 受理された人間の発言者は VC 参加の呼びかけ手として `voice` に覚えさせる（D-1072）。
pub(crate) async fn handle_incoming(
    client: &InstanceClient,
    agent_id: &str,
    self_bot_id: &str,
    access: &AccessConfig,
    line: &str,
    attachment_spool: Option<&AttachmentSpool>,
    voice: Option<&VoiceManager>,
) {
    let Some(msg) = parse_event_line(line) else {
        return;
    };
    // map_message が自分の投稿・非 snowflake を落とす。author は Discord 認証済み sender（#848）。
    let Some(mapped) = map_message(&msg, self_bot_id) else {
        return;
    };
    let address = address_of(agent_id, &msg);
    // 購読集合: core が bind ack した channel だけを said にする（wire discovery を追加しない）。
    if client.binding_for_address(&address).await.is_none() {
        tracing::debug!(%address, "message outside ack'd binding; discarded (no core frame)");
        return;
    }
    let mut text = mapped.text;
    let mut attachments = Vec::with_capacity(mapped.attachments.len());
    for source in &mapped.attachments {
        let result = match attachment_spool {
            Some(spool) => {
                spool
                    .download(&client.instance_id, &mapped.origin, source)
                    .await
            }
            None => Err(anyhow::anyhow!("attachment spool is not configured")),
        };
        match result {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => {
                tracing::warn!(
                    attachment_id = %source.id,
                    error = %error,
                    "attachment download failed"
                );
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&format!(
                    "[Attachment unavailable: {} ({})]",
                    source.filename,
                    source.content_type.as_deref().unwrap_or("unknown")
                ));
            }
        }
    }
    let context = SaidContext {
        caller: caller_for(access, &mapped.author_id),
        start_turn: true,
        system_context: None,
        reply_target: None,
        live_inbound_scope: LiveInboundScope::All,
        model_profile: opencrab_gate_client::wire::TurnModelProfile::Default,
        agent_prelude: None,
        provisional_id: None,
    };
    let outcome = client
        .post_said_with_context(
            &address,
            &mapped.origin,
            &mapped.author_id,
            mapped.author_label.as_deref(),
            &context,
            &text,
            &attachments,
        )
        .await;
    let accepted = matches!(&outcome, Ok(SaidOutcome::Accepted { .. }));
    match outcome {
        Ok(SaidOutcome::Accepted { seq }) => {
            // 👀 はここ（受理・推論前）では付けない。LLM request にこのメッセージを新しく
            // 含める直前に付ける。record-only は読まれるまで付けない。実際の付与は consumer が
            // activity read(origin) を受けた時点で行う（#964）。
            tracing::info!(%address, seq, "said accepted");
            if let (Some(voice), false) = (voice, msg.author.bot) {
                voice.note_speaker(&address, &mapped.author_id);
            }
        }
        Ok(SaidOutcome::NotAdmitted) => tracing::info!(%address, "said not admitted"),
        Ok(SaidOutcome::Disconnected) => tracing::info!(%address, "said disconnected"),
        Ok(SaidOutcome::WireErr { code, detail }) => {
            tracing::warn!(%address, code, ?detail, "said wire err")
        }
        Err(PostRefuse::NotReady) => tracing::info!(%address, "said dropped; binding not ready"),
        Err(PostRefuse::Busy) => tracing::info!(%address, "said refused; binding busy"),
    }
    if !accepted {
        if let Some(spool) = attachment_spool {
            for attachment in &attachments {
                spool.remove(attachment).await;
            }
        }
    }
}

/// ターン終端で、そのターンの積み残しをセッション宛先へ配送する（D-1087）。
///
/// 通常は活動イベントごとに配送済み（[`deliver_activity_event`]）なので、ここで送るのは
/// 配送時に宛先が無かった等で残った分だけ。宛先が無ければ何も送らない（記録は core に残る）。
async fn deliver_activity_turn(
    activity: &Option<Arc<crate::webhook::ActivityDelivery>>,
    client: &Arc<InstanceClient>,
    address: &str,
    agent_id: &str,
    channel: Option<&str>,
) {
    let (Some(activity), Some(channel)) = (activity, channel) else {
        return;
    };
    let Some(binding_id) = client.binding_for_address(address).await else {
        return;
    };
    activity.flush(&binding_id, agent_id, channel);
}

/// 活動イベント 1 件を、その場でセッション宛先へ配送する（D-1087）。
///
/// core は活動を発生順に流すので、受け取った時点で送ることで順序が保たれる。宛先が
/// 無ければ何も送らない（記録は core に残る）。
async fn deliver_activity_event(
    activity: &Option<Arc<crate::webhook::ActivityDelivery>>,
    client: &Arc<InstanceClient>,
    address: &str,
    agent_id: &str,
    channel: Option<&str>,
    item: crate::webhook::ActivityItem,
) {
    let (Some(activity), Some(channel)) = (activity, channel) else {
        return;
    };
    let Some(binding_id) = client.binding_for_address(address).await else {
        return;
    };
    activity.event(&binding_id, agent_id, channel, item);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_say_consumer(
    client: Arc<InstanceClient>,
    address: String,
    transport: Arc<dyn DiscordTransport>,
    agent_id: String,
    reactions: SystemReactions,
    delivery_targets: BindingDeliveryTargets,
    voice: Option<Arc<VoiceManager>>,
    activity: Option<Arc<crate::webhook::ActivityDelivery>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // address → channel snowflake（say はこの channel への通常投稿）。
        let channel = parse_address(&agent_id, &address).map(|(_, ch)| ch);
        // ターン進行中の typing keepalive（設計 §5.4）。activity started で起こし、ended で止める。
        // 別タスクで打つのでこのループはブロックしない。ガードはこのループ寿命に束ねて保持し、
        // consumer 終了時（＝プロセス終了時）にも drop されて止まる。
        let mut typing: Option<crate::typing::TypingKeepalive> = None;
        // #930: 👀（accepted）を付けた発端 origin の集合。started と read の両方で付けるので、
        // 同一 origin への二重付与を防ぐループローカルガード（1 origin 1 回）。
        let mut accepted_origins: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message {
                    delivery_id,
                    text,
                    reply_origin,
                    stream_id,
                }) => {
                    // DI-16: say は通常発言（reply target は暗黙設定しない）。配送成功時は core の
                    // delivery_id と Discord message id の対応だけを記録する。🏁 の判断・付与は
                    // activity ended 起点の LiveEvent::Completed が担う。❌ は従来どおり発端へ付ける。
                    let Some(channel) = &channel else {
                        tracing::warn!(%address, "say dropped; address has no channel component");
                        continue;
                    };
                    match deliver_say(&transport, channel, &text).await {
                        SayDelivery::Posted { message_id } => {
                            tracing::info!(%address, "say posted");
                            if let Some(voice) = &voice {
                                voice.speak_say(channel, &text, stream_id.as_deref());
                            }
                            if let Some(own) = &message_id {
                                if let Some(binding_id) = client.binding_for_address(&address).await
                                {
                                    targets_for(&delivery_targets, &binding_id)
                                        .await
                                        .lock()
                                        .await
                                        .insert(delivery_id, (channel.clone(), own.clone()));
                                }
                            } else {
                                tracing::debug!(%address, "posted say has no message id");
                            }
                        }
                        SayDelivery::Failed(e) => {
                            tracing::warn!(%address, error = %e, "say post failed");
                            if let Some(origin) = &reply_origin {
                                // ❌: 発端メッセージへの返信配送が失敗した（失敗サイン）。
                                react_system(&transport, origin, &reactions.failed).await;
                            }
                        }
                    }
                }
                Some(event @ (LiveEvent::Partial { .. } | LiveEvent::ProvisionalResult { .. })) => {
                    // D-1083: 生成途中の完成した文を VC で先に読み上げる（投稿はしない）。
                    if let (Some(voice), Some(channel)) = (&voice, &channel) {
                        voice.on_live_speech(channel, event).await;
                    }
                }
                Some(LiveEvent::Activity { state, origin, .. }) => {
                    // #964 👀: 投稿を新しく含む LLM request の直前に届く read+origin だけで付ける。
                    // started は typing 専用で origin を持たない。1 origin 1 回。record-only/held は
                    // read が来ないので「読まれるまで付かない」が保たれる。
                    if state == "read" {
                        if let Some(origin) = &origin {
                            if accepted_origins.insert(origin.clone()) {
                                react_system(&transport, origin, &reactions.accepted).await;
                            }
                        }
                    }
                    // typing: core の platform-neutral activity を typing 開始/keepalive/停止へ機械的に
                    // 写す（設計 §5.4）。started で keepalive を起こし、ended（裁定A で say 配送後に届く）
                    // で止める。read はターン境界ではないので typing を駆動しない（進行中を止めない）。
                    // channel が無い address（say 不可）では typing も出さない。best-effort。
                    if state != "read" {
                        if let Some(channel) = &channel {
                            typing = crate::typing::apply_activity_state(
                                typing,
                                &state,
                                &transport,
                                channel,
                                crate::typing::TYPING_REFRESH_INTERVAL,
                            );
                        }
                    }
                }
                Some(LiveEvent::ToolActivity {
                    activity_id,
                    call_id,
                    sub_session_id,
                    tool,
                    status,
                    depth,
                    started_at,
                    duration_ms,
                    args,
                    result,
                    error,
                }) => {
                    // D-1087: ツール活動。core は宛先を知らないので、gateway がセッション宛先へ送る。
                    if let Some(status) = crate::webhook::ActivityStatus::from_wire(&status) {
                        deliver_activity_event(
                            &activity,
                            &client,
                            &address,
                            &agent_id,
                            channel.as_deref(),
                            crate::webhook::ActivityItem::Tool(crate::webhook::ActivityEvent {
                                activity_id,
                                call_id,
                                tool,
                                status,
                                depth,
                                sub_session_id,
                                started_at,
                                duration_ms,
                                args,
                                result,
                                error,
                            }),
                        )
                        .await;
                    }
                }
                Some(LiveEvent::SubtaskLifecycle {
                    run_id,
                    sub_session_id,
                    status,
                    label,
                    task,
                    detail,
                    duration_ms,
                    result_text,
                }) => {
                    // D-1087: サブタスク走行のライフサイクル（started / progress / 終端）。
                    deliver_activity_event(
                        &activity,
                        &client,
                        &address,
                        &agent_id,
                        channel.as_deref(),
                        crate::webhook::ActivityItem::Lifecycle(crate::webhook::LifecycleEvent {
                            run_id,
                            sub_session_id,
                            status,
                            label,
                            task,
                            detail,
                            duration_ms,
                            result_text,
                        }),
                    )
                    .await;
                }
                Some(LiveEvent::Completed { target }) => {
                    deliver_activity_turn(
                        &activity,
                        &client,
                        &address,
                        &agent_id,
                        channel.as_deref(),
                    )
                    .await;
                    let location =
                        if let Some(binding_id) = client.binding_for_address(&address).await {
                            targets_for(&delivery_targets, &binding_id)
                                .await
                                .lock()
                                .await
                                .remove(&target)
                        } else {
                            None
                        };
                    if let Some((target_channel, message)) = location {
                        react_system_on(
                            &transport,
                            &target_channel,
                            &message,
                            &reactions.completed,
                        )
                        .await;
                    } else {
                        tracing::debug!(%address, %target, "skip 🏁: completed target not found");
                    }
                }
                Some(LiveEvent::CompletedNoReply { reply_origin }) => {
                    deliver_activity_turn(
                        &activity,
                        &client,
                        &address,
                        &agent_id,
                        channel.as_deref(),
                    )
                    .await;
                    // Authoritative silent origin（または旧core fallback）を機械的に🤐へ写す。
                    react_no_reply(&transport, reply_origin.as_deref(), &reactions.no_reply).await;
                }
                Some(LiveEvent::TurnFailed { reply_origin }) => {
                    deliver_activity_turn(
                        &activity,
                        &client,
                        &address,
                        &agent_id,
                        channel.as_deref(),
                    )
                    .await;
                    // ❌: ターン失敗（エンジン/プロバイダ失敗）を発端メッセージへ可視化する（R3）。
                    // エラー本文はチャンネルへ出さない（wire にも載っていない・#668）。
                    react_system(&transport, &reply_origin, &reactions.failed).await;
                    // typing: ターン失敗で進行中の typing を止める（ended が来ない経路でも打ちっぱなしにしない）。
                    typing = None;
                }
                Some(LiveEvent::Error { .. }) | None => {
                    // ターンが壊れた/接続が切れた。進行中の typing を止める（打ちっぱなしにしない）。
                    typing = None;
                    tokio::time::sleep(RETRY).await;
                }
            }
        }
    })
}

mod model_command;
#[cfg(test)]
mod silent_origin_tests;
mod thinking_command;
mod webhook_command;

#[cfg(test)]
mod caller_tests;

#[cfg(test)]
#[path = "run/webhook_command_tests.rs"]
mod webhook_command_tests;

#[cfg(test)]
mod model_command_route_tests;

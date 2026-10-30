//! 1 instance の UDS client と watch lane。
//!
//! watch は bind ack の後にだけ起動する（DESIGN-NOSTRGATE §6 #16）。
//! 切断で child を止め、読取済み未送信は破棄する。再送はしない。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use opencrab_gate_client::client::{InstanceClient, LiveEvent, PostRefuse, SaidOutcome};
use opencrab_gate_client::wire::{LiveInboundScope, SaidContext};
use opencrab_gate_client::{FinalDelivery, RuntimeCapabilities, SayPolicy};

use crate::config::{parse_instance_config, AccessConfig, InstancePlacement};
use crate::dedup::SeenEvents;
use crate::follows::SharedAccess;
use crate::harness::HarnessOverrides;
use crate::map::{
    bundle_id, classify_route, map_event, normalize_author_id, parse_watch_line, BundlePlace, Lane,
    LaneKind, Route, WatchEvent,
};
use crate::post::{self, SayDelivery};
use crate::run_env::{self, SecretOwner};

const BIND_POLL: Duration = Duration::from_millis(50);

/// 車線をまたぐ dedup TTL の下限。最大 watch interval を上回れないと、即時送出した
/// メンションと interval 後に flush される bundle の間で取りこぼす。
const DEDUP_TTL_FLOOR: Duration = Duration::from_secs(600);

/// 最大 watch interval の 2 倍と下限の大きい方＋余裕。bundle flush 窓を確実に跨ぐ。
fn dedup_ttl(max_interval_secs: i64) -> Duration {
    let max_interval = max_interval_secs.max(0) as u64;
    let by_interval = Duration::from_secs(max_interval.saturating_mul(2).saturating_add(60));
    by_interval.max(DEDUP_TTL_FLOOR)
}

#[derive(Default)]
pub(crate) struct SaidMetrics {
    store_error: AtomicU64,
    bad_request: AtomicU64,
    queue_full: AtomicU64,
    bundle_discarded: AtomicU64,
    deduped: AtomicU64,
    /// watch 車線が自分宛て #p を default 車線へ譲って捨てた回数（QC #10 対策・Defect A）。
    lane_deferred: AtomicU64,
    /// say → nostaro reply を投稿できた回数。
    say_posted: AtomicU64,
    /// say 投稿が失敗した回数（nostaro 非ゼロ / spawn 失敗 / timeout）。
    say_post_failed: AtomicU64,
    /// 返信先が無い（bundle/曖昧）say を新規ノート（standalone）として publish した回数（row292）。
    say_posted_standalone: AtomicU64,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_instance(
    socket: PathBuf,
    place: &InstancePlacement,
    config_bytes: &[u8],
    secret: Option<Arc<String>>,
    secret_owner: SecretOwner,
    nostaro_bin: PathBuf,
    overrides: HarnessOverrides,
    attachment_outbox_root: Option<PathBuf>,
) -> anyhow::Result<Arc<InstanceClient>> {
    if overrides.is_active() {
        tracing::warn!(
            fake_watch = ?overrides.fake_watch,
            dry_run = overrides.dry_run,
            instance = %place.instance_id,
            "QC harness overrides ACTIVE — this is NOT a production path"
        );
    }
    let cfg = parse_instance_config(config_bytes)?;
    // 返信本文を nostaro reply で投稿するための relays だけの config（鍵は env 注入・config に載せない）。
    let post_config = post::post_config_path(&socket, &place.instance_id);
    post::write_relays_config(&post_config, &cfg.relays).map_err(|e| {
        anyhow::anyhow!(
            "nostaro post config を書けない ({}): {e}",
            post_config.display()
        )
    })?;
    // DI 能力宣言（§9.2）と invoke handler を hello に載せて接続する。invoke は nostaro CLI 実行へ
    // 写す（reply/reaction/repost/follow/unfollow/kind0/upload/resolve）。秘密鍵は env 注入のみ。
    // Nostr は単一契約: 本文だけなら通常配送し、指定投稿への操作が必要な時だけ tool を使う。
    let (operations, runtime_capabilities) = runtime_contract(
        attachment_outbox_root
            .as_ref()
            .is_some_and(|p| p.is_absolute()),
    );
    // D-1092: watch（タイムライン定期取得）はゲートウェイの状態。保存ファイルはこの instance
    // 専用のパスに置き、無ければ配置の `watches` を種にして 1 度だけ書き出す。
    let watches = crate::watch_store::WatchRegistry::load(
        crate::watch_store::store_path(&post_config, &place.instance_id),
        &cfg.watches,
    )?;
    let invoke_handler: Arc<dyn opencrab_gate_client::InvokeHandler> = Arc::new(
        crate::ops::NostrInvokeHandler::new(
            nostaro_bin.clone(),
            post_config.clone(),
            secret.as_ref().map(|s| s.as_str().to_string()),
            overrides.dry_run,
        )
        .with_attachments(attachment_outbox_root, cfg.self_pubkey.clone())
        .with_watches(watches.clone()),
    );
    // D-1091: この binding のターンで `execute_shell` が起動する子へ渡す環境を宣言する。
    // relays-only config は instance ごとのパス。鍵は「このプロセスがこの instance だけを持つ」
    // と分かるときだけ載せる（複数 instance の配置では誰にも載せない）。ログには名前だけを出す。
    let run_env = run_env::for_instance(
        &post_config,
        secret_owner,
        secret.as_ref().map(|s| s.as_str()),
    );
    if !secret_owner.declares_secret() && secret.is_some() {
        tracing::warn!(
            instance = %place.instance_id,
            declared = ?run_env::declared_env_names(&run_env),
            "placement holds several instances; the process credential is not declared to any of them"
        );
    }
    tracing::debug!(
        instance = %place.instance_id,
        declared = ?run_env::declared_env_names(&run_env),
        "binding env declared"
    );
    let client = InstanceClient::spawn_with_operations_and_env(
        socket,
        place.instance_id.clone(),
        place.agent_id.clone(),
        cfg.self_pubkey.clone(),
        SayPolicy::AcceptToLiveQueue,
        operations,
        runtime_capabilities,
        invoke_handler,
        run_env,
    );
    let metrics = Arc::new(SaidMetrics::default());
    // 車線をまたぐ said dedup は instance 単位で共有する（全 default/watch 車線が同じセット）。
    // D-1092: watch の正本は保存ファイル。間隔はエージェントや設定画面が変えうるので、
    // 許される最大間隔で dedup 窓を取る（間隔を延ばした後に重複送出しない）。
    let seen = Arc::new(SeenEvents::new(dedup_ttl(
        watches
            .max_interval_secs()
            .max(crate::watch_store::MAX_INTERVAL_SECS),
    )));
    watches.spawn_reload_poller(crate::watch_store::RELOAD_POLL);
    supervise_lanes(
        client.clone(),
        place.address.clone(),
        cfg,
        watches,
        secret,
        nostaro_bin,
        post_config.clone(),
        crate::follows::following_out_path(&post_config, &place.instance_id),
        metrics,
        seen,
        overrides,
    );
    Ok(client)
}

fn runtime_contract(files_enabled: bool) -> (Option<serde_json::Value>, RuntimeCapabilities) {
    (
        Some(crate::ops::operation_declarations_with_files(files_enabled)),
        RuntimeCapabilities {
            final_delivery: FinalDelivery::OperationDriven,
        },
    )
}

/// core からの say を live queue から取り出し、発端イベントへの e-tag reply として nostaro で
/// 投稿する。返信先があれば e-tag reply、無ければ（bundle/曖昧）新規ノート（standalone post）で
/// publish する（row292/#843: drop しない）。
fn spawn_say_consumer(
    client: Arc<InstanceClient>,
    address: String,
    nostaro_bin: PathBuf,
    post_config: PathBuf,
    secret: Option<Arc<String>>,
    metrics: Arc<SaidMetrics>,
    dry_run: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match client.next_live(&address).await {
                Some(LiveEvent::Message {
                    text, reply_origin, ..
                }) => {
                    let secret_ref = secret.as_ref().map(|s| s.as_str());
                    match post::deliver_say(
                        &nostaro_bin,
                        &post_config,
                        secret_ref,
                        reply_origin,
                        &text,
                        dry_run,
                    )
                    .await
                    {
                        SayDelivery::Posted => {
                            let n = metrics.say_posted.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::info!(address = %address, say_posted = n, "say posted as reply");
                        }
                        SayDelivery::PostedStandalone => {
                            let n = metrics
                                .say_posted_standalone
                                .fetch_add(1, Ordering::Relaxed)
                                + 1;
                            tracing::info!(
                                address = %address,
                                say_posted_standalone = n,
                                "say posted as standalone (no single reply target)"
                            );
                        }
                        SayDelivery::Failed(e) => {
                            let n = metrics.say_post_failed.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::warn!(address = %address, say_post_failed = n, error = %e, "say post failed");
                        }
                    }
                }
                // 切断。少し待って再試行（再接続後 next_live が再びブロックする）。
                Some(LiveEvent::Error { .. }) | None => {
                    tokio::time::sleep(BIND_POLL).await;
                }
                // Activity / CompletedNoReply は投稿対象ではない。
                Some(_) => {}
            }
        }
    })
}

/// D-1099: core needs no registration; the lanes run while the core connection is up.
async fn wait_until_connected(client: &InstanceClient) {
    while !client.connection_live().await {
        tokio::time::sleep(BIND_POLL).await;
    }
}

async fn wait_until_disconnected(client: &InstanceClient) {
    while client.connection_live().await {
        tokio::time::sleep(BIND_POLL).await;
    }
}

fn snapshot(access: &SharedAccess) -> AccessConfig {
    access.read().unwrap_or_else(|e| e.into_inner()).clone()
}

#[allow(clippy::too_many_arguments)]
async fn handle_line(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    access: &AccessConfig,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
    metrics: &SaidMetrics,
    seen: &SeenEvents,
    line: String,
) {
    let Some(event) = parse_watch_line(&line) else {
        return;
    };
    if crate::admission::admit(&event, self_pubkey, access).is_none() {
        tracing::debug!(id = %event.id, "event dropped by gateway admission");
        return;
    }
    // Defect A（QC #10）: 自分宛て #p メンション/リプライは default(mention) 車線が `--npub` で
    // 必ず即時に拾う。watch 車線が同じイベントを先に握っても、ここで default 車線へ譲って捨てる。
    // これで owner がフォロイーにも含まれるときのレースで watch origin に取り込まれ、権限デバウンスへ
    // 降格する取りこぼしを防ぐ（default 車線の即時処理へ一本化する）。
    if should_defer_to_default_lane(lane, &event, self_pubkey) {
        let n = metrics.lane_deferred.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::debug!(
            id = %event.id,
            lane_deferred = n,
            "watch lane defers self-mention to default lane"
        );
        return;
    }
    let route = classify_route(&event, self_pubkey, beyond, lane);
    if route == Route::Bundle {
        // bundle は flush 時に dedup する（manifest の count/origins を一貫させるため）。
        pending.lock().await.push(event);
        return;
    }
    // 即時送出はここで event_id を握る（二重 said 回避）。ただし core が Accepted しなければ
    // claim を解除し、別車線 copy が後追いで届くようにする（claim-after-accept・#839 NIT の実害化対策）。
    if !claim_or_skip(seen, metrics, &event.id) {
        return;
    }
    let accepted = send_mapped(
        client,
        address,
        lane,
        self_pubkey,
        access,
        beyond,
        &event,
        None,
        metrics,
    )
    .await;
    if !accepted {
        if let Some(id) = normalize_author_id(&event.id) {
            seen.release(&id);
        }
    }
}

/// Defect A（QC #10）: watch(timeline) 車線が受けたイベントを default(mention) 車線へ譲るか。
///
/// 自分宛て `#p` のメンション/リプライは、default 車線が `mention_lane_filter` の `--npub` で必ず即時に
/// 拾う。owner がフォロイーにも含まれると同じイベントが両車線へ届き、watch 車線が先に握ると watch origin
/// で取り込まれて権限デバウンスへ降格する（即応の取りこぼし）。watch 車線側をここで捨てて default 車線の
/// 即時処理へ一本化する。default 車線・自分宛てでないイベントは対象外。
fn should_defer_to_default_lane(lane: &Lane, event: &WatchEvent, self_pubkey: &str) -> bool {
    matches!(lane.kind, LaneKind::Watch { .. })
        && crate::map::is_self_p_tag_mention(event, self_pubkey)
}

/// event_id を握れたら `true`。hex 化できる id のみ dedup 対象（それ以外は send_mapped が落とす）。
/// 既に握られていたら dedup メトリクスを上げて `false` を返す。
fn claim_or_skip(seen: &SeenEvents, metrics: &SaidMetrics, raw_event_id: &str) -> bool {
    let Some(event_id) = normalize_author_id(raw_event_id) else {
        // 非 hex は dedup キーにできない。send_mapped 側の map_event が落とす。
        return true;
    };
    if seen.claim(&event_id) {
        return true;
    }
    let n = metrics.deduped.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(event_id = %event_id, deduped = n, "said skipped; event already handled by another lane");
    false
}

struct BundleWindow {
    events: Vec<WatchEvent>,
    discarded: usize,
}

/// interval 内のイベントを created_at / id 順に並べ、新しい方から `max_items` 件残す。
fn take_bundle_window(mut events: Vec<WatchEvent>, max_items: usize) -> BundleWindow {
    events.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let discarded = events.len().saturating_sub(max_items);
    if discarded > 0 {
        events.drain(..discarded);
    }
    BundleWindow { events, discarded }
}

fn record_bundle_discarded(metrics: &SaidMetrics, discarded: usize, kept: usize, max_items: usize) {
    if discarded == 0 {
        return;
    }
    let n = metrics
        .bundle_discarded
        .fetch_add(discarded as u64, Ordering::Relaxed)
        + discarded as u64;
    tracing::warn!(
        discarded,
        kept,
        max_items,
        bundle_discarded = n,
        "bundle trimmed; older items dropped"
    );
}

#[allow(clippy::too_many_arguments)]
async fn flush_bundle(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    access: &AccessConfig,
    beyond: bool,
    pending: &tokio::sync::Mutex<Vec<WatchEvent>>,
    metrics: &SaidMetrics,
    seen: &SeenEvents,
    max_items: usize,
) {
    let events = {
        let mut g = pending.lock().await;
        std::mem::take(&mut *g)
    };
    if events.is_empty() {
        return;
    }
    let window = take_bundle_window(events, max_items);
    record_bundle_discarded(metrics, window.discarded, window.events.len(), max_items);
    // 別車線（default 即時など）が既に送ったイベントを manifest 構築の前に除く。
    // ここで削ると bundle_id / origins / count が実送信分と一致する。
    let events: Vec<WatchEvent> = window
        .events
        .into_iter()
        .filter(|e| claim_or_skip(seen, metrics, &e.id))
        .collect();
    if events.is_empty() {
        return;
    }
    let Some(watch_id) = lane.watch_id() else {
        tracing::warn!("bundle dropped; default lane has no watch_id");
        return;
    };
    if !client.connection_live().await {
        tracing::info!(count = events.len(), "bundle dropped; core not connected");
        return;
    }
    let ids: Vec<String> = events
        .iter()
        .filter_map(|e| crate::map::normalize_author_id(&e.id))
        .collect();
    if ids.len() != events.len() {
        tracing::warn!(
            kept = ids.len(),
            total = events.len(),
            "bundle dropped; event id not hex"
        );
        return;
    }
    let bundle = bundle_id(address, watch_id, &ids);
    let count = events.len() as u32;
    let lane_for_origin = Lane::watch(watch_id);
    let origins: Vec<String> = ids
        .iter()
        .map(|id| crate::map::decisive_origin(&lane_for_origin, id))
        .collect();
    for (i, event) in events.iter().enumerate() {
        let place = BundlePlace {
            bundle_id: bundle.clone(),
            index: (i as u32) + 1,
            count,
            origins: origins.clone(),
        };
        let accepted = send_mapped(
            client,
            address,
            lane,
            self_pubkey,
            access,
            beyond,
            event,
            Some(&place),
            metrics,
        )
        .await;
        if !accepted {
            if let Some(id) = crate::map::normalize_author_id(&event.id) {
                seen.release(&id);
            }
        }
    }
}

/// said を送る。`true` は core が **Accepted** した場合のみ（claim を保持してよい）。
/// それ以外（WireErr=bad_request 等 / NotAdmitted / NotReady / Busy / map 失敗）は `false` を返し、
/// 呼び出し側が claim を解除して別車線 copy の取りこぼしを防ぐ（claim-after-accept・#839 NIT）。
#[allow(clippy::too_many_arguments)]
async fn send_mapped(
    client: &InstanceClient,
    address: &str,
    lane: &Lane,
    self_pubkey: &str,
    access: &AccessConfig,
    beyond: bool,
    event: &WatchEvent,
    bundle: Option<&BundlePlace>,
    metrics: &SaidMetrics,
) -> bool {
    let Some(caller) = crate::admission::admit(event, self_pubkey, access) else {
        tracing::debug!(id = %event.id, "said dropped by gateway admission");
        return true;
    };
    let Some(mapped) = map_event(event, self_pubkey, beyond, lane, bundle) else {
        tracing::warn!(id = %event.id, "said dropped; author or event id is not hex");
        return false;
    };
    let context = SaidContext {
        caller,
        start_turn: bundle.is_none_or(|place| place.index == place.count),
        system_context: None,
        reply_target: mapped.reply_target.clone(),
        // D-1099: the event id is what replies and eN references point at.
        reference: Some(event.id.clone()),
        live_inbound_scope: LiveInboundScope::Speaker,
        model_profile: opencrab_gate_client::wire::TurnModelProfile::Default,
        agent_prelude: None,
        provisional_id: None,
    };
    let post = client
        .post_said_with_context(
            address,
            &mapped.origin,
            &mapped.author_id,
            None,
            &context,
            &mapped.text,
            &mapped.attachments,
        )
        .await;
    match post {
        Ok(outcome) => {
            record_said_outcome(metrics, &mapped.origin, &outcome);
            matches!(outcome, SaidOutcome::Accepted { .. })
        }
        Err(PostRefuse::NotReady) => {
            tracing::info!(origin = %mapped.origin, "said dropped; binding not ready");
            false
        }
        Err(PostRefuse::Busy) => {
            let n = metrics.queue_full.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(origin = %mapped.origin, queue_full = n, "said refused; binding busy");
            false
        }
    }
}

fn record_said_outcome(metrics: &SaidMetrics, origin: &str, outcome: &SaidOutcome) {
    match outcome {
        SaidOutcome::Accepted { seq } => {
            tracing::info!(origin, seq, "said accepted");
        }
        SaidOutcome::NotAdmitted => {
            tracing::info!(origin, "said not admitted");
        }
        SaidOutcome::Disconnected => {
            tracing::info!(origin, "said disconnected");
        }
        SaidOutcome::Opened { .. } => {
            tracing::warn!(origin, "said answered as open");
        }
        SaidOutcome::WireErr { code, detail } => {
            if code == "store_error" {
                let n = metrics.store_error.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(origin, code, ?detail, store_error = n, "said wire err");
            } else if code == "bad_request" {
                let n = metrics.bad_request.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(origin, code, ?detail, bad_request = n, "said wire err");
            } else {
                tracing::warn!(origin, code, ?detail, "said wire err");
            }
        }
    }
}

mod lanes;

#[cfg(test)]
pub(crate) use lanes::plan_lane_spawns;
pub(crate) use lanes::supervise_lanes;

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;

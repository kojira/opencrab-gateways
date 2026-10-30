//! D-1092: instance の車線（メンション車線 + watch 車線）の起動と監督。
//!
//! 車線は「bind されている間だけ」動く。watch の設定が変わったら**watch 車線だけ**を作り直し、
//! say consumer とフォロー反映は動かし続ける（車線の再起動で say の取りこぼしを作らない）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::config::{
    mention_lane_filter, watches_beyond_self, AccessConfig, InstanceConfig, WatchFilter,
    DEFAULT_BUNDLE_MAX_ITEMS,
};
use crate::follows::SharedAccess;
use crate::harness::HarnessOverrides;
use crate::map::{Lane, LaneKind, WatchEvent};
use crate::watch::{run_fake_watch_once, run_watch_loop, RESUBSCRIBE};
use crate::watch_store::{WatchRegistry, WatchSetting};

use super::{
    flush_bundle, handle_line, snapshot, spawn_say_consumer, wait_until_connected,
    wait_until_disconnected, InstanceClient, SaidMetrics, SeenEvents,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn supervise_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    watches: Arc<WatchRegistry>,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    post_config: PathBuf,
    following_out: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
    overrides: HarnessOverrides,
) {
    // admission が読む access。store 由来の固定分にフォローリスト（kind:3）を定期反映する。
    let base_access = cfg.access.clone();
    let access: SharedAccess = Arc::new(std::sync::RwLock::new(base_access.clone()));
    tokio::spawn(async move {
        loop {
            wait_until_connected(&client).await;
            tracing::info!(address = %address, "core connected; starting watch");
            let cancel = Arc::new(Notify::new());
            if overrides.fake_watch.is_none() {
                start_follow_refresher(
                    &access,
                    &base_access,
                    &nostaro_bin,
                    &post_config,
                    secret.clone(),
                    &cfg.self_pubkey,
                    &following_out,
                );
            }
            // core からの say（返信本文）を消費して nostaro post で publish する consumer。
            // watch の変更では作り直さない（車線の再起動で say の取りこぼしを作らない）。
            let mut side_handles = vec![spawn_say_consumer(
                client.clone(),
                address.clone(),
                nostaro_bin.clone(),
                post_config.clone(),
                secret.clone(),
                metrics.clone(),
                overrides.dry_run,
            )];
            // D-1092: watch の設定が変わったら、その車線だけ作り直す。say consumer と
            // フォロー反映はそのまま動かし続ける。
            let mut bound = true;
            loop {
                let mut lane_handles = start_lanes(
                    client.clone(),
                    address.clone(),
                    cfg.clone(),
                    watches.list(),
                    secret.clone(),
                    nostaro_bin.clone(),
                    metrics.clone(),
                    seen.clone(),
                    cancel.clone(),
                    overrides.fake_watch.clone(),
                    access.clone(),
                );
                let reloaded = tokio::select! {
                    () = wait_until_disconnected(&client) => {
                        bound = false;
                        false
                    }
                    () = watches.changed() => true,
                };
                cancel.notify_waiters();
                for handle in lane_handles.drain(..) {
                    handle.abort();
                }
                if !bound {
                    break;
                }
                if reloaded {
                    tracing::info!(address = %address, "watch settings changed; restarting lanes");
                }
            }
            tracing::info!(address = %address, "binding lost; stopping watch");
            for handle in side_handles.drain(..) {
                handle.abort();
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn start_follow_refresher(
    access: &SharedAccess,
    base_access: &AccessConfig,
    nostaro_bin: &Path,
    post_config: &Path,
    secret: Option<Arc<String>>,
    self_pubkey: &str,
    following_out: &Path,
) {
    tokio::spawn(crate::follows::spawn_refresher(
        access.clone(),
        base_access.clone(),
        nostaro_bin.to_path_buf(),
        post_config.to_path_buf(),
        secret,
        self_pubkey.to_string(),
        following_out.to_path_buf(),
    ));
}

pub(crate) struct LaneSpawn {
    pub(crate) lane: Lane,
    pub(crate) filter: WatchFilter,
    /// watch 車線だけが持つ (間隔, 1 間隔の上限件数)。
    pub(crate) flush: Option<(i64, usize)>,
}

pub(crate) fn plan_lane_spawns(cfg: &InstanceConfig, watches: &[WatchSetting]) -> Vec<LaneSpawn> {
    let mut planned = Vec::with_capacity(1 + watches.len());
    planned.push(LaneSpawn {
        lane: Lane::default_lane(),
        filter: mention_lane_filter(cfg),
        flush: None,
    });
    for watch in watches {
        planned.push(LaneSpawn {
            lane: Lane::watch(watch.id),
            filter: watch.filter(),
            flush: Some((watch.interval_secs, DEFAULT_BUNDLE_MAX_ITEMS as usize)),
        });
    }
    planned
}

#[allow(clippy::too_many_arguments)]
fn start_lanes(
    client: Arc<InstanceClient>,
    address: String,
    cfg: InstanceConfig,
    watches: Vec<WatchSetting>,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
    cancel: Arc<Notify>,
    fake_watch: Option<PathBuf>,
    access: SharedAccess,
) -> Vec<tokio::task::JoinHandle<()>> {
    plan_lane_spawns(&cfg, &watches)
        .into_iter()
        .map(|planned| {
            spawn_lane(
                client.clone(),
                address.clone(),
                planned.lane,
                cfg.relays.clone(),
                planned.filter,
                cfg.self_pubkey.clone(),
                access.clone(),
                planned.flush,
                secret.clone(),
                nostaro_bin.clone(),
                metrics.clone(),
                seen.clone(),
                cancel.clone(),
                fake_watch.clone(),
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn spawn_lane(
    client: Arc<InstanceClient>,
    address: String,
    lane: Lane,
    relays: Vec<String>,
    filter: WatchFilter,
    self_pubkey: String,
    access: SharedAccess,
    flush_spec: Option<(i64, usize)>,
    secret: Option<Arc<String>>,
    nostaro_bin: PathBuf,
    metrics: Arc<SaidMetrics>,
    seen: Arc<SeenEvents>,
    cancel: Arc<Notify>,
    fake_watch: Option<PathBuf>,
) -> tokio::task::JoinHandle<()> {
    let beyond = match lane.kind {
        LaneKind::Default => false,
        LaneKind::Watch { .. } => watches_beyond_self(&filter),
    };
    let flush = flush_spec.map(|(interval, max_items)| {
        (
            Duration::from_secs(interval.max(1) as u64),
            max_items.max(1),
        )
    });
    tokio::spawn(async move {
        let pending = Arc::new(tokio::sync::Mutex::new(Vec::<WatchEvent>::new()));
        // 1 行受信ごとに handle_line を回すコールバック（実 watch・偽 watch 共通）。
        let on_line = {
            let pending = pending.clone();
            let client = client.clone();
            let address = address.clone();
            let lane = lane.clone();
            let self_pubkey = self_pubkey.clone();
            let metrics = metrics.clone();
            let seen = seen.clone();
            let access = access.clone();
            move |line: String| {
                let pending = pending.clone();
                let client = client.clone();
                let address = address.clone();
                let lane = lane.clone();
                let self_pubkey = self_pubkey.clone();
                let metrics = metrics.clone();
                let seen = seen.clone();
                let access = snapshot(&access);
                tokio::spawn(async move {
                    handle_line(
                        &client,
                        &address,
                        &lane,
                        &self_pubkey,
                        &access,
                        beyond,
                        &pending,
                        &metrics,
                        &seen,
                        line,
                    )
                    .await;
                });
            }
        };
        let watch_fut = async {
            // QC ハーネス: fake_watch が指定されていれば nostaro を spawn せず fixture を流す。
            // 指定が無ければ（production）従来どおり実 nostaro watch を回す。
            if let Some(fixture) = fake_watch {
                if let Err(e) = run_fake_watch_once(&fixture, on_line).await {
                    tracing::error!(error = %e, "fake watch failed");
                }
            } else {
                run_watch_loop(nostaro_bin, relays, filter, secret, RESUBSCRIBE, on_line).await;
            }
        };
        let flush_fut = async {
            let Some((interval, max_items)) = flush else {
                std::future::pending::<()>().await;
                return;
            };
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                flush_bundle(
                    &client,
                    &address,
                    &lane,
                    &self_pubkey,
                    &snapshot(&access),
                    beyond,
                    &pending,
                    &metrics,
                    &seen,
                    max_items,
                )
                .await;
            }
        };
        tokio::select! {
            _ = watch_fut => {}
            _ = flush_fut => {}
            _ = cancel.notified() => {
                pending.lock().await.clear();
            }
        }
    })
}

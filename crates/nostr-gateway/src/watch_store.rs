//! エージェントが自分で設定する watch（タイムライン定期取得）の保存と適用。
//!
//! watch はゲートウェイの状態なので、**保存先もゲートウェイ**に置く（core の DB には持たない）。
//! 1 instance = 1 JSON ファイル（mode 0600）で、配置（placement）の隣に置く。
//!
//! # 正本の移行
//!
//! 起動時、保存ファイルが無ければ**配置の `watches` を種として 1 度だけ書き出す**。以降の正本は
//! このファイルで、`watch_set` / `watch_delete` はファイルと実行中の車線の両方を更新する。
//! これで「配置に書いた分」と「エージェントが設定した分」が二重の正本にならない。
//!
//! # 変更の反映
//!
//! 保存だけして終わると次の再起動まで効かないので、[`WatchRegistry::changed`] を鳴らし、
//! 車線の監督ループが watch 車線を作り直す（[`crate::run`]）。
//!
//! # 検証
//!
//! 間隔・kinds・件数の範囲はここ 1 箇所だけが持つ。範囲外は**状態を変えずに**拒否する
//! （書いてから直す形にすると、途中状態が配置に残る）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::WatchPlacement;

/// 間隔の下限（秒）。これ未満はリレーへの連投になるので拒否する。
pub const MIN_INTERVAL_SECS: i64 = 60;
/// 間隔の上限（秒・24 時間）。
pub const MAX_INTERVAL_SECS: i64 = 86_400;
/// 1 watch の `authors` 上限。
pub const MAX_AUTHORS: usize = 2_000;
/// 1 watch の `keywords` 上限。
pub const MAX_KEYWORDS: usize = 32;
/// 指定できる kinds（DM とリアクション以外の通常ノート・リポスト）。
pub const ALLOWED_KINDS: &[u32] = &[1, 6, 7];
/// 新規 watch の既定 kinds。
const DEFAULT_KINDS: &[u32] = &[1];

/// エージェントが設定した watch 1 件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchSetting {
    pub id: i64,
    pub interval_secs: i64,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub kinds: Vec<u32>,
}

impl WatchSetting {
    /// 車線が使うフィルタ。`kinds` が空なら既定（`[1]`）。
    pub fn filter(&self) -> crate::config::WatchFilter {
        crate::config::WatchFilter {
            authors: self.authors.clone(),
            keywords: self.keywords.clone(),
            kinds: if self.kinds.is_empty() {
                DEFAULT_KINDS.to_vec()
            } else {
                self.kinds.clone()
            },
            npub: None,
        }
    }
}

/// 保存ファイル 1 つ分。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchFile {
    #[serde(default)]
    watches: Vec<WatchSetting>,
}

/// `watch_set` の適用内容（検証済み）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSetRequest {
    /// 既存 id を更新するときだけ `Some`。`None` は新規採番。
    pub id: Option<i64>,
    pub interval_secs: i64,
    pub authors: Vec<String>,
    pub keywords: Vec<String>,
    pub kinds: Vec<u32>,
}

/// 設定の拒否理由（呼び出し元へそのまま返す文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchReject(pub String);

fn reject(message: impl Into<String>) -> WatchReject {
    WatchReject(message.into())
}

fn string_list(value: Option<&Value>, field: &str) -> Result<Vec<String>, WatchReject> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Some(items) = value.as_array() else {
        return Err(reject(format!("{field} は文字列の配列で指定してください")));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(reject(format!("{field} の要素は文字列で指定してください")));
        };
        let text = text.trim();
        if !text.is_empty() {
            out.push(text.to_string());
        }
    }
    Ok(out)
}

/// `watch_set` の payload を検証して適用内容にする。範囲外は状態を変えずに拒否する。
pub fn parse_set_request(payload: &Value) -> Result<WatchSetRequest, WatchReject> {
    let interval_secs = payload
        .get("interval_secs")
        .and_then(Value::as_i64)
        .ok_or_else(|| reject("interval_secs（秒）を整数で指定してください"))?;
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&interval_secs) {
        return Err(reject(format!(
            "interval_secs は {MIN_INTERVAL_SECS}〜{MAX_INTERVAL_SECS} の範囲で指定してください"
        )));
    }
    let authors = string_list(payload.get("authors"), "authors")?;
    if authors.len() > MAX_AUTHORS {
        return Err(reject(format!(
            "authors は最大 {MAX_AUTHORS} 件です（{} 件指定されました）",
            authors.len()
        )));
    }
    let keywords = string_list(payload.get("keywords"), "keywords")?;
    if keywords.len() > MAX_KEYWORDS {
        return Err(reject(format!(
            "keywords は最大 {MAX_KEYWORDS} 件です（{} 件指定されました）",
            keywords.len()
        )));
    }
    let mut kinds = Vec::new();
    let kinds_given = payload.get("kinds").filter(|v| !v.is_null());
    if let Some(value) = kinds_given {
        let Some(items) = value.as_array() else {
            return Err(reject("kinds は整数の配列で指定してください"));
        };
        for item in items {
            let Some(kind) = item.as_u64() else {
                return Err(reject("kinds の要素は整数で指定してください"));
            };
            let kind = u32::try_from(kind).map_err(|_| reject("kinds の値が大きすぎます"))?;
            if !ALLOWED_KINDS.contains(&kind) {
                return Err(reject(format!(
                    "kinds に {kind} は指定できません（{:?} のいずれか）",
                    ALLOWED_KINDS
                )));
            }
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        }
        if kinds.is_empty() {
            return Err(reject(format!(
                "kinds を空にはできません（{:?} のいずれか）",
                ALLOWED_KINDS
            )));
        }
    }
    if kinds.is_empty() {
        kinds = DEFAULT_KINDS.to_vec();
    }
    let id = match payload.get("id").filter(|v| !v.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_i64()
                .ok_or_else(|| reject("id は整数で指定してください"))?,
        ),
    };
    Ok(WatchSetRequest {
        id,
        interval_secs,
        authors,
        keywords,
        kinds,
    })
}

/// 保存先パス。nostaro の post config と同じディレクトリ（socket の隣）に instance ごと 1 ファイル。
///
/// 配置ファイルの隣にしないのは、配置が複数 instance を 1 ファイルに持つことがあり、書き戻すと
/// 他の instance の設定まで触ってしまうため。instance 専用のパスに閉じる。
pub fn store_path(post_config: &Path, instance_id: &str) -> PathBuf {
    post_config
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("nostaro-watches-{instance_id}.json"))
}

/// 保存ファイルを読む。無ければ配置の `watches` を種にして書き出す（1 度だけの移行）。
pub fn load_or_seed(
    path: &Path,
    placement_watches: &[WatchPlacement],
) -> anyhow::Result<Vec<WatchSetting>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let file: WatchFile = serde_json::from_str(&text)?;
            Ok(file.watches)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let seeded: Vec<WatchSetting> = placement_watches
                .iter()
                .map(|watch| {
                    let filter = watch.effective_filter();
                    WatchSetting {
                        id: watch.id,
                        interval_secs: watch.interval_secs,
                        authors: filter.authors.clone(),
                        keywords: filter.keywords.clone(),
                        kinds: filter.kinds.clone(),
                    }
                })
                .collect();
            save(path, &seeded)?;
            tracing::info!(
                path = %path.display(),
                seeded = seeded.len(),
                "watch store created from the placement (one-time migration)"
            );
            Ok(seeded)
        }
        Err(error) => Err(error.into()),
    }
}

fn mtime_of(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// 保存ファイルを書く（一時ファイル + rename・mode 0600）。
pub fn save(path: &Path, watches: &[WatchSetting]) -> anyhow::Result<()> {
    let body = serde_json::to_vec_pretty(&WatchFile {
        watches: watches.to_vec(),
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&tmp)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&tmp, permissions)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 実行中の watch 集合。変更は [`WatchRegistry::changed`] で車線へ伝える。
pub struct WatchRegistry {
    path: PathBuf,
    watches: Mutex<Vec<WatchSetting>>,
    /// 最後に読んだ/書いたファイルの mtime。設定画面（別プロセス）や手編集の反映に使う。
    seen_mtime: Mutex<Option<std::time::SystemTime>>,
    changed: tokio::sync::Notify,
}

impl WatchRegistry {
    /// 保存済みの集合で作る（無ければ配置から種を作る）。
    pub fn load(path: PathBuf, placement_watches: &[WatchPlacement]) -> anyhow::Result<Arc<Self>> {
        let watches = load_or_seed(&path, placement_watches)?;
        Ok(Arc::new(Self {
            seen_mtime: Mutex::new(mtime_of(&path)),
            path,
            watches: Mutex::new(watches),
            changed: tokio::sync::Notify::new(),
        }))
    }

    /// テスト用: 保存もしない空の集合。
    #[cfg(test)]
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self {
            path: PathBuf::new(),
            watches: Mutex::new(Vec::new()),
            seen_mtime: Mutex::new(None),
            changed: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<WatchSetting>> {
        self.watches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 現在の watch 集合（id 昇順）。
    pub fn list(&self) -> Vec<WatchSetting> {
        let mut watches = self.lock().clone();
        watches.sort_by_key(|w| w.id);
        watches
    }

    /// 追加または更新する。保存に成功したときだけ実行中の集合を差し替える。
    pub fn set(&self, request: WatchSetRequest) -> Result<WatchSetting, WatchReject> {
        self.reload_if_changed();
        let mut watches = self.lock();
        let id = request.id.unwrap_or_else(|| {
            watches
                .iter()
                .map(|w| w.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1)
        });
        let setting = WatchSetting {
            id,
            interval_secs: request.interval_secs,
            authors: request.authors,
            keywords: request.keywords,
            kinds: request.kinds,
        };
        let mut next: Vec<WatchSetting> = watches.iter().filter(|w| w.id != id).cloned().collect();
        next.push(setting.clone());
        next.sort_by_key(|w| w.id);
        self.persist(&next)?;
        *watches = next;
        drop(watches);
        self.changed.notify_one();
        Ok(setting)
    }

    /// id 指定で削除する。消えたら `true`。
    pub fn delete(&self, id: i64) -> Result<bool, WatchReject> {
        self.reload_if_changed();
        let mut watches = self.lock();
        let next: Vec<WatchSetting> = watches.iter().filter(|w| w.id != id).cloned().collect();
        if next.len() == watches.len() {
            return Ok(false);
        }
        self.persist(&next)?;
        *watches = next;
        drop(watches);
        self.changed.notify_one();
        Ok(true)
    }

    fn persist(&self, watches: &[WatchSetting]) -> Result<(), WatchReject> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        save(&self.path, watches)
            .map_err(|e| reject(format!("watch の保存に失敗しました: {e}")))?;
        *self.seen_mtime.lock().unwrap_or_else(|e| e.into_inner()) = mtime_of(&self.path);
        Ok(())
    }

    /// 保存ファイルが外から書き換えられていたら読み直す（設定画面・手編集）。
    ///
    /// 変わっていたら `true` を返し、車線へ変更を知らせる。ファイルが消えた・読めないときは
    /// 現在の集合を保つ（設定を失わない）。
    pub fn reload_if_changed(&self) -> bool {
        if self.path.as_os_str().is_empty() {
            return false;
        }
        let current = mtime_of(&self.path);
        {
            let seen = self.seen_mtime.lock().unwrap_or_else(|e| e.into_inner());
            if *seen == current {
                return false;
            }
        }
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return false;
        };
        let Ok(file) = serde_json::from_str::<WatchFile>(&text) else {
            tracing::warn!(path = %self.path.display(), "watch store is unreadable; keeping the current set");
            return false;
        };
        *self.watches.lock().unwrap_or_else(|e| e.into_inner()) = file.watches;
        *self.seen_mtime.lock().unwrap_or_else(|e| e.into_inner()) = current;
        self.changed.notify_one();
        true
    }

    /// 変更の通知。車線の監督ループが待つ。待ち手がいない間の変更も 1 回分は残る
    /// （車線の作り直し中に来た変更を落とさない）。
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    /// 設定画面（別プロセス）や手編集でファイルが変わったら、`interval` ごとに読み直して
    /// 車線へ知らせる。instance の寿命中ずっと回す。
    pub fn spawn_reload_poller(self: &Arc<Self>, interval: std::time::Duration) {
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if registry.reload_if_changed() {
                    tracing::info!(
                        path = %registry.path.display(),
                        watches = registry.list().len(),
                        "watch store changed on disk; applying"
                    );
                }
            }
        });
    }

    /// 現在の集合の最大間隔（秒）。dedup の保持時間の計算に使う。
    pub fn max_interval_secs(&self) -> i64 {
        self.lock()
            .iter()
            .map(|w| w.interval_secs)
            .max()
            .unwrap_or(0)
    }
}

/// 設定画面の変更を拾う間隔。
pub const RELOAD_POLL: std::time::Duration = std::time::Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn placement_watch(id: i64, interval: i64) -> WatchPlacement {
        serde_json::from_value(json!({
            "id": id,
            "interval_secs": interval,
            "filter": {"authors": ["a1"], "keywords": ["k"], "kinds": [1]}
        }))
        .unwrap()
    }

    #[test]
    fn store_is_seeded_from_the_placement_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir.path().join("nostaro-post-inst.toml"), "inst");
        let seeded = load_or_seed(&path, &[placement_watch(4, 1800)]).unwrap();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].id, 4);
        assert_eq!(seeded[0].interval_secs, 1800);
        assert_eq!(seeded[0].authors, vec!["a1".to_string()]);
        // 2 度目は配置を見ない（ファイルが正本）。
        let again = load_or_seed(&path, &[]).unwrap();
        assert_eq!(again, seeded);
    }

    #[test]
    fn set_list_delete_roundtrip_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir.path().join("nostaro-post-inst.toml"), "inst");
        let registry = WatchRegistry::load(path.clone(), &[]).unwrap();
        let created = registry
            .set(
                parse_set_request(&json!({
                    "interval_secs": 900,
                    "authors": ["x"],
                    "keywords": ["y"],
                    "kinds": [1, 7]
                }))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(created.id, 1);
        assert_eq!(registry.list().len(), 1);
        // 再読込（再起動相当）でも残る。
        let reloaded = WatchRegistry::load(path, &[]).unwrap();
        assert_eq!(reloaded.list(), registry.list());
        assert!(registry.delete(created.id).unwrap());
        assert!(registry.list().is_empty());
        assert!(!registry.delete(created.id).unwrap());
    }

    #[test]
    fn set_with_existing_id_replaces_in_place() {
        let registry = WatchRegistry::in_memory();
        let first = registry
            .set(parse_set_request(&json!({"interval_secs": 600, "authors": ["a"]})).unwrap())
            .unwrap();
        let second = registry
            .set(
                parse_set_request(
                    &json!({"id": first.id, "interval_secs": 1200, "authors": ["b"]}),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(second.id, first.id);
        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].interval_secs, 1200);
        assert_eq!(listed[0].authors, vec!["b".to_string()]);
    }

    #[test]
    fn invalid_requests_change_no_state() {
        let registry = WatchRegistry::in_memory();
        for payload in [
            json!({"interval_secs": 59}),
            json!({"interval_secs": 86401}),
            json!({"interval_secs": 600, "kinds": [4]}),
            json!({"interval_secs": 600, "kinds": []}),
            json!({"interval_secs": 600, "authors": "not-an-array"}),
            json!({"interval_secs": "600"}),
        ] {
            assert!(
                parse_set_request(&payload).is_err(),
                "must reject: {payload}"
            );
        }
        assert!(registry.list().is_empty(), "state must not change");
    }

    #[test]
    fn too_many_authors_and_keywords_are_rejected() {
        let authors: Vec<String> = (0..MAX_AUTHORS + 1).map(|i| format!("a{i}")).collect();
        assert!(parse_set_request(&json!({"interval_secs": 600, "authors": authors})).is_err());
        let keywords: Vec<String> = (0..MAX_KEYWORDS + 1).map(|i| format!("k{i}")).collect();
        assert!(parse_set_request(&json!({"interval_secs": 600, "keywords": keywords})).is_err());
    }

    #[test]
    fn kinds_default_to_note_when_omitted() {
        let parsed = parse_set_request(&json!({"interval_secs": 600})).unwrap();
        assert_eq!(parsed.kinds, vec![1]);
        assert_eq!(parsed.filter_kinds(), vec![1]);
    }

    impl WatchSetRequest {
        fn filter_kinds(&self) -> Vec<u32> {
            WatchSetting {
                id: 1,
                interval_secs: self.interval_secs,
                authors: self.authors.clone(),
                keywords: self.keywords.clone(),
                kinds: self.kinds.clone(),
            }
            .filter()
            .kinds
        }
    }
}

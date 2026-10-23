//! watch 設定画面（`nostr-gateway watch-settings <placement.json> <port>`）。
//!
//! Discord の VC 設定画面（`discord-gateway voice-settings`）と同じ形の**別プロセス**で、
//! 127.0.0.1 だけに bind する。gateway 本体（instance / daemon）は HTTP を listen しない。
//!
//! - `GET /` 一覧と追加・編集・削除の 1 ページ
//! - `GET /api/watches` 現在の watch（JSON）
//! - `POST /api/watches` 追加・更新（`id` を渡すと更新）。検証は capability と同じ
//!   [`crate::watch_store::parse_set_request`]
//! - `POST /api/watches/delete` 削除
//!
//! 保存は [`crate::watch_store`] を通すので、実行中の instance はファイルの変更を拾って
//! 数秒以内に車線を作り直す（`WatchRegistry::reload_if_changed`）。
//!
//! **秘密は置かない**: 鍵もトークンも画面・ログ・URL に出さない。URL に認証トークンを載せない
//! （loopback だけに bind する）。

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::config::{decode_config_b64, Placement};
use crate::watch_store::{parse_set_request, WatchRegistry, WatchSetting};

const PAGE: &str = include_str!("watch_settings_page.html");

struct AppState {
    /// instance ごとの (表示名, registry)。
    instances: Vec<(String, Arc<WatchRegistry>)>,
}

type Shared = State<Arc<AppState>>;

/// placement に載っている instance の watch 保存先を開き、loopback の `port` で画面を出す。
pub async fn run(placement: PathBuf, port: u16) -> anyhow::Result<()> {
    let instances = registries_for(&placement)?;
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
    tracing::info!(
        addr = %listener.local_addr()?,
        instances = instances.len(),
        "watch settings UI listening (loopback only)"
    );
    serve(listener, instances).await
}

/// placement の各 instance について、その instance の保存先を開く。
pub fn registries_for(
    placement: &std::path::Path,
) -> anyhow::Result<Vec<(String, Arc<WatchRegistry>)>> {
    let place = Placement::load(placement)?;
    let socket = PathBuf::from(&place.core_socket);
    let mut out = Vec::new();
    for instance in &place.instances {
        // instance 本体（main → spawn_instance）と同じ `config_b64` を種にする。別の欄を読むと、
        // 画面が先に起動したときに旧来の watch を落とした空の保存ファイルを作ってしまう。
        let bytes = decode_config_b64(&instance.config_b64)?;
        let cfg = crate::config::parse_instance_config(&bytes)?;
        let post_config = crate::post::post_config_path(&socket, &instance.instance_id);
        let registry = WatchRegistry::load(
            crate::watch_store::store_path(&post_config, &instance.instance_id),
            &cfg.watches,
        )?;
        out.push((instance.address.clone(), registry));
    }
    Ok(out)
}

pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    instances: Vec<(String, Arc<WatchRegistry>)>,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState { instances });
    let app = Router::new()
        .route("/", get(page))
        .route("/api/watches", get(list).post(apply))
        .route("/api/watches/delete", post(delete))
        .with_state(state);
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
struct Target {
    /// instance の address（1 つだけなら省略可）。
    #[serde(default)]
    instance: Option<String>,
}

impl AppState {
    fn pick(&self, instance: Option<&str>) -> Option<&Arc<WatchRegistry>> {
        match instance {
            Some(name) => self
                .instances
                .iter()
                .find(|(address, _)| address == name)
                .map(|(_, registry)| registry),
            None => self.instances.first().map(|(_, registry)| registry),
        }
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn list(State(state): Shared, Query(target): Query<Target>) -> Response {
    let Some(registry) = state.pick(target.instance.as_deref()) else {
        return fail(StatusCode::NOT_FOUND, "unknown instance");
    };
    // instance 側（エージェントの watch_set / watch_delete）が保存ファイルを変えていたら読み直す。
    registry.reload_if_changed();
    Json(json!({
        "watches": registry.list().iter().map(view).collect::<Vec<_>>(),
    }))
    .into_response()
}

fn view(watch: &WatchSetting) -> serde_json::Value {
    json!({
        "id": watch.id,
        "interval_secs": watch.interval_secs,
        "authors": watch.authors,
        "keywords": watch.keywords,
        "kinds": watch.kinds,
    })
}

async fn apply(
    State(state): Shared,
    Query(target): Query<Target>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(registry) = state.pick(target.instance.as_deref()) else {
        return fail(StatusCode::NOT_FOUND, "unknown instance");
    };
    // capability と同じ検証を使う（画面だけ緩い、を作らない）。
    let request = match parse_set_request(&body) {
        Ok(request) => request,
        Err(reject) => return fail(StatusCode::BAD_REQUEST, reject.0),
    };
    match registry.set(request) {
        Ok(setting) => Json(view(&setting)).into_response(),
        Err(reject) => fail(StatusCode::BAD_REQUEST, reject.0),
    }
}

async fn delete(
    State(state): Shared,
    Query(target): Query<Target>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(registry) = state.pick(target.instance.as_deref()) else {
        return fail(StatusCode::NOT_FOUND, "unknown instance");
    };
    let Some(id) = body.get("id").and_then(serde_json::Value::as_i64) else {
        return fail(StatusCode::BAD_REQUEST, "id is required");
    };
    match registry.delete(id) {
        Ok(deleted) => Json(json!({"id": id, "deleted": deleted})).into_response(),
        Err(reject) => fail(StatusCode::BAD_REQUEST, reject.0),
    }
}

fn fail(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn registry() -> Arc<WatchRegistry> {
        WatchRegistry::in_memory()
    }

    #[test]
    fn page_renders_without_secrets_and_has_the_controls() {
        assert!(PAGE.contains("watch"), "page must mention watches");
        for needle in ["interval_secs", "authors", "keywords", "kinds"] {
            assert!(PAGE.contains(needle), "page must expose {needle}");
        }
        assert!(!PAGE.contains("nsec"), "page must not contain key material");
        assert!(
            !PAGE.contains("NOSTARO_SECRET_KEY"),
            "page must not contain the key env name"
        );
    }

    #[test]
    fn list_view_matches_the_stored_watch() {
        let registry = registry();
        registry
            .set(
                parse_set_request(&json!({
                    "interval_secs": 1800,
                    "authors": ["a"],
                    "keywords": ["k"],
                    "kinds": [1, 7]
                }))
                .unwrap(),
            )
            .unwrap();
        let listed: Vec<_> = registry.list().iter().map(view).collect();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["interval_secs"], json!(1800));
        assert_eq!(listed[0]["kinds"], json!([1, 7]));
    }

    /// 実 HTTP で: 画面が出る・POST が保存ファイルへ書く・不正値は 400 で保存しない・削除できる。
    #[tokio::test]
    async fn http_page_renders_and_post_persists_to_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("nostaro-watches-inst.json");
        let registry = WatchRegistry::load(store.clone(), &[]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, vec![("nostr-a".into(), registry)]));
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");

        let page = http.get(&base).send().await.unwrap();
        assert!(page.status().is_success());
        let html = page.text().await.unwrap();
        assert!(html.contains("interval_secs") && !html.contains("nsec"));

        let saved = http
            .post(format!("{base}/api/watches"))
            .json(&json!({"interval_secs": 1800, "authors": ["a"], "kinds": [1, 6]}))
            .send()
            .await
            .unwrap();
        assert!(saved.status().is_success(), "{:?}", saved.text().await);
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
        assert_eq!(on_disk["watches"][0]["interval_secs"], json!(1800));
        assert_eq!(on_disk["watches"][0]["kinds"], json!([1, 6]));

        let bad = http
            .post(format!("{base}/api/watches"))
            .json(&json!({"interval_secs": 5}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
        let listed: serde_json::Value = http
            .get(format!("{base}/api/watches"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            listed["watches"].as_array().unwrap().len(),
            1,
            "bad input must not be stored"
        );

        let deleted = http
            .post(format!("{base}/api/watches/delete"))
            .json(&json!({"id": on_disk["watches"][0]["id"]}))
            .send()
            .await
            .unwrap();
        assert!(deleted.status().is_success());
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&store).unwrap()).unwrap();
        assert_eq!(on_disk["watches"], json!([]));
    }

    /// エージェント（instance 側の `watch_set` / `watch_delete`）が保存ファイルを書き換えたら、
    /// 画面の `GET /api/watches` はその変更を返す（画面プロセスの古い一覧を見せない）。
    #[tokio::test]
    async fn http_list_shows_changes_written_by_the_instance() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("nostaro-watches-inst.json");
        let page_registry = WatchRegistry::load(store.clone(), &[]).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, vec![("nostr-a".into(), page_registry)]));
        let http = reqwest::Client::new();
        let list_url = format!("http://{addr}/api/watches");
        let list = || async {
            http.get(&list_url)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        };
        assert_eq!(list().await["watches"], json!([]));

        // instance 側の registry（別プロセス相当）が同じ保存ファイルへ書く。mtime の粒度で
        // 取りこぼさないよう、画面が最初に読んだ時刻から間をあける。
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let instance_registry = WatchRegistry::load(store.clone(), &[]).unwrap();
        let created = instance_registry
            .set(
                parse_set_request(&json!({
                    "interval_secs": 1800,
                    "authors": ["a"],
                    "kinds": [1]
                }))
                .unwrap(),
            )
            .unwrap();

        let after_set = list().await;
        assert_eq!(
            after_set["watches"].as_array().unwrap().len(),
            1,
            "{after_set}"
        );
        assert_eq!(after_set["watches"][0]["id"], json!(created.id));
        assert_eq!(after_set["watches"][0]["interval_secs"], json!(1800));

        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(instance_registry.delete(created.id).unwrap());
        assert_eq!(list().await["watches"], json!([]));
    }

    /// 画面が先に起動しても、instance と同じ `config_b64` の watches を種にする（旧来分を落とさない）。
    #[test]
    fn settings_seed_from_the_same_config_as_the_instance() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let config = json!({
            "relays": ["wss://relay.invalid"],
            "self_pubkey": "a".repeat(64),
            "name": "crab",
            "access": {"owner": ["b".repeat(64)]},
            "watches": [{"id": 4, "interval_secs": 1800, "filter": {"authors": ["x"], "kinds": [1]}}],
        });
        let placement = json!({
            "core_socket": dir.path().join("core.sock"),
            "nostaro_bin": "/nonexistent-nostaro",
            "instances": [{
                "instance_id": "00000000-0000-4000-8000-000000000001",
                "revision": 1,
                "address": "nostr-a",
                "config_b64": base64::engine::general_purpose::STANDARD
                    .encode(serde_json::to_vec(&config).unwrap()),
            }],
        });
        let path = dir.path().join("placement.json");
        std::fs::write(&path, placement.to_string()).unwrap();
        let registries = registries_for(&path).unwrap();
        let listed = registries[0].1.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, 4);
        assert_eq!(listed[0].interval_secs, 1800);
    }

    #[test]
    fn invalid_input_is_refused_without_state_change() {
        let registry = registry();
        assert!(parse_set_request(&json!({"interval_secs": 5})).is_err());
        assert!(registry.list().is_empty());
    }
}

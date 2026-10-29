//! D-1097 N2: Nostr gateway settings page (`nostr-gateway settings <daemon-config> <bind>`).
//!
//! A separate process from the daemon. It owns no state: settings go through the daemon's
//! protected admin socket (the same ops an operator tool would use), and watches go to each
//! instance's watch store file, which the running child already reloads within seconds.
//! Every instance in the admin scope is listed; none is picked implicitly.
//!
//! HTTP API (JSON):
//! - `GET  /` the page
//! - `GET  /api/instances` every instance with settings and access (no secrets)
//! - `POST /api/instances/{id}/config` `{relays, name, filter}`
//! - `POST /api/instances/{id}/access` `{owner, trusted_users, followees, co_agents}`
//! - `POST /api/instances/{id}/key` `{nsec}` (write-only)
//! - `POST /api/instances/{id}/generate-key` `{prefix, overwrite: true}` (vanity; leaves it disabled)
//! - `POST /api/instances/{id}/enabled` `{enabled}`
//! - `GET  /api/instances/{id}/watches`, `POST …/watches`, `POST …/watches/delete` `{id}`
//!
//! Secrets are never returned, logged or put in URLs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

use crate::config::WatchPlacement;
use crate::watch_store::{parse_set_request, WatchRegistry, WatchSetting};

const PAGE: &str = include_str!("settings_page.html");

#[derive(Clone)]
pub struct Settings {
    /// The daemon's protected admin socket (mode 0600).
    pub admin_socket: PathBuf,
    /// Directory holding `nostaro-watches-<instance_id>.json` (the core socket's directory).
    pub watch_dir: PathBuf,
}

struct AppState {
    settings: Settings,
    watches: Mutex<HashMap<String, Arc<WatchRegistry>>>,
}

type Shared = State<Arc<AppState>>;

/// Load the daemon config, then serve the page on `bind` (e.g. `0.0.0.0:8794`).
pub async fn run(daemon_config: PathBuf, bind: String) -> anyhow::Result<()> {
    let config = crate::daemon::DaemonConfig::load(&daemon_config)?;
    let watch_dir = config
        .core_socket
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "nostr settings UI listening");
    serve(
        listener,
        Settings {
            admin_socket: config.admin_socket,
            watch_dir,
        },
    )
    .await
}

pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    settings: Settings,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        settings,
        watches: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/", get(page))
        .route("/api/instances", get(list))
        .route("/api/instances/{id}/config", post(edit_config))
        .route("/api/instances/{id}/access", post(edit_access))
        .route("/api/instances/{id}/key", post(edit_key))
        .route("/api/instances/{id}/generate-key", post(generate_key))
        .route("/api/instances/{id}/enabled", post(edit_enabled))
        .route(
            "/api/instances/{id}/watches",
            get(list_watches).post(set_watch),
        )
        .route("/api/instances/{id}/watches/delete", post(delete_watch))
        .with_state(state);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

/// One admin request. The scope is the instance itself, as the daemon requires.
async fn admin(
    socket: &std::path::Path,
    op: &str,
    instance_id: &str,
    extra: Value,
) -> Result<Value, Response> {
    let mut request =
        json!({"id": op, "op": op, "scope_instance_id": instance_id, "instance_id": instance_id});
    if let Value::Object(fields) = extra {
        for (key, value) in fields {
            request[key] = value;
        }
    }
    let unavailable = |_| {
        fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "nostr gateway is not reachable",
        )
    };
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(unavailable)?;
    let mut line = serde_json::to_vec(&request)
        .map_err(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "encode"))?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(unavailable)?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .await
        .map_err(unavailable)?;
    let reply: Value = serde_json::from_str(&reply)
        .map_err(|_| fail(StatusCode::BAD_GATEWAY, "bad admin reply"))?;
    if reply["ok"] == json!(true) {
        return Ok(reply["result"].clone());
    }
    let error = reply["error"].as_str().unwrap_or("rejected").to_string();
    let status = match error.as_str() {
        "scope_denied" | "instance unknown" => StatusCode::NOT_FOUND,
        _ => StatusCode::BAD_REQUEST,
    };
    Err(fail(status, error))
}

async fn list(State(state): Shared) -> Response {
    let socket = &state.settings.admin_socket;
    let listed = match admin(socket, "list", "", json!({})).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let mut instances = Vec::new();
    for item in listed["instances"].as_array().cloned().unwrap_or_default() {
        let id = item["instance_id"].as_str().unwrap_or_default().to_string();
        match admin(socket, "get", &id, json!({})).await {
            Ok(detail) => instances.push(detail),
            Err(response) => return response,
        }
    }
    Json(json!({ "instances": instances })).into_response()
}

async fn edit_config(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    reply(
        admin(
            &state.settings.admin_socket,
            "set_config",
            &id,
            json!({ "settings": body }),
        )
        .await,
    )
}

async fn edit_access(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    reply(
        admin(
            &state.settings.admin_socket,
            "set_access",
            &id,
            json!({ "settings": body }),
        )
        .await,
    )
}

async fn edit_key(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(nsec) = body.get("nsec").and_then(Value::as_str) else {
        return fail(StatusCode::BAD_REQUEST, "nsec is required");
    };
    reply(
        admin(
            &state.settings.admin_socket,
            "set_key",
            &id,
            json!({ "credential": nsec }),
        )
        .await,
    )
}

async fn generate_key(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let extra = json!({
        "prefix": body.get("prefix").and_then(Value::as_str).unwrap_or_default(),
        "overwrite": body.get("overwrite").and_then(Value::as_bool).unwrap_or(false),
    });
    reply(admin(&state.settings.admin_socket, "generate_key", &id, extra).await)
}

async fn edit_enabled(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return fail(StatusCode::BAD_REQUEST, "enabled is required");
    };
    reply(
        admin(
            &state.settings.admin_socket,
            "set_enabled",
            &id,
            json!({ "enabled": enabled }),
        )
        .await,
    )
}

/// The instance's watch registry. The first open seeds an absent store from the instance's
/// stored config, exactly as the child does, so opening the page first loses nothing.
async fn registry(state: &AppState, id: &str) -> Result<Arc<WatchRegistry>, Response> {
    if let Some(found) = state
        .watches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(id)
    {
        return Ok(found.clone());
    }
    // Ask the daemon first: unknown or out-of-scope ids never get a file.
    let detail = admin(&state.settings.admin_socket, "get", id, json!({})).await?;
    if detail.is_null() {
        return Err(fail(StatusCode::NOT_FOUND, "instance unknown"));
    }
    let seed: Vec<WatchPlacement> =
        serde_json::from_value(detail["settings"]["watches"].clone()).unwrap_or_default();
    let path = crate::watch_store::store_path(
        &crate::post::post_config_path(&state.settings.watch_dir.join("core.sock"), id),
        id,
    );
    let opened = WatchRegistry::load(path, &seed)
        .map_err(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "watch store unavailable"))?;
    let mut map = state.watches.lock().unwrap_or_else(|e| e.into_inner());
    Ok(map.entry(id.to_string()).or_insert(opened).clone())
}

async fn list_watches(State(state): Shared, Path(id): Path<String>) -> Response {
    let registry = match registry(&state, &id).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    // The agent (watch_set / watch_delete) may have written the file since the last read.
    registry.reload_if_changed();
    Json(json!({ "watches": registry.list().iter().map(view).collect::<Vec<_>>() })).into_response()
}

async fn set_watch(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let registry = match registry(&state, &id).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    // The same validation as the agent capability.
    let request = match parse_set_request(&body) {
        Ok(request) => request,
        Err(reject) => return fail(StatusCode::BAD_REQUEST, reject.0),
    };
    match registry.set(request) {
        Ok(setting) => Json(view(&setting)).into_response(),
        Err(reject) => fail(StatusCode::BAD_REQUEST, reject.0),
    }
}

async fn delete_watch(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let registry = match registry(&state, &id).await {
        Ok(registry) => registry,
        Err(response) => return response,
    };
    let Some(watch_id) = body.get("id").and_then(Value::as_i64) else {
        return fail(StatusCode::BAD_REQUEST, "id is required");
    };
    match registry.delete(watch_id) {
        Ok(deleted) => Json(json!({ "id": watch_id, "deleted": deleted })).into_response(),
        Err(reject) => fail(StatusCode::BAD_REQUEST, reject.0),
    }
}

fn view(watch: &WatchSetting) -> Value {
    json!({
        "id": watch.id,
        "interval_secs": watch.interval_secs,
        "authors": watch.authors,
        "keywords": watch.keywords,
        "kinds": watch.kinds,
    })
}

fn reply(result: Result<Value, Response>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

fn fail(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

#[cfg(test)]
#[path = "settings_http_tests.rs"]
mod tests;

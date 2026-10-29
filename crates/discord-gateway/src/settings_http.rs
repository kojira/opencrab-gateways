//! D-1097 D4: the Discord gateway settings page
//! (`discord-gateway settings <daemon-config> <bind>`).
//!
//! A separate process from the daemon. It reads and writes only through the daemon's admin
//! socket (mode 0600), so the daemon stays the one writer of its store. Secrets go in, never
//! out: the page shows only whether a bot token is configured.
//!
//! - `GET  /`                                   one HTML page for every instance plus voice
//! - `GET  /api/instances`                      every instance (no secrets)
//! - `PUT  /api/instances/{id}/config`          the whole Discord instance config (JSON)
//! - `PUT  /api/instances/{id}/addresses`       `{"addresses": [...]}`
//! - `PUT  /api/instances/{id}/credential`      `{"credential": "<bot token>"}` (write-only)
//! - `PUT  /api/instances/{id}/enabled`         `{"enabled": bool}` (start / stop the bot)
//! - `GET  /voice/`                             the VC settings page (embedded)
//! - `/api/voice-settings*`, `/voice/api/voice-settings*`  the VC settings API

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

const PAGE: &str = include_str!("settings_page.html");

struct AppState {
    admin_socket: PathBuf,
    discord: DiscordApi,
    http: reqwest::Client,
}

type Shared = State<Arc<AppState>>;

/// `host:port`. Production binds `0.0.0.0:8793` so the page opens over Tailscale.
pub fn parse_bind(raw: &str) -> anyhow::Result<SocketAddr> {
    raw.parse()
        .with_context(|| format!("bind must be host:port, got {raw:?}"))
}

/// Runs the page for the daemon described by `config` on `bind` (returns only on failure).
pub async fn run(config: crate::daemon::DaemonConfig, bind: SocketAddr) -> anyhow::Result<()> {
    let voice = config.voice_settings_path();
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %listener.local_addr()?, "discord settings UI listening");
    serve_on(listener, config.admin_socket, voice).await
}

/// How the page asks Discord which bot a token belongs to.
#[derive(Clone)]
pub struct DiscordApi {
    pub base_url: String,
}

impl DiscordApi {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

/// Discord's real API.
pub const DISCORD_API: &str = "https://discord.com/api/v10";

impl DiscordApi {
    /// The bot user id the token authenticates as (`GET /users/@me`). The token is sent only
    /// to Discord and never logged.
    async fn bot_id(&self, http: &reqwest::Client, token: &str) -> Result<String, String> {
        let response = http
            .get(format!("{}/users/@me", self.base_url))
            .header("authorization", format!("Bot {token}"))
            .header(
                "user-agent",
                "DiscordBot (https://github.com/kojira/opencrab, 1.0)",
            )
            .send()
            .await
            .map_err(|e| {
                format!(
                    "Discord could not be reached: {}",
                    crate::secret::redact_token(&e.to_string())
                )
            })?;
        if !response.status().is_success() {
            return Err(format!("Discord refused the token ({})", response.status()));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|_| "Discord returned an unreadable response".to_string())?;
        let id = body["id"].as_str().unwrap_or_default().to_string();
        if !crate::config::is_snowflake(&id) || body["bot"] != true {
            return Err("the token is not a bot token".into());
        }
        Ok(id)
    }
}

pub async fn serve_on(
    listener: tokio::net::TcpListener,
    admin_socket: PathBuf,
    voice_settings: PathBuf,
) -> anyhow::Result<()> {
    serve_with(
        listener,
        admin_socket,
        voice_settings,
        DiscordApi::new(DISCORD_API),
    )
    .await
}

pub async fn serve_with(
    listener: tokio::net::TcpListener,
    admin_socket: PathBuf,
    voice_settings: PathBuf,
    discord: DiscordApi,
) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        admin_socket,
        discord,
        http: reqwest::Client::new(),
    });
    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/instances", get(list))
        .route("/api/instances/{id}/credential", put(set_credential))
        .route("/api/instances/{id}/channels", post(add_channel))
        .route("/api/instances/{id}/{field}", put(update))
        .with_state(state)
        // The VC page is embedded as-is; its relative `api/voice-settings` calls resolve under
        // `voice/`. The same routes are also served at the top for direct use.
        .route(
            "/voice/",
            get(|| async { Html(crate::voice::settings_http::PAGE) }),
        )
        .nest(
            "/voice",
            crate::voice::settings_http::api_router(voice_settings.clone()),
        )
        .merge(crate::voice::settings_http::api_router(voice_settings));
    axum::serve(listener, app).await?;
    Ok(())
}

fn error(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

async fn list(State(state): Shared) -> Response {
    match admin(&state, json!({"id":"settings","op":"list"})).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn update(
    State(state): Shared,
    Path((id, field)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let mut request = json!({"id":"settings","scope_instance_id":id,"instance_id":id});
    let (op, key) = match field.as_str() {
        "config" => ("set_config", "config"),
        "addresses" => ("set_addresses", "addresses"),
        "enabled" => ("set_enabled", "enabled"),
        _ => return error(StatusCode::NOT_FOUND, "unknown setting"),
    };
    request["op"] = json!(op);
    request[key] = if key == "config" {
        body
    } else {
        body.get(key).cloned().unwrap_or(Value::Null)
    };
    match admin(&state, request).await {
        Ok(value) => Json(value).into_response(),
        Err(AdminError::Rejected(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

/// Checks the token with Discord first, then stores it together with the bot id it belongs to.
async fn set_credential(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let token = body["credential"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        return error(StatusCode::BAD_REQUEST, "the bot token is empty");
    }
    let bot_id = match state.discord.bot_id(&state.http, &token).await {
        Ok(id) => id,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let request = json!({"id":"settings","op":"set_credential","scope_instance_id":id,
        "instance_id":id,"credential":token,"self_bot_id":bot_id});
    match admin(&state, request).await {
        Ok(mut value) => {
            value["self_bot_id"] = json!(bot_id);
            Json(value).into_response()
        }
        Err(AdminError::Rejected(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

/// Adds one channel (`guild_id` empty for a DM) to the bot's channels.
async fn add_channel(
    State(state): Shared,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let guild = body["guild_id"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let channel = body["channel_id"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !crate::config::is_snowflake(&channel)
        || !(guild.is_empty() || crate::config::is_snowflake(&guild))
    {
        return error(
            StatusCode::BAD_REQUEST,
            "server and channel ids must be Discord ids",
        );
    }
    let listed = match admin(&state, json!({"id":"settings","op":"list"})).await {
        Ok(value) => value,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let Some(instance) = listed["instances"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["instance_id"] == id.as_str()))
    else {
        return error(StatusCode::BAD_REQUEST, "instance_unknown");
    };
    let agent = instance["agent_id"].as_str().unwrap_or_default();
    let mut addresses: Vec<Value> = instance["addresses"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let address = crate::map::address_for(agent, &guild, &channel);
    if !addresses.iter().any(|a| a == address.as_str()) {
        addresses.push(json!(address));
    }
    let request = json!({"id":"settings","op":"set_addresses","scope_instance_id":id,
        "instance_id":id,"addresses":addresses});
    match admin(&state, request).await {
        Ok(value) => Json(value).into_response(),
        Err(AdminError::Rejected(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[derive(Debug)]
enum AdminError {
    /// The daemon refused the change (validation, scope, unknown instance).
    Rejected(String),
    Unreachable(anyhow::Error),
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) => write!(f, "{message}"),
            Self::Unreachable(error) => write!(f, "daemon admin socket: {error:#}"),
        }
    }
}

/// One JSON line to the daemon's admin socket and its one-line answer.
async fn admin(state: &AppState, request: Value) -> Result<Value, AdminError> {
    let call = async {
        let mut stream = tokio::net::UnixStream::connect(&state.admin_socket).await?;
        stream.write_all(format!("{request}\n").as_bytes()).await?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await?;
        Ok::<Value, anyhow::Error>(serde_json::from_str(&line)?)
    };
    let response = call.await.map_err(AdminError::Unreachable)?;
    if response["ok"] == true {
        Ok(response["result"].clone())
    } else {
        Err(AdminError::Rejected(
            response["error"].as_str().unwrap_or("rejected").to_string(),
        ))
    }
}

#[cfg(test)]
#[path = "settings_http_tests.rs"]
mod tests;

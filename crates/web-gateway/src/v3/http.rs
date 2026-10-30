//! Web HTTP/SSE transport. The loopback HTTP routes retain their historical Owner context.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use super::client::{InstanceClient, LiveEvent, OpenError, PostRefuse, SaidOutcome};
use super::json::parse_object_no_dup;
use super::wire::{parse_uuid, Attachment, LiveInboundScope, SaidCaller, SaidContext};

const JSON_TYPE: &str = "application/json; charset=utf-8";

#[derive(Clone)]
pub struct HttpState {
    pub instances: Vec<Arc<InstanceClient>>,
    pub agent_clients: HashMap<String, Arc<InstanceClient>>,
    /// D-1099: which instance owns each web conversation (gateway state).
    pub conversations: Arc<crate::conversations::Conversations>,
}

pub fn router(state: HttpState) -> Router {
    Router::new()
        .route("/api/web-conversations", post(create_conversation))
        .route("/api/web-conversations/{session_id}", get(get_conversation))
        .route(
            "/api/web-conversations/{session_id}/messages",
            post(post_message),
        )
        .route(
            "/api/web-conversations/{session_id}/events",
            get(get_events),
        )
        .route("/rooms/{room}/messages", get(gone).post(gone))
        .route("/chat", get(gone))
        .with_state(state)
}

struct CreateBody {
    agent_id: String,
    name: Option<String>,
}

fn parse_create_body(bytes: &[u8]) -> Result<CreateBody, &'static str> {
    let value = parse_object_no_dup(bytes).map_err(|_| "bad_request")?;
    let obj = value.as_object().ok_or("bad_request")?;
    if obj.keys().any(|key| key != "agent_id" && key != "name") {
        return Err("bad_request");
    }
    let agent_id = obj
        .get("agent_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("bad_request")?
        .to_string();
    let name = match obj.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => {
            let trimmed = value.trim_matches(char::is_whitespace);
            if trimmed.contains('\n') || trimmed.contains('\r') || trimmed.chars().count() > 100 {
                return Err("bad_request");
            }
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Some(_) => return Err("bad_request"),
    };
    Ok(CreateBody { agent_id, name })
}

async fn create_conversation(State(state): State<HttpState>, body: axum::body::Bytes) -> Response {
    let parsed = match parse_create_body(&body) {
        Ok(parsed) => parsed,
        Err(code) => return json_error(StatusCode::BAD_REQUEST, code, None),
    };
    let Some(client) = state.agent_clients.get(&parsed.agent_id) else {
        return json_error(StatusCode::CONFLICT, "instance_unavailable", None);
    };
    if !client.connection_live().await {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None);
    }
    let conversation_id = uuid::Uuid::new_v4().to_string();
    // The session key keeps the historical `extgate-<uuid>` shape, so session URLs are unchanged.
    let session_key = format!("extgate-{}", uuid::Uuid::new_v4());
    let session_id = match client.open(&session_key, parsed.name.as_deref()).await {
        Ok(session_id) => session_id,
        Err(OpenError::NotReady | OpenError::Disconnected) => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None);
        }
        Err(OpenError::Rejected { code }) => {
            return json_error(wire_status(&code), &code, None);
        }
    };
    if let Err(error) = state
        .conversations
        .record(&session_key, &client.instance_id, &session_id)
    {
        tracing::warn!(error = %error, "web conversation not recorded");
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "store_error", None);
    }
    json_state(
        StatusCode::CREATED,
        json!({
            "conversation_id": conversation_id,
            "session_id": session_id,
            // Kept for the dashboard's create response (it is the session key now).
            "binding_id": session_key,
            "name": parsed.name,
            "state": "ready",
        }),
    )
}

async fn get_conversation(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
) -> Response {
    let client = match find_client(&state, &session_id) {
        Ok(client) => client,
        Err(FindClient::None) => return StatusCode::NOT_FOUND.into_response(),
        Err(FindClient::Unavailable) => {
            return json_state(
                StatusCode::OK,
                json!({"session_id": session_id, "state": "unavailable"}),
            )
        }
    };
    let state_name = if client.connection_live().await {
        "ready"
    } else {
        "unavailable"
    };
    json_state(
        StatusCode::OK,
        json!({"session_id": session_id, "state": state_name}),
    )
}

async fn gone() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn json_error(status: StatusCode, code: &str, detail: Option<&str>) -> Response {
    let body = json!({"error":{"code": code, "detail": detail}});
    let mut res = (status, Json(body)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_TYPE));
    res
}

fn json_state(status: StatusCode, body: Value) -> Response {
    let mut res = (status, Json(body)).into_response();
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(JSON_TYPE));
    res
}

/// The instance that owns the web conversation `session_key` (gateway state, D-1099).
fn find_client(state: &HttpState, session_key: &str) -> Result<Arc<InstanceClient>, FindClient> {
    let owner = match state.conversations.owner(session_key) {
        Ok(Some(owner)) => owner,
        Ok(None) => return Err(FindClient::None),
        Err(error) => {
            tracing::warn!(error = %error, "web conversation lookup failed");
            return Err(FindClient::Unavailable);
        }
    };
    state
        .instances
        .iter()
        .find(|inst| inst.instance_id == owner)
        .cloned()
        .ok_or(FindClient::Unavailable)
}

fn sse_error(code: &str) -> Response {
    let ev = Event::default()
        .event("gate_error")
        .data(json!({"code": code, "detail": null}).to_string());
    let stream = futures::stream::once(async move { Ok::<_, Infallible>(ev) });
    Sse::new(stream).into_response()
}

enum FindClient {
    /// Not a conversation of this gateway.
    None,
    /// Ours, but its instance is not running here.
    Unavailable,
}

struct PostBody {
    client_message_id: String,
    text: String,
    attachments: Vec<Attachment>,
}

fn parse_post_body(bytes: &[u8]) -> Result<PostBody, &'static str> {
    let obj = parse_object_no_dup(bytes).map_err(|_| "bad_request")?;
    let id = obj
        .get("client_message_id")
        .and_then(Value::as_str)
        .ok_or("bad_request")?;
    let client_message_id = parse_uuid(id).map_err(|_| "bad_request")?;
    let text = obj
        .get("text")
        .and_then(Value::as_str)
        .ok_or("bad_request")?;
    let attachments = parse_attachments(obj.get("attachments"))?;
    if text.is_empty() && attachments.is_empty() {
        return Err("bad_request");
    }
    Ok(PostBody {
        client_message_id,
        text: text.to_string(),
        attachments,
    })
}

fn parse_attachments(value: Option<&Value>) -> Result<Vec<Attachment>, &'static str> {
    let Some(Value::Array(items)) = value else {
        return Err("bad_request");
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let obj = item.as_object().ok_or("bad_request")?;
        let kind = obj
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("bad_request")?;
        if kind != "image" {
            return Err("bad_request");
        }
        let url = obj
            .get("url")
            .and_then(Value::as_str)
            .ok_or("bad_request")?;
        if !url.starts_with("https://") || url.len() <= "https://".len() {
            return Err("bad_request");
        }
        out.push(Attachment::ImageUrl {
            url: url.to_string(),
        });
    }
    Ok(out)
}

async fn post_message(
    State(state): State<HttpState>,
    Path(session_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let parsed = match parse_post_body(&body) {
        Ok(p) => p,
        Err(code) => return json_error(StatusCode::BAD_REQUEST, code, None),
    };
    let origin = format!("web:{}", parsed.client_message_id);
    let client = match find_client(&state, &session_id) {
        Ok(c) => c,
        Err(FindClient::None) => return StatusCode::NOT_FOUND.into_response(),
        Err(FindClient::Unavailable) => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None);
        }
    };
    if !client.connection_live().await {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "disconnect", None);
    }
    let context = SaidContext {
        caller: SaidCaller::Owner,
        start_turn: true,
        system_context: None,
        reply_target: None,
        reference: None,
        live_inbound_scope: LiveInboundScope::All,
        model_profile: super::wire::TurnModelProfile::Default,
        agent_prelude: None,
        provisional_id: None,
    };
    match client
        .post_said_with_self_context(
            &session_id,
            &origin,
            &context,
            &parsed.text,
            &parsed.attachments,
        )
        .await
    {
        Err(PostRefuse::NotReady) => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "instance_not_ready", None)
        }
        Err(PostRefuse::Busy) => json_error(StatusCode::CONFLICT, "conversation_busy", None),
        Ok(SaidOutcome::Accepted { seq }) => json_state(
            StatusCode::ACCEPTED,
            json!({
                "client_message_id": parsed.client_message_id,
                "origin": origin,
                "seq": seq,
                "state": "accepted",
            }),
        ),
        Ok(SaidOutcome::NotAdmitted) => {
            json_state(StatusCode::FORBIDDEN, json!({"state": "not_admitted"}))
        }
        Ok(SaidOutcome::WireErr { code, detail }) => {
            json_error(wire_status(&code), &code, detail.as_deref())
        }
        Ok(SaidOutcome::Disconnected) => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "disconnect", None)
        }
        Ok(SaidOutcome::Opened { .. }) => {
            json_error(StatusCode::BAD_GATEWAY, "unexpected_open", None)
        }
    }
}

fn wire_status(code: &str) -> StatusCode {
    match code {
        "bad_request" => StatusCode::BAD_REQUEST,
        "subject_unknown" => StatusCode::NOT_FOUND,
        "instance_not_ready" | "instance_conflict" | "instance_active" => StatusCode::CONFLICT,
        "store_error" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_GATEWAY,
    }
}

async fn get_events(State(state): State<HttpState>, Path(session_id): Path<String>) -> Response {
    let client = match find_client(&state, &session_id) {
        Ok(c) => c,
        Err(FindClient::None) => return StatusCode::NOT_FOUND.into_response(),
        Err(FindClient::Unavailable) => return sse_error("disconnect"),
    };
    if !client.connection_live().await {
        return sse_error("disconnect");
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
    tokio::spawn(async move {
        while let Some(ev) = client.next_live(&session_id).await {
            let event = match live_to_event(&ev) {
                Some(e) => e,
                None => continue,
            };
            if tx.send(Ok(event)).await.is_err() {
                break;
            }
            if matches!(ev, LiveEvent::Error { .. }) {
                break;
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn live_to_event(ev: &LiveEvent) -> Option<Event> {
    match ev {
        // web SSE は返信先（reply_origin）を使わない（Nostr 固有の e-tag 用）。
        LiveEvent::Message { text, .. } => Some(
            Event::default()
                .event("message")
                .data(json!({"text": text}).to_string()),
        ),
        LiveEvent::Activity {
            activity_id,
            state,
            origin,
        } => Some(Event::default().event("activity").data(
            json!({"activity_id": activity_id, "state": state, "origin": origin}).to_string(),
        )),
        // web は返信先アンカーを使わないので reply_origin は無視する（SSE 外形は不変）。
        LiveEvent::CompletedNoReply { .. } => {
            Some(Event::default().event("completed_no_reply").data("{}"))
        }
        // web には platform message id 対応表が無いため、Discord 用の完了対象は流さない。
        LiveEvent::Completed { .. } => None,
        // web SSE は最終 message だけを流す。途中の partial text は流さない（外形は不変）。
        LiveEvent::Partial { .. } => None,
        // D-1085: provisional runs are a voice speculation; web SSE never starts them.
        LiveEvent::ProvisionalResult { .. } => None,
        // R3: web SSE は失敗を通知イベントとして流す（reply_origin は Nostr 固有の e-tag 用途）。
        LiveEvent::TurnFailed { .. } => Some(Event::default().event("turn_failed").data("{}")),
        // D-1087: 活動の詳細（ツール実行・サブタスク lifecycle）は web SSE の外形に載せない。
        LiveEvent::ToolActivity { .. } | LiveEvent::SubtaskLifecycle { .. } => None,
        LiveEvent::Error { code, detail } => Some(
            Event::default()
                .event("gate_error")
                .data(json!({"code": code, "detail": detail}).to_string()),
        ),
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use tower::ServiceExt as _;

    #[test]
    fn partial_text_is_not_streamed_to_sse() {
        let partial = LiveEvent::Partial {
            stream_id: "s".into(),
            text: "一文目です。".into(),
            provisional_id: None,
        };
        assert!(live_to_event(&partial).is_none());
    }

    #[tokio::test]
    async fn web_routes_preserve_historical_credential_free_access() {
        let app = router(HttpState {
            instances: Vec::new(),
            agent_clients: HashMap::new(),
            conversations: Arc::new(crate::conversations::Conversations::open(None).unwrap()),
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/web-conversations")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(r#"{"agent_id":"agent"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}

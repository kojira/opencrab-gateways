//! A2UI restored at the gateway layer (D-1110). The agent's `send_ui` operation posts a message
//! with buttons / select menus (and a modal Form); the first answer of an owner (or the timeout)
//! goes back to the same core session as a normal `said`. Core keeps no UI state: pending UIs live
//! in this gateway's DB ([`store::A2uiStore`]). Behaviour follows the legacy implementation
//! (`a2b1844b^` core `send_ui`, `34903f00^` Discord renderer/interactions); see
//! `d1110-a2ui-inventory`.

pub mod discord;
pub mod event;
pub mod render;
pub mod store;

use std::sync::{Arc, OnceLock};

use opencrab_gate_client::client::{InstanceClient, SaidOutcome};
use opencrab_gate_client::wire::{LiveInboundScope, SaidCaller, SaidContext};
use opencrab_gate_client::InvokeOutcome;
use serde_json::{json, Value};

use crate::config::AccessConfig;
use discord::A2uiDiscord;
use event::{InteractionEvent, InteractionKind};
use store::{A2uiStore, PendingUi};

pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const TIMEOUT_NOTE: &str = "⏰ タイムアウトしました";

/// The `send_ui` declaration: same name and arguments as the legacy core tool. `channel_id` is
/// optional now (default: the channel of this conversation), since core no longer hands channel
/// ids to the agent.
pub fn send_ui_declaration() -> Value {
    json!({
        "name": "send_ui",
        "description": "A2UIコンポーネントで構成されたUI（ボタン・セレクトメニュー・フォーム）をこの会話のチャンネルに送信する。応答（押した人・押したボタンの action と context・選んだ値・フォームの入力）は、オーナーが操作したときに、この会話への発言として届く。誰も操作しないまま timeout_secs が過ぎると、タイムアウトしたことが届く。1つのUIへの応答は最初の1回だけで、応答後はボタンが無効になる。\n\n使用例（ボタン）:\n{\"components\": [{\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"選んでください\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"btn1\", \"btn2\"]}, {\"id\": \"btn1\", \"component\": \"Button\", \"text\": \"選択A\", \"style\": \"primary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"A\"}}}, {\"id\": \"btn2\", \"component\": \"Button\", \"text\": \"選択B\", \"style\": \"secondary\", \"action\": {\"name\": \"choose\", \"context\": {\"value\": \"B\"}}}]}\n\n使用例（セレクトメニュー）:\n{\"components\": [{\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"sel1\"]}, {\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"モデルを選択\"}, {\"id\": \"sel1\", \"component\": \"SelectMenu\", \"placeholder\": \"モデルを選んでください\", \"options\": [{\"label\": \"GPT-4\", \"value\": \"gpt-4\"}, {\"label\": \"Claude\", \"value\": \"claude\"}], \"action\": {\"name\": \"select_model\"}}]}\n\n使用例（フォーム/モーダル）:\n{\"components\": [{\"id\": \"col1\", \"component\": \"Column\", \"children\": [\"txt1\", \"row1\"]}, {\"id\": \"txt1\", \"component\": \"Text\", \"text\": \"設定を変更\"}, {\"id\": \"row1\", \"component\": \"Row\", \"children\": [\"trigger_btn\"]}, {\"id\": \"trigger_btn\", \"component\": \"Button\", \"text\": \"設定を開く\", \"style\": \"primary\", \"action\": {\"name\": \"submit_form\"}}, {\"id\": \"form1\", \"component\": \"Form\", \"title\": \"設定変更\", \"children\": [\"input_name\", \"input_desc\"], \"action\": {\"name\": \"submit_form\"}}, {\"id\": \"input_name\", \"component\": \"TextInput\", \"label\": \"名前\", \"placeholder\": \"名前を入力\", \"style\": \"short\", \"required\": true}, {\"id\": \"input_desc\", \"component\": \"TextInput\", \"label\": \"説明\", \"placeholder\": \"説明を入力\", \"style\": \"paragraph\", \"required\": false}]}\n\n注意: Rowのchildrenで参照するButton/SelectMenuはトップレベルのcomponents配列に定義する。各Buttonには一意のidとaction（name + context）を設定する。SelectMenuの選択結果はcontextのselected_valuesとして届く。Formはモーダル表示用で、同じaction.nameのトリガーボタンが必要。",
        "input_schema": send_ui_input_schema(),
        "output_schema": null,
        "callback_schema": null,
        "authorization": {"allowed_callers": ["co_agent", "guest", "owner", "trusted"]},
        "dispatch": "inline",
        "sub_engine": "blocked",
        "sharing": "conversation_bound",
        "effect": "state_change",
    })
}

/// The `send_ui` arguments (split into several `json!` calls to stay under the macro recursion limit).
fn send_ui_input_schema() -> Value {
    let str_ = |d: &str| json!({"type": "string", "description": d});
    let option = json!({"type": "object", "required": ["label", "value"], "properties": {
        "label": {"type": "string"},
        "value": {"type": "string"},
        "description": {"type": "string"},
        "emoji": {"type": "string"},
        "default": {"type": "boolean"}
    }});
    let action = json!({"type": "object", "properties": {
        "name": {"type": "string"},
        "context": {"type": "object"}
    }});
    let mut props = serde_json::Map::new();
    for (key, value) in [
        ("id", json!({"type": "string"})),
        (
            "component",
            json!({"type": "string", "enum": ["Text", "Button", "Row", "Column", "SelectMenu", "TextInput", "Form"]}),
        ),
        ("text", json!({"type": "string"})),
        ("variant", json!({"type": "string"})),
        (
            "label",
            json!({"type": "string", "description": "TextInputのラベル"}),
        ),
        (
            "title",
            json!({"type": "string", "description": "Formのタイトル"}),
        ),
        ("action", action),
        ("style", json!({"type": "string"})),
        ("emoji", json!({"type": "string"})),
        (
            "children",
            json!({"type": "array", "items": {"type": "string"}}),
        ),
        (
            "options",
            json!({"type": "array", "description": "SelectMenuの選択肢", "items": option}),
        ),
        ("placeholder", json!({"type": "string"})),
        ("min_values", json!({"type": "integer"})),
        ("max_values", json!({"type": "integer"})),
        ("min_length", json!({"type": "integer"})),
        ("max_length", json!({"type": "integer"})),
        ("required", json!({"type": "boolean"})),
    ] {
        props.insert(key.to_string(), value);
    }
    let item = json!({"type": "object", "required": ["id", "component"], "properties": props});
    json!({
        "type": "object",
        "required": ["components"],
        "properties": {
            "channel_id": str_("送信先チャンネルID。通常は省略する（この会話のチャンネルに送る）。推測した識別子を渡してはならない。"),
            "components": {"type": "array", "description": "A2UI v0.9 コンポーネント配列", "items": item},
            "timeout_secs": {"type": "integer", "description": "タイムアウト秒数（デフォルト: 300、10〜3600）"},
            "owner_only": {"type": "boolean", "description": "オーナーのみ操作可能か（デフォルト: true。現在は常にオーナーのみ）"}
        }
    })
}

/// The instance's A2UI: needs a real Discord connection and the gateway DB (the placement's
/// `database_path`). Without either, `send_ui` answers that UI is not available.
pub fn for_instance(
    cfg: &crate::config::InstanceConfig,
    http: Option<&Arc<serenity::http::Http>>,
    database_path: Option<&std::path::Path>,
) -> Option<Arc<A2ui>> {
    let (http, path) = (http?, database_path?);
    match A2uiStore::open(path) {
        Ok(store) => Some(A2ui::new(
            cfg.agent_id.clone(),
            cfg.access.clone(),
            store,
            Arc::new(discord::SerenityA2ui::new(http.clone())),
        )),
        Err(error) => {
            tracing::warn!(error = %error, "a2ui store unavailable; send_ui is disabled");
            None
        }
    }
}

/// What one instance needs to run A2UI.
pub struct A2ui {
    pub agent_id: String,
    pub access: AccessConfig,
    pub store: A2uiStore,
    pub discord: Arc<dyn A2uiDiscord>,
    client: OnceLock<Arc<InstanceClient>>,
}

impl A2ui {
    pub fn new(
        agent_id: String,
        access: AccessConfig,
        store: A2uiStore,
        discord: Arc<dyn A2uiDiscord>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_id,
            access,
            store,
            discord,
            client: OnceLock::new(),
        })
    }

    pub fn attach_client(&self, client: Arc<InstanceClient>) {
        let _ = self.client.set(client);
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    /// The `send_ui` invoke: render, post, store, arm the timeout.
    pub async fn send_ui(self: &Arc<Self>, session_key: &str, payload: &Value) -> InvokeOutcome {
        let fail = |e: String| InvokeOutcome::Ok(json!({"ok": false, "error": e}));
        let channel_id = match payload.get("channel_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(Value::String(s)) if s.bytes().all(|b| b.is_ascii_digit()) => Some(s.clone()),
            Some(_) => return fail("channel_id はチャンネルの数字IDです".into()),
        };
        let channel_id = match channel_id.or_else(|| {
            crate::map::parse_address(&self.agent_id, session_key).map(|(_, channel)| channel)
        }) {
            Some(c) => c,
            None => return fail("この会話の送信先チャンネルが分かりません".into()),
        };
        let Some(components_value) = payload.get("components") else {
            return fail("componentsパラメータが必要です".into());
        };
        let components = match render::parse_components(components_value) {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let timeout_secs = payload
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(10, 3600);
        let ui_id = uuid::Uuid::new_v4().to_string();
        let rows = match render::action_rows(&ui_id, &components) {
            Ok(rows) => rows,
            Err(e) => return fail(format!("レンダリングエラー: {e}")),
        };
        let content = render::message_text(&components);
        let mut body = json!({});
        if !content.is_empty() {
            body["content"] = json!(content);
        }
        if !rows.is_empty() {
            body["components"] = json!(rows);
        }
        let ui = PendingUi {
            ui_id: ui_id.clone(),
            agent_id: self.agent_id.clone(),
            session_key: session_key.to_string(),
            channel_id: channel_id.clone(),
            message_id: None,
            components_json: components_value.to_string(),
            expires_at_ms: Self::now_ms() + (timeout_secs as i64) * 1000,
        };
        if let Err(e) = self.store.insert(&ui) {
            return fail(format!("保存に失敗: {e:#}"));
        }
        let message_id = match self.discord.send(&channel_id, &body).await {
            Ok(id) => id,
            Err(e) => {
                let _ = self.store.take(&ui_id);
                tracing::warn!(error = %e, "send_ui post failed");
                return fail(format!("送信に失敗: {e}"));
            }
        };
        if let Err(e) = self.store.set_message_id(&ui_id, &message_id) {
            tracing::warn!(error = %e, "send_ui message id not stored");
        }
        self.arm_timeout(PendingUi {
            message_id: Some(message_id.clone()),
            ..ui
        });
        tracing::info!(%ui_id, %channel_id, timeout_secs, "A2UI interaction registered");
        InvokeOutcome::Ok(json!({
            "interaction_id": ui_id,
            "surface_id": format!("interaction:{ui_id}"),
            "message_id": message_id,
            "status": "pending",
            "message": "UIを送信しました。ユーザーの応答を待機中...",
        }))
    }

    /// Times the UI out at its expiry (now, if it already passed — e.g. while the gateway was down).
    pub fn arm_timeout(self: &Arc<Self>, ui: PendingUi) {
        let this = self.clone();
        tokio::spawn(async move {
            let wait = (ui.expires_at_ms - Self::now_ms()).max(0) as u64;
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            this.expire(&ui.ui_id).await;
        });
    }

    /// Re-arms the timers of this agent's pending UIs after a start.
    pub fn resume_pending(self: &Arc<Self>) {
        match self.store.list_for_agent(&self.agent_id) {
            Ok(list) => {
                for ui in list {
                    self.arm_timeout(ui);
                }
            }
            Err(e) => tracing::warn!(error = %e, "a2ui pending list failed"),
        }
    }

    /// The timeout: only when the UI is still pending (first answer wins).
    pub async fn expire(&self, ui_id: &str) {
        let Ok(Some(ui)) = self.store.take(ui_id) else {
            return;
        };
        self.close_message(&ui, true).await;
        let text = format!(
            "[UI] タイムアウトしました（誰も操作しませんでした） (ui_id={ui_id})\n\n{}",
            response_block(ui_id, "_timeout", "timeout", None, "system")
        );
        self.post(
            &ui,
            &format!("a2ui:{ui_id}:timeout"),
            "system",
            Some("system"),
            SaidCaller::Agent,
            &text,
        )
        .await;
    }

    /// Disables the components (and appends the timeout note) by re-rendering the stored tree.
    async fn close_message(&self, ui: &PendingUi, timed_out: bool) {
        let Some(message_id) = &ui.message_id else {
            return;
        };
        let Ok(components) = serde_json::from_str::<Value>(&ui.components_json)
            .map_err(|e| e.to_string())
            .and_then(|v| render::parse_components(&v))
        else {
            return;
        };
        let rows = render::action_rows(&ui.ui_id, &components).unwrap_or_default();
        let mut body = json!({"components": render::disabled_rows(&json!(rows))});
        if timed_out {
            let content = render::message_text(&components);
            body["content"] = json!(if content.is_empty() {
                TIMEOUT_NOTE.to_string()
            } else {
                format!("{content}\n\n{TIMEOUT_NOTE}")
            });
        }
        if let Err(e) = self.discord.edit(&ui.channel_id, message_id, &body).await {
            tracing::warn!(error = %e, ui_id = %ui.ui_id, "a2ui message edit failed");
        }
    }

    async fn post(
        &self,
        ui: &PendingUi,
        origin: &str,
        author_id: &str,
        author_label: Option<&str>,
        caller: SaidCaller,
        text: &str,
    ) {
        let Some(client) = self.client.get() else {
            tracing::warn!(ui_id = %ui.ui_id, "a2ui answer dropped: no core client");
            return;
        };
        let context = SaidContext {
            caller,
            start_turn: true,
            system_context: None,
            reply_target: None,
            reference: None,
            live_inbound_scope: LiveInboundScope::All,
            model_profile: opencrab_gate_client::wire::TurnModelProfile::Default,
            agent_prelude: None,
            provisional_id: None,
        };
        match client
            .post_said_with_context(
                &ui.session_key,
                origin,
                author_id,
                author_label,
                &context,
                text,
                &[],
            )
            .await
        {
            Ok(SaidOutcome::Accepted { seq }) => {
                tracing::info!(ui_id = %ui.ui_id, seq, "a2ui answer accepted")
            }
            other => {
                tracing::warn!(ui_id = %ui.ui_id, outcome = ?other, "a2ui answer not accepted")
            }
        }
    }

    /// One component interaction. Always answers Discord (no "interaction failed"); only an
    /// owner's first answer reaches the agent.
    pub async fn handle_interaction(&self, event: InteractionEvent) {
        let ack = json!({"type": 6});
        let Some((ui_id, component_id, action_name)) = render::parse_custom_id(&event.custom_id)
        else {
            return;
        };
        let pending = self.store.get(&ui_id).ok().flatten();
        let is_owner = self.access.owners.iter().any(|o| o == &event.user_id);
        let (Some(ui), true) = (pending, is_owner) else {
            // Expired, already answered, another bot's UI or not an owner: acknowledge only.
            self.respond(&event, &ack).await;
            return;
        };
        if ui.agent_id != self.agent_id {
            self.respond(&event, &ack).await;
            return;
        }
        let components = serde_json::from_str::<Value>(&ui.components_json)
            .ok()
            .and_then(|v| render::parse_components(&v).ok())
            .unwrap_or_default();
        if event.kind == InteractionKind::Button {
            if let Some(modal) = render::form_modal(&ui_id, &components, &component_id) {
                // The Form opens; the answer is the modal submit.
                self.respond(&event, &json!({"type": 9, "data": modal}))
                    .await;
                return;
            }
        }
        let Ok(Some(ui)) = self.store.take(&ui_id) else {
            self.respond(&event, &ack).await;
            return;
        };
        self.respond(&event, &ack).await;
        if event.kind != InteractionKind::Modal {
            self.close_message(&ui, false).await;
        }
        let who = event
            .user_label
            .clone()
            .unwrap_or_else(|| event.user_id.clone());
        let (headline, context) = match event.kind {
            InteractionKind::Button => {
                let label = render::label_of(&components, &component_id)
                    .unwrap_or_else(|| component_id.clone());
                (
                    format!("[ボタン] {who} が「{label}」を押した"),
                    render::button_context(&components, &component_id),
                )
            }
            InteractionKind::Select => {
                let labels = render::option_labels(&components, &component_id, &event.values);
                (
                    format!("[選択] {who} が「{}」を選んだ", labels.join("」「")),
                    Some(json!({"selected_values": event.values})),
                )
            }
            InteractionKind::Modal => {
                let mut map = serde_json::Map::new();
                for (k, v) in &event.fields {
                    map.insert(k.clone(), json!(v));
                }
                (
                    format!("[フォーム] {who} がフォームを送信した"),
                    Some(Value::Object(map)),
                )
            }
        };
        let text = format!(
            "{headline} (ui_id={ui_id})\n\n{}",
            response_block(
                &ui_id,
                &component_id,
                &action_name,
                context.as_ref(),
                &event.user_id
            )
        );
        let caller = crate::run::caller_for(&self.access, &event.user_id);
        self.post(
            &ui,
            &format!("a2ui:{ui_id}:{}", event.interaction_id),
            &event.user_id,
            event.user_label.as_deref(),
            caller,
            &text,
        )
        .await;
    }

    async fn respond(&self, event: &InteractionEvent, body: &Value) {
        if let Err(e) = self
            .discord
            .respond(&event.interaction_id, &event.token, body)
            .await
        {
            tracing::warn!(error = %e, "a2ui interaction response failed");
        }
    }
}

/// The legacy `[interaction_response]` block the agent received (session log text, D-1110 keeps it).
pub fn response_block(
    ui_id: &str,
    component_id: &str,
    action: &str,
    context: Option<&Value>,
    responder: &str,
) -> String {
    format!(
        "[interaction_response] ユーザーがUIに応答しました。\nsurface_id: interaction:{ui_id}\ncomponent_id: {component_id}\naction: {action}\ncontext: {}\nresponder: {responder}",
        context.map(|c| c.to_string()).unwrap_or_default()
    )
}

#[cfg(test)]
mod tests;

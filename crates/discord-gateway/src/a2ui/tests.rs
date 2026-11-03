//! D-1110: A2UI restored in the Discord gateway. The declared operation, rendering, who may click,
//! the first answer only, the timeout, and the pending store surviving a restart.

use super::*;
use opencrab_gate_client::wire::{read_frame, write_json};
use std::sync::Mutex;
use std::time::Duration;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

const AGENT: &str = "agent";
const BOT: &str = "555555555555555555";
const OWNER: &str = "222222222222222222";
const STRANGER: &str = "444444444444444444";
const INSTANCE: &str = "11111111-1111-4111-8111-111111111111";

/// Records the Discord calls.
#[derive(Default)]
struct FakeDiscord {
    sent: Mutex<Vec<(String, Value)>>,
    edits: Mutex<Vec<(String, String, Value)>>,
    responses: Mutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl A2uiDiscord for FakeDiscord {
    async fn send(&self, channel_id: &str, body: &Value) -> Result<String, String> {
        self.sent
            .lock()
            .unwrap()
            .push((channel_id.to_string(), body.clone()));
        Ok("900".to_string())
    }
    async fn edit(&self, channel_id: &str, message_id: &str, body: &Value) -> Result<(), String> {
        self.edits.lock().unwrap().push((
            channel_id.to_string(),
            message_id.to_string(),
            body.clone(),
        ));
        Ok(())
    }
    async fn respond(&self, interaction_id: &str, _t: &str, body: &Value) -> Result<(), String> {
        self.responses
            .lock()
            .unwrap()
            .push((interaction_id.to_string(), body.clone()));
        Ok(())
    }
}

fn access() -> AccessConfig {
    AccessConfig {
        owners: vec![OWNER.into()],
        ..AccessConfig::default()
    }
}

fn session() -> String {
    crate::map::address_for(AGENT, "100", "200")
}

fn two_buttons() -> Value {
    json!({"components": [
        {"id": "txt", "component": "Text", "text": "どうする？"},
        {"id": "row", "component": "Row", "children": ["ok", "ng"]},
        {"id": "ok", "component": "Button", "text": "承認", "style": "success",
         "action": {"name": "confirm", "context": {"value": true}}},
        {"id": "ng", "component": "Button", "text": "却下", "style": "danger",
         "action": {"name": "confirm", "context": {"value": false}}}
    ]})
}

struct Core {
    _dir: tempfile::TempDir,
    reader: OwnedReadHalf,
    writer: OwnedWriteHalf,
}

/// A fake core on a real socket; the returned client is attached to `a2ui`.
async fn core_for(a2ui: &Arc<A2ui>) -> Core {
    let dir = tempfile::Builder::new()
        .prefix("a2")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path().join("core.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let p = path.clone();
    let connect = tokio::spawn(async move {
        InstanceClient::connect(&p, INSTANCE.into(), AGENT.into(), BOT.into())
            .await
            .unwrap()
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut reader, mut writer) = stream.into_split();
    let hello: Value = serde_json::from_slice(&read_frame(&mut reader).await.unwrap()).unwrap();
    write_json(&mut writer, &json!({"id": hello["id"], "m": "ok"}))
        .await
        .unwrap();
    a2ui.attach_client(connect.await.unwrap());
    Core {
        _dir: dir,
        reader,
        writer,
    }
}

impl Core {
    /// The next said, answered `ok`; `None` if nothing comes within `wait`.
    async fn said(&mut self, wait: Duration) -> Option<Value> {
        let frame = tokio::time::timeout(wait, read_frame(&mut self.reader))
            .await
            .ok()?
            .unwrap();
        let said: Value = serde_json::from_slice(&frame).unwrap();
        write_json(
            &mut self.writer,
            &json!({"id": said["id"], "m": "ok", "seq": 1, "session_id": said["session"]}),
        )
        .await
        .unwrap();
        Some(said)
    }
}

fn a2ui_with(store: A2uiStore) -> (Arc<A2ui>, Arc<FakeDiscord>) {
    let discord = Arc::new(FakeDiscord::default());
    (
        A2ui::new(AGENT.into(), access(), store, discord.clone()),
        discord,
    )
}

fn click(ui_id: &str, component: &str, action: &str, user: &str, n: u32) -> InteractionEvent {
    InteractionEvent {
        event_kind: event::A2UI_EVENT_KIND.into(),
        interaction_id: format!("{}", 700 + n),
        token: "tok".into(),
        custom_id: render::custom_id(ui_id, component, action),
        channel_id: "200".into(),
        message_id: Some("900".into()),
        user_id: user.into(),
        user_label: Some(if user == OWNER { "kojira" } else { "someone" }.into()),
        kind: InteractionKind::Button,
        values: Vec::new(),
        fields: Vec::new(),
    }
}

async fn send(a2ui: &Arc<A2ui>, payload: Value) -> Value {
    match a2ui.send_ui(&session(), &payload).await {
        InvokeOutcome::Ok(v) => v,
        _ => panic!("send_ui is answered with a result"),
    }
}

#[test]
fn d1110_send_ui_is_declared_with_the_legacy_arguments() {
    let decls = crate::ops::operation_declarations(false);
    let names: Vec<&str> = decls
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"send_ui"), "{names:?}");
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "operations stay in name order");
    let d = send_ui_declaration();
    let props = &d["input_schema"]["properties"];
    for key in ["channel_id", "components", "timeout_secs", "owner_only"] {
        assert!(props.get(key).is_some(), "argument {key}");
    }
    assert_eq!(d["input_schema"]["required"], json!(["components"]));
    assert_eq!(
        props["components"]["items"]["properties"]["component"]["enum"],
        json!([
            "Text",
            "Button",
            "Row",
            "Column",
            "SelectMenu",
            "TextInput",
            "Form"
        ])
    );
    assert_eq!(
        d["sub_engine"], "blocked",
        "not from sub-engines, as before"
    );
}

#[tokio::test]
async fn d1110_send_ui_posts_text_and_buttons_into_the_session_channel() {
    let (a2ui, discord) = a2ui_with(A2uiStore::in_memory().unwrap());
    let result = send(&a2ui, two_buttons()).await;
    assert_eq!(result["status"], "pending");
    let ui_id = result["interaction_id"].as_str().unwrap().to_string();
    let sent = discord.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, "200", "the channel of the session");
    assert_eq!(sent[0].1["content"], "どうする？");
    let buttons = &sent[0].1["components"][0]["components"];
    assert_eq!(buttons[0]["label"], "承認");
    assert_eq!(buttons[0]["style"], 3);
    assert_eq!(buttons[1]["style"], 4);
    assert_eq!(
        buttons[0]["custom_id"],
        format!("interaction:{ui_id}:ok:confirm")
    );
    let stored = a2ui.store.get(&ui_id).unwrap().unwrap();
    assert_eq!(stored.message_id.as_deref(), Some("900"));
    assert_eq!(stored.session_key, session());
}

#[tokio::test]
async fn d1110_an_owner_click_is_one_said_and_disables_the_buttons() {
    let (a2ui, discord) = a2ui_with(A2uiStore::in_memory().unwrap());
    let mut core = core_for(&a2ui).await;
    let ui_id = send(&a2ui, two_buttons()).await["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();

    let task = {
        let a2ui = a2ui.clone();
        let ev = click(&ui_id, "ok", "confirm", OWNER, 1);
        tokio::spawn(async move { a2ui.handle_interaction(ev).await })
    };
    let said = core.said(Duration::from_secs(5)).await.expect("a said");
    task.await.unwrap();
    assert_eq!(said["m"], "said");
    assert_eq!(said["session"], session());
    assert_eq!(said["author_id"], OWNER);
    let text = said["text"].as_str().unwrap();
    assert!(
        text.starts_with(&format!(
            "[ボタン] kojira が「承認」を押した (ui_id={ui_id})"
        )),
        "{text}"
    );
    assert!(text.contains("action: confirm"), "{text}");
    assert!(text.contains(r#"context: {"value":true}"#), "{text}");
    assert!(text.contains(&format!("responder: {OWNER}")), "{text}");

    let edits = discord.edits.lock().unwrap().clone();
    assert_eq!(edits.len(), 1);
    let rows = &edits[0].2["components"];
    assert_eq!(rows[0]["components"][0]["disabled"], true);
    assert_eq!(rows[0]["components"][1]["disabled"], true);
    assert_eq!(
        discord.responses.lock().unwrap()[0].1["type"],
        6,
        "acknowledged"
    );

    // A second click: acknowledged, nothing reaches the agent.
    a2ui.handle_interaction(click(&ui_id, "ng", "confirm", OWNER, 2))
        .await;
    assert!(core.said(Duration::from_millis(300)).await.is_none());
    assert_eq!(discord.responses.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn d1110_a_non_owner_click_is_only_acknowledged() {
    let (a2ui, discord) = a2ui_with(A2uiStore::in_memory().unwrap());
    let mut core = core_for(&a2ui).await;
    let ui_id = send(&a2ui, two_buttons()).await["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    a2ui.handle_interaction(click(&ui_id, "ok", "confirm", STRANGER, 1))
        .await;
    assert!(core.said(Duration::from_millis(300)).await.is_none());
    assert!(discord.edits.lock().unwrap().is_empty());
    assert!(
        a2ui.store.get(&ui_id).unwrap().is_some(),
        "still open for the owner"
    );
}

#[tokio::test]
async fn d1110_a_select_reports_the_chosen_values() {
    let (a2ui, _) = a2ui_with(A2uiStore::in_memory().unwrap());
    let mut core = core_for(&a2ui).await;
    let ui_id = send(
        &a2ui,
        json!({"components": [
            {"id": "sel", "component": "SelectMenu", "placeholder": "モデル",
             "options": [{"label": "GPT", "value": "gpt"}, {"label": "Claude", "value": "claude"}],
             "action": {"name": "pick"}}
        ]}),
    )
    .await["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut ev = click(&ui_id, "sel", "pick", OWNER, 1);
    ev.kind = InteractionKind::Select;
    ev.values = vec!["claude".into()];
    let task = {
        let a2ui = a2ui.clone();
        tokio::spawn(async move { a2ui.handle_interaction(ev).await })
    };
    let said = core.said(Duration::from_secs(5)).await.unwrap();
    task.await.unwrap();
    let text = said["text"].as_str().unwrap();
    assert!(
        text.starts_with("[選択] kojira が「Claude」を選んだ"),
        "{text}"
    );
    assert!(
        text.contains(r#"context: {"selected_values":["claude"]}"#),
        "{text}"
    );
}

#[tokio::test]
async fn d1110_the_timeout_disables_the_ui_and_tells_the_agent_once() {
    let (a2ui, discord) = a2ui_with(A2uiStore::in_memory().unwrap());
    let mut core = core_for(&a2ui).await;
    let ui_id = send(&a2ui, two_buttons()).await["interaction_id"]
        .as_str()
        .unwrap()
        .to_string();
    let task = {
        let a2ui = a2ui.clone();
        let id = ui_id.clone();
        tokio::spawn(async move { a2ui.expire(&id).await })
    };
    let said = core.said(Duration::from_secs(5)).await.unwrap();
    task.await.unwrap();
    let text = said["text"].as_str().unwrap();
    assert!(text.contains("タイムアウト"), "{text}");
    assert!(text.contains("action: timeout"), "{text}");
    let edits = discord.edits.lock().unwrap().clone();
    assert_eq!(
        edits[0].2["content"],
        format!("どうする？\n\n{TIMEOUT_NOTE}")
    );
    assert_eq!(
        edits[0].2["components"][0]["components"][0]["disabled"],
        true
    );
    // After the timeout a click does nothing.
    a2ui.handle_interaction(click(&ui_id, "ok", "confirm", OWNER, 1))
        .await;
    assert!(core.said(Duration::from_millis(300)).await.is_none());
}

#[tokio::test]
async fn d1110_a_pending_ui_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("gateway.db");
    let ui_id = {
        let (a2ui, _) = a2ui_with(A2uiStore::open(&db).unwrap());
        send(&a2ui, two_buttons()).await["interaction_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    // A new process opens the same DB: the click still reaches the agent.
    let (a2ui, _) = a2ui_with(A2uiStore::open(&db).unwrap());
    let mut core = core_for(&a2ui).await;
    let task = {
        let a2ui = a2ui.clone();
        let ev = click(&ui_id, "ok", "confirm", OWNER, 1);
        tokio::spawn(async move { a2ui.handle_interaction(ev).await })
    };
    let said = core.said(Duration::from_secs(5)).await.unwrap();
    task.await.unwrap();
    assert!(said["text"].as_str().unwrap().contains("「承認」を押した"));
}

#[tokio::test]
async fn d1110_a_ui_that_expired_while_down_is_timed_out_on_start() {
    let store = A2uiStore::in_memory().unwrap();
    store
        .insert(&PendingUi {
            ui_id: "u1".into(),
            agent_id: AGENT.into(),
            session_key: session(),
            channel_id: "200".into(),
            message_id: Some("900".into()),
            components_json: two_buttons()["components"].to_string(),
            expires_at_ms: chrono::Utc::now().timestamp_millis() - 1000,
        })
        .unwrap();
    let (a2ui, discord) = a2ui_with(store);
    let mut core = core_for(&a2ui).await;
    a2ui.resume_pending();
    let said = core.said(Duration::from_secs(5)).await.unwrap();
    assert!(said["text"].as_str().unwrap().contains("action: timeout"));
    assert_eq!(discord.edits.lock().unwrap().len(), 1);
    assert!(a2ui.store.get("u1").unwrap().is_none());
}

#[test]
fn d1110_rendering_follows_the_legacy_limits() {
    let comps = render::parse_components(&json!([
        {"id": "col", "component": "Column", "children": ["h", "row"]},
        {"id": "h", "component": "Text", "text": "見出し", "variant": "h2"},
        {"id": "row", "component": "Row", "children": ["b1","b2","b3","b4","b5","b6"]},
        {"id": "b1", "component": "Button", "text": "x".repeat(90), "action": {"name": "a"}},
        {"id": "b2", "component": "Button", "text": "2", "action": {"name": "a"}},
        {"id": "b3", "component": "Button", "text": "3", "action": {"name": "a"}},
        {"id": "b4", "component": "Button", "text": "4", "action": {"name": "a"}},
        {"id": "b5", "component": "Button", "text": "5", "action": {"name": "a"}},
        {"id": "b6", "component": "Button", "text": "6", "action": {"name": "a"}}
    ]))
    .unwrap();
    assert_eq!(render::message_text(&comps), "## 見出し");
    let rows = render::action_rows("u", &comps).unwrap();
    assert_eq!(rows.len(), 2, "6 buttons → 5 + 1");
    let label = rows[0]["components"][0]["label"].as_str().unwrap();
    assert_eq!(label.chars().count(), 80);
    assert!(label.ends_with("..."));
}

#[test]
fn d1110_a_form_button_opens_its_modal() {
    let comps = render::parse_components(&json!([
        {"id": "row", "component": "Row", "children": ["open"]},
        {"id": "open", "component": "Button", "text": "開く", "action": {"name": "submit"}},
        {"id": "form", "component": "Form", "title": "設定", "children": ["name"], "action": {"name": "submit"}},
        {"id": "name", "component": "TextInput", "label": "名前", "style": "paragraph"}
    ]))
    .unwrap();
    let modal = render::form_modal("u", &comps, "open").unwrap();
    assert_eq!(modal["custom_id"], "interaction:u:modal:submit");
    assert_eq!(modal["title"], "設定");
    assert_eq!(modal["components"][0]["components"][0]["custom_id"], "name");
    assert_eq!(modal["components"][0]["components"][0]["style"], 2);
}

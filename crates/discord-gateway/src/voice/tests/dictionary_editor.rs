//! D-1106 dictionary editor: the settings pages look up a word's reading, moras and accent through
//! ZEN-TTS (`analyze`) and preview one entry with a one-off dictionary (`test-tts` `dict`) that is
//! never saved. The editor sets the accent by tapping a mora; there is no number to type.
use serde_json::json;

use super::mock_http;
use super::zen_tts::spawn_zen;
use crate::voice::settings::load_settings;

/// VOICEVOX by default; `agent-zen` reads with ZEN-TTS `kikka`. Shared dictionary: AI→エーアイ.
fn settings_json(voicevox: &str, zen: Option<&str>) -> String {
    let mut v: serde_json::Value =
        serde_json::from_str(&mock_http::settings_json(voicevox)).unwrap();
    if let Some(zen) = zen {
        v["tts"]["zen"] = json!({"base_url": zen, "default_voice": "marine"});
        v["tts"]["agents"] = json!({"agent-zen": {"provider": "zen-tts", "voice": "kikka"}});
    }
    v["tts"]["dictionary"] = json!({"shared": [{"surface": "AI", "reading": "エーアイ"}]});
    v.to_string()
}

async fn serve(settings: String) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, settings).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(crate::voice::settings_http::serve_on(listener, path));
    (format!("http://{addr}"), dir)
}

#[tokio::test]
async fn d1106_analyze_returns_the_reading_with_the_bots_zen_voice() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (zen, recorded) = spawn_zen().await;
    let (base, _dir) = serve(settings_json(&voicevox, Some(&zen))).await;
    let http = reqwest::Client::new();
    let url = format!("{base}/api/voice-settings/analyze");

    let got = http
        .post(&url)
        .json(&json!({"text": "明日", "agent": "agent-zen"}))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    assert_eq!(
        got.json::<serde_json::Value>().await.unwrap(),
        json!({"reading": "アシタ", "moras": ["ア", "シ", "タ"], "accent": 3})
    );
    // A VOICEVOX bot (or no bot) is looked up with the default ZEN voice; an edited reading is sent
    // as a one-word dictionary so the moras follow it.
    let edited = http
        .post(&url)
        .json(&json!({"text": "明日", "agent": "agent", "dict": [{"surface": "明日", "reading": "アス"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        edited.json::<serde_json::Value>().await.unwrap()["moras"],
        json!(["ア", "ス"])
    );
    assert_eq!(
        recorded.analyze_requests.lock().unwrap().clone(),
        [
            json!({"text": "明日", "voice": "kikka"}),
            json!({"text": "明日", "voice": "marine", "dict": [{"surface": "明日", "reading": "アス"}]}),
        ]
    );

    // ZEN-TTS's 400 (a word it cannot read) reaches the page with its reason.
    let bad = http
        .post(&url)
        .json(&json!({"text": "？？"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(
        bad.json::<serde_json::Value>().await.unwrap()["error"],
        "cannot read this word"
    );
}

#[tokio::test]
async fn d1106_analyze_is_404_without_zen() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (base, _dir) = serve(settings_json(&voicevox, None)).await;
    let res = reqwest::Client::new()
        .post(format!("{base}/api/voice-settings/analyze"))
        .json(&json!({"text": "明日"}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    assert!(res.json::<serde_json::Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .contains("ZEN-TTS"));
}

#[tokio::test]
async fn d1106_preview_dict_reaches_zen_once_and_is_not_saved() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (zen, recorded) = spawn_zen().await;
    let (base, dir) = serve(settings_json(&voicevox, Some(&zen))).await;
    let path = dir.path().join("settings.json");
    let before = std::fs::read_to_string(&path).unwrap();
    let http = reqwest::Client::new();
    let url = format!("{base}/api/voice-settings/test-tts");
    let entry = json!({"surface": "明日", "reading": "アス", "accent": 1});

    // The bot page: the bot's voice, the saved dictionary plus this entry.
    let res = http
        .post(&url)
        .json(&json!({"text": "明日です。", "agent": "agent-zen", "dict": [entry]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "audio/wav");
    // The shared dictionary page (no bot): the default ZEN voice.
    let res = http
        .post(&url)
        .json(&json!({"text": "明日です。", "dict": [entry]}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    // The next ordinary play does not carry the preview entry.
    http.post(&url)
        .json(&json!({"text": "明日です。", "agent": "agent-zen"}))
        .send()
        .await
        .unwrap();

    let sent = recorded.requests.lock().unwrap().clone();
    let merged = json!([{"surface": "AI", "reading": "エーアイ"}, entry]);
    assert_eq!(
        sent,
        [
            json!({"text": "明日です。", "voice": "kikka", "dict": merged}),
            json!({"text": "明日です。", "voice": "marine", "dict": merged}),
            json!({"text": "明日です。", "voice": "kikka", "dict": [{"surface": "AI", "reading": "エーアイ"}]}),
        ]
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert!(load_settings(&path)
        .unwrap()
        .tts
        .dictionary
        .agents
        .is_empty());

    // A malformed preview entry is refused, not sent.
    let bad = http
        .post(&url)
        .json(
            &json!({"text": "x", "agent": "agent-zen", "dict": [{"surface": "", "reading": "ア"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert_eq!(recorded.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn d1106_pages_load_the_mora_editor_without_a_numeric_accent_input() {
    let (voicevox, _vv) = mock_http::spawn("x").await;
    let (base, _dir) = serve(settings_json(&voicevox, None)).await;
    let js = reqwest::get(format!("{base}/dict-editor.js"))
        .await
        .unwrap();
    assert_eq!(js.status(), 200);
    assert!(js.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/javascript"));
    let js = js.text().await.unwrap();
    // Mora boxes (tap = the drop after that mora), 平板 / おまかせ, look-up and ▶.
    for needed in [
        "dict-moras",
        "mora",
        "平板",
        "おまかせ",
        "読みを調べる",
        "/analyze",
        "test-tts",
    ] {
        assert!(js.contains(needed), "editor lacks {needed}");
    }
    for page in [
        crate::voice::settings_http::PAGE,
        crate::settings_http::PAGE,
        crate::voice::settings_http::DICT_EDITOR_JS,
    ] {
        assert!(!page.contains("dict-accent"), "no numeric accent input");
        assert!(!page.contains("0 = 平板"), "no accent number explained");
    }
}

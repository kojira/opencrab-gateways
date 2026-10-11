use super::mock_http;
use crate::voice::settings::load_settings;
use crate::voice::settings_http::serve_on;

async fn start(path: std::path::PathBuf) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_on(listener, path));
    format!("http://{addr}")
}

#[tokio::test]
async fn get_put_round_trip_with_validation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("voice").join("settings.json");
    let base = start(path.clone()).await;
    let http = reqwest::Client::new();

    let page = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    // 相対パスで呼ぶ（dashboard の proxy 配下に置いても動くように）。
    assert!(page
        .text()
        .await
        .unwrap()
        .contains(r#""api/voice-settings""#));

    let missing = http
        .get(format!("{base}/api/voice-settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404, "未設定は 404（既定値で埋めない）");

    let invalid = http
        .put(format!("{base}/api/voice-settings"))
        .header("content-type", "application/json")
        .body(r#"{"stt":{"base_url":"ftp://x","model":"m"},"tts":{"provider":"voicevox","base_url":"http://127.0.0.1:1","default_voice":"3"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);
    assert!(!path.exists(), "不正な設定は書かない");

    let body = mock_http::settings_json("http://127.0.0.1:50021");
    let put = http
        .put(format!("{base}/api/voice-settings"))
        .header("content-type", "application/json")
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 200);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let saved = load_settings(&path).unwrap();
    assert_eq!(saved.tts.default_voice, "3");

    let got: serde_json::Value = http
        .get(format!("{base}/api/voice-settings"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        got,
        serde_json::from_str::<serde_json::Value>(&body).unwrap()
    );
}

#[tokio::test]
async fn speakers_and_test_buttons_use_the_saved_endpoints() {
    let (mock, recorded) = mock_http::spawn("音声認識のテストです").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let base = start(path.clone()).await;
    let http = reqwest::Client::new();

    // 未設定ではエンドポイントへ行かない。
    let unconfigured = http
        .get(format!("{base}/api/voice-settings/speakers"))
        .send()
        .await
        .unwrap();
    assert_eq!(unconfigured.status(), 409);

    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let speakers: serde_json::Value = http
        .get(format!("{base}/api/voice-settings/speakers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(speakers[0]["styles"][0]["id"], 3);

    let wav = http
        .post(format!("{base}/api/voice-settings/test-tts"))
        .header("content-type", "application/json")
        .body(r#"{"text":"こんにちは","voice":"8"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(wav.status(), 200);
    assert_eq!(wav.headers()["content-type"], "audio/wav");
    assert_eq!(wav.bytes().await.unwrap().as_ref(), mock_http::WAV_FROM_TTS);
    assert_eq!(
        recorded.tts_queries.lock().unwrap().last().unwrap(),
        &("こんにちは".to_string(), "8".to_string())
    );

    let stt: serde_json::Value = http
        .post(format!("{base}/api/voice-settings/test-stt"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stt["text"], "音声認識のテストです");
    assert_eq!(recorded.stt_bodies.lock().unwrap().len(), 1);
}

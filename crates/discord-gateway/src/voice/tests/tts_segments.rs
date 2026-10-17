//! 長い投稿を文ごとに合成し、合成できた文から順に再生する（D-1072、全文の合成待ちを避ける）。
use std::sync::Arc;

use super::e2e::{core, manager_with, wait_played, FakePlayer, BINDING};
use super::mock_http;
use crate::transport::DryRunTransport;

#[tokio::test]
async fn long_post_is_synthesized_and_played_sentence_by_sentence() {
    let (mock, recorded) = mock_http::spawn_with_synthesis_delay(vec![(0, "unused")], 150).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager_with(&core, path, player.clone(), Arc::new(DryRunTransport));
    m.join(BINDING, Some("555"), None).await.unwrap();

    m.speak_posted("10", "一文目です。二文目です！三文目？");
    wait_played(&player, 3).await;

    let queries = recorded.tts_queries.lock().unwrap().clone();
    assert_eq!(
        queries,
        vec![
            ("一文目です。".to_string(), "8".to_string()),
            ("二文目です！".to_string(), "8".to_string()),
            ("三文目？".to_string(), "8".to_string()),
        ]
    );
    let started = recorded.synthesis_started.lock().unwrap().clone();
    assert_eq!(started.len(), 3);
    let played_at = player.played_at.lock().unwrap().clone();
    assert_eq!(played_at.len(), 3);
    assert!(
        played_at[0] < started[2],
        "first sentence must be enqueued before the last one is synthesized"
    );
}

#[tokio::test]
async fn multi_line_post_is_synthesized_line_by_line() {
    let (mock, recorded) = mock_http::spawn_with_synthesis_delay(vec![(0, "unused")], 0).await;
    let core = core().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, mock_http::settings_json(&mock)).unwrap();
    let player = Arc::new(FakePlayer::default());
    let m = manager_with(&core, path, player.clone(), Arc::new(DryRunTransport));
    m.join(BINDING, Some("555"), None).await.unwrap();

    m.speak_posted("10", "一行目\n二行目\n三行目");
    wait_played(&player, 1).await;
    // 1 行ずつ合成されるなら 3 件届く。届かなければ届いた分で比較して差分を示す。
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while recorded.tts_queries.lock().unwrap().len() < 3 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;

    let texts: Vec<String> = recorded
        .tts_queries
        .lock()
        .unwrap()
        .iter()
        .map(|(text, _)| text.clone())
        .collect();
    assert_eq!(texts, vec!["一行目", "二行目", "三行目"]);
}

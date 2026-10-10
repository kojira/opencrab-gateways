use super::super::settings::{
    load_settings, save_settings, settings_path_for_placement, VoiceSettings,
};
use serde_json::json;

fn valid() -> serde_json::Value {
    json!({
        "stt": {"base_url": "http://127.0.0.1:50022/v1", "model": "reazonspeech-k2-v2", "language": "ja"},
        "tts": {"provider": "voicevox", "base_url": "http://127.0.0.1:50021", "default_voice": "3",
                "agent_voices": {"agent-a": "8"}}
    })
}

#[test]
fn valid_settings_parse_and_resolve_agent_voice() {
    let s = VoiceSettings::from_json(&valid().to_string()).unwrap();
    assert_eq!(s.tts.voice_for_agent("agent-a"), "8");
    assert_eq!(s.tts.voice_for_agent("other"), "3");
    assert_eq!(s.stt.language.as_deref(), Some("ja"));
}

#[test]
fn invalid_settings_are_rejected() {
    let cases = [
        ("/stt/base_url", json!("ftp://x")),
        ("/stt/base_url", json!("")),
        ("/tts/base_url", json!("not a url")),
        ("/tts/provider", json!("openai")),
        ("/tts/default_voice", json!("zundamon")),
        ("/tts/agent_voices", json!({"agent-a": "x"})),
        ("/stt/model", json!("")),
    ];
    for (pointer, value) in cases {
        let mut v = valid();
        *v.pointer_mut(pointer).unwrap() = value.clone();
        assert!(
            VoiceSettings::from_json(&v.to_string()).is_err(),
            "{pointer}={value} must be rejected"
        );
    }
    let mut extra = valid();
    extra["stt"]["api_key"] = json!("x");
    assert!(
        VoiceSettings::from_json(&extra.to_string()).is_err(),
        "unknown field"
    );
}

#[test]
fn missing_file_is_not_configured_and_never_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let err = load_settings(&dir.path().join("settings.json")).unwrap_err();
    assert!(err.to_string().contains("not configured"), "{err}");
}

#[cfg(unix)]
#[test]
fn save_is_atomic_and_mode_0600() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("voice").join("settings.json");
    let s = VoiceSettings::from_json(&valid().to_string()).unwrap();
    save_settings(&path, &s).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(load_settings(&path).unwrap(), s);
    // 上書きも 0600 のまま、一時ファイルを残さない。
    let mut changed = s.clone();
    changed.tts.default_voice = "1".into();
    save_settings(&path, &changed).unwrap();
    assert_eq!(load_settings(&path).unwrap().tts.default_voice, "1");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["settings.json".to_string()]);
}

#[test]
fn settings_live_in_a_subdirectory_next_to_the_placement() {
    let p = settings_path_for_placement(std::path::Path::new("/x/data/gate/discord/agent.json"));
    assert_eq!(
        p,
        std::path::PathBuf::from("/x/data/gate/discord/voice/settings.json")
    );
}

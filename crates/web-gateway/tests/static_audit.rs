//! web-gateway の静的監査。core リポジトリの `webgate_static_audit` から gateway 側の
//! 検査だけを移した（Issue #1074）。

use std::fs;
use std::path::{Path, PathBuf};

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn production_sources() -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_rs(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty(), "web-gateway src is empty");
    files
}

fn hits(patterns: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    for path in production_sources() {
        let text = fs::read_to_string(&path).unwrap();
        if patterns.iter().any(|pattern| text.contains(pattern)) {
            hits.push(path.display().to_string());
        }
    }
    hits
}

#[test]
fn operator_bearer_is_absent_from_gateway() {
    let hits = hits(&["OPENCRAB_GATE_OPERATOR_TOKEN", "Authorization: Bearer"]);
    assert!(
        hits.is_empty(),
        "operator Bearer leaked into gateway:\n{}",
        hits.join("\n")
    );
}

#[test]
fn gateway_message_post_does_not_create_bindings() {
    let hits = hits(&["create_gate_binding_in_tx", "INSERT INTO gate_bindings"]);
    assert!(
        hits.is_empty(),
        "gateway message POST must not create bindings:\n{}",
        hits.join("\n")
    );
}

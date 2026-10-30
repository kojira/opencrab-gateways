use super::*;

// ==================== fixture（Discord Message JSONL） ====================

pub(crate) struct Fixture {
    pub(crate) path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();
        Self { path, _dir: dir }
    }
}

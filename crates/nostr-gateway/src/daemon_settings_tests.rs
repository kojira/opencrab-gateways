//! D-1097: a settings edit through the admin store reaches a running instance without any
//! manual restart. The daemon stops the old child and starts one with the new access.

use super::*;
use opencrab_process_supervisor::{ChildSpawner, SupervisedChild};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

const ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

struct Child {
    kills: Arc<AtomicUsize>,
    exit: Arc<Notify>,
}
#[async_trait]
impl SupervisedChild for Child {
    async fn wait_exit(&mut self) -> String {
        self.exit.notified().await;
        "exit".into()
    }
    async fn kill(&mut self) {
        self.kills.fetch_add(1, Ordering::SeqCst);
    }
    fn pid(&self) -> Option<u32> {
        Some(4242)
    }
}

struct Spawner {
    target: String,
    kills: Arc<AtomicUsize>,
    spawned: Arc<AtomicUsize>,
}
#[async_trait]
impl ChildSpawner for Spawner {
    async fn spawn(&self) -> std::io::Result<Box<dyn SupervisedChild>> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Child {
            kills: self.kills.clone(),
            exit: Arc::new(Notify::new()),
        }))
    }
    fn target_id(&self) -> &str {
        &self.target
    }
    fn service_name(&self) -> &str {
        "nostr-adapter"
    }
}

/// Records the access each started child was built with (what the production factory writes
/// into the child placement).
struct RecordingFactory {
    started_with: Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    kills: Arc<AtomicUsize>,
    spawned: Arc<AtomicUsize>,
}
impl SpawnerFactory for RecordingFactory {
    fn for_instance(
        &self,
        row: &InstanceRow,
        _: zeroize::Zeroizing<Vec<u8>>,
        _: &str,
    ) -> Result<Arc<dyn ChildSpawner>> {
        self.started_with
            .lock()
            .unwrap()
            .push(row.access.owner.clone());
        Ok(Arc::new(Spawner {
            target: row.instance_id.clone(),
            kills: self.kills.clone(),
            spawned: self.spawned.clone(),
        }))
    }
}

async fn until(mut check: impl FnMut() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn access_edit_restarts_the_child_with_the_new_owner() {
    let temp = tempfile::tempdir().unwrap();
    let store = NostrStore::open(&temp.path().join("nostr.db")).unwrap();
    store
        .upsert_desired(
            ID,
            "agent",
            "e30=",
            &["addr".into()],
            "nsec",
            true,
            &[9; 32],
        )
        .unwrap();
    store
        .set_access(
            ID,
            &config::AccessConfig {
                owner: vec!["11".repeat(32)],
                ..Default::default()
            },
        )
        .unwrap();
    let started_with = Arc::new(std::sync::Mutex::new(Vec::new()));
    let kills = Arc::new(AtomicUsize::new(0));
    let spawned = Arc::new(AtomicUsize::new(0));
    let daemon = NostrDaemon::new(
        store,
        [9; 32],
        Arc::new(RecordingFactory {
            started_with: started_with.clone(),
            kills: kills.clone(),
            spawned: spawned.clone(),
        }),
    )
    .unwrap();
    daemon.reconcile_once().await.unwrap();
    // Wait for the supervisor to actually run the first child before editing.
    until(|| spawned.load(Ordering::SeqCst) == 1).await;
    assert_eq!(started_with.lock().unwrap()[0], vec!["11".repeat(32)]);

    // The settings page edits access through the admin store while the child runs.
    daemon
        .store()
        .lock()
        .unwrap()
        .set_access(
            ID,
            &config::AccessConfig {
                owner: vec!["22".repeat(32)],
                ..Default::default()
            },
        )
        .unwrap();
    daemon.reconcile_once().await.unwrap();
    until(|| started_with.lock().unwrap().len() == 2).await;
    assert_eq!(
        started_with.lock().unwrap()[1],
        vec!["22".repeat(32)],
        "the new child must be built with the edited owner"
    );
    assert!(
        kills.load(Ordering::SeqCst) >= 1,
        "the old child must be stopped"
    );
    daemon.shutdown().await;
}

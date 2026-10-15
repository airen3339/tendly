//! The account manager: login processes in flight, and maintenance that
//! keeps two operations on one account from overlapping.

use super::*;

#[derive(Clone, Default)]
pub struct AccountManager {
    pub(super) inner: Arc<Mutex<ManagerState>>,
    pub(super) metadata: Arc<Mutex<()>>,
}
#[derive(Default)]
pub(super) struct ManagerState {
    pub(super) views: HashMap<Uuid, AccountView>,
    pub(super) generations: HashMap<Uuid, Uuid>,
    pub(super) pending: HashMap<Uuid, PendingLogin>,
    operations: HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>,
    pub(super) maintenance: HashSet<Uuid>,
}
pub(super) struct PendingLogin {
    pub(super) generation: Uuid,
    pub(super) cancel: oneshot::Sender<()>,
    pub(super) task: JoinHandle<()>,
}

impl AccountManager {
    pub fn new() -> Self {
        Self::default()
    }
    pub(super) fn operation(&self, id: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .lock()
            .expect("accounts lock")
            .operations
            .entry(id)
            .or_default()
            .clone()
    }
    pub(super) fn pending(&self, id: Uuid) -> bool {
        self.inner
            .lock()
            .expect("accounts lock")
            .pending
            .contains_key(&id)
    }
    pub async fn shutdown(&self) {
        let pending: Vec<_> = self
            .inner
            .lock()
            .expect("accounts lock")
            .pending
            .drain()
            .map(|(_, pending)| pending)
            .collect();
        futures_util::future::join_all(pending.into_iter().map(finish_pending)).await;
    }
}

/// Stop an account bridge and its native client. The group is always a fresh
/// child's own, and it is also stopped on abort, when the `OwnedGroup` drops.
pub(super) async fn close_group(group: &mut OwnedGroup, child: &mut tokio::process::Child) {
    let had_group = group.terminate();
    let _ = tokio::time::timeout(Duration::from_millis(700), child.wait()).await;
    if had_group {
        tokio::time::sleep(Duration::from_millis(100)).await;
        group.kill();
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    group.release();
}

pub(super) struct Maintenance {
    manager: AccountManager,
    id: Uuid,
}
impl Drop for Maintenance {
    fn drop(&mut self) {
        self.manager
            .inner
            .lock()
            .expect("accounts lock")
            .maintenance
            .remove(&self.id);
    }
}
pub(super) async fn begin_maintenance(state: &AppState, id: Uuid) -> Result<Maintenance> {
    let _guard = state.operations.lock().await;
    let source = format!("account:{id}");
    let native_source = record(state, id)
        .ok()
        .and_then(|record| record.native_config.map(|reference| reference.source_id));
    for session in state
        .store
        .list_sessions(None)
        .map_err(ApiError::internal)?
    {
        let bound = session.native_source_id.as_deref() == Some(&source)
            || native_source
                .as_deref()
                .is_some_and(|source_id| session.native_source_id.as_deref() == Some(source_id))
            || session
                .endpoint_snapshot
                .as_ref()
                .and_then(|profile| profile.native_config.as_ref())
                .is_some_and(|reference| {
                    reference.source_id == source
                        || native_source.as_deref() == Some(reference.source_id.as_str())
                });
        if bound
            && (matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Waiting
            ) || state
                .runtime
                .get(&session.id.to_string())
                .is_some_and(|runtime| runtime.running())
                || state
                    .chats
                    .get(session.id)
                    .is_some_and(|runtime| runtime.running()))
        {
            return Err(ApiError::conflict(
                "Stop sessions using this account before changing its authentication or consuming a reset credit",
            ));
        }
    }
    state
        .accounts
        .inner
        .lock()
        .expect("accounts lock")
        .maintenance
        .insert(id);
    Ok(Maintenance {
        manager: state.accounts.clone(),
        id,
    })
}

pub(super) async fn finish_pending(pending: PendingLogin) {
    let _ = pending.cancel.send(());
    let mut task = pending.task;
    if tokio::time::timeout(Duration::from_secs(45), &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
}

use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use jolt_proto::{Chat, Space};
use jolt_vcs::{Repos, WorkspaceReference};
use tokio::sync::watch;

use crate::{SessionsEngine, Terminals, WorkspaceHost};

const ORPHAN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

struct WorkspaceCleanupInner {
    repos: Repos,
    sessions: SessionsEngine,
    terminals: Terminals,
    device_id: String,
}

/// Device-local lifecycle service for Jolt-created Git worktrees and JJ workspaces.
/// Dropping the service releases the weakly-owned background task.
pub struct WorkspaceCleanup {
    _inner: Arc<WorkspaceCleanupInner>,
}

impl WorkspaceCleanup {
    pub fn start(
        repos: Repos,
        workspace: WorkspaceHost,
        sessions: SessionsEngine,
        terminals: Terminals,
        device_id: &str,
    ) -> Self {
        let inner = Arc::new(WorkspaceCleanupInner {
            repos,
            sessions,
            terminals,
            device_id: device_id.to_string(),
        });
        tokio::spawn(cleanup_task(
            Arc::downgrade(&inner),
            workspace.watch_chats(),
            workspace.watch_spaces(),
        ));
        Self { _inner: inner }
    }
}

async fn cleanup_task(
    inner: Weak<WorkspaceCleanupInner>,
    mut chats_rx: watch::Receiver<Vec<Chat>>,
    mut spaces_rx: watch::Receiver<Vec<Space>>,
) {
    let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    sweep.tick().await;
    if let Some(inner) = inner.upgrade() {
        let chats = chats_rx.borrow().clone();
        let spaces = spaces_rx.borrow().clone();
        reconcile(&inner, &chats, &spaces, true).await;
    }
    loop {
        tokio::select! {
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow_and_update().clone();
                let spaces = spaces_rx.borrow().clone();
                reconcile(&inner, &chats, &spaces, false).await;
            }
            changed = spaces_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                let spaces = spaces_rx.borrow_and_update().clone();
                reconcile(&inner, &chats, &spaces, false).await;
            }
            _ = sweep.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                let spaces = spaces_rx.borrow().clone();
                reconcile(&inner, &chats, &spaces, true).await;
            }
        }
    }
}

async fn reconcile(inner: &WorkspaceCleanupInner, chats: &[Chat], spaces: &[Space], reap: bool) {
    let references = workspace_references(&inner.device_id, chats, spaces);
    if !reap || inner.sessions.any_active() || inner.terminals.any_open() {
        if let Err(error) = inner
            .repos
            .publish_managed_workspace_references(&references)
            .await
        {
            tracing::warn!(error = %error, "managed workspace references update failed");
        }
        return;
    }
    match inner
        .repos
        .reconcile_managed_workspaces(&references, ORPHAN_GRACE)
        .await
    {
        Ok(report) if report.removed > 0 || report.dirty > 0 || report.failed > 0 => {
            tracing::info!(
                removed = report.removed,
                dirty = report.dirty,
                failed = report.failed,
                "orphaned workspace cleanup completed"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(error = %error, "orphaned workspace cleanup failed"),
    }
}

fn workspace_references(
    device_id: &str,
    chats: &[Chat],
    spaces: &[Space],
) -> Vec<WorkspaceReference> {
    let mut seen = HashSet::new();
    chats
        .iter()
        .filter(|chat| chat.device_id == device_id)
        .filter_map(|chat| {
            let path = chat.cwd.as_deref().unwrap_or_default().trim();
            (chat.checkout_id.is_some() || !path.is_empty())
                .then(|| (chat.checkout_id.clone(), path.to_string()))
        })
        .chain(
            spaces
                .iter()
                .filter(|space| space.device_id == device_id)
                .filter_map(|space| {
                    let path = space.path.trim();
                    (!path.is_empty()).then(|| (space.checkout_id.clone(), path.to_string()))
                }),
        )
        .filter(|reference| seen.insert(reference.clone()))
        .map(|(checkout_id, path)| WorkspaceReference { checkout_id, path })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use jolt_proto::Space;

    use super::*;

    fn chat(id: &str, device_id: &str, path: &str) -> Chat {
        Chat {
            id: id.into(),
            device_id: device_id.into(),
            title: None,
            archived: false,
            pinned: false,
            cwd: Some(path.into()),
            branch: None,
            checkout_id: Some(format!("checkout-{id}")),
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_conversations: Vec::new(),
            space_id: None,
            last_seen_at: None,
            goal: None,
        }
    }

    #[test]
    fn references_include_closed_threads_and_spaces_on_this_device() {
        let mut closed = chat("closed", "device", "/worktree");
        closed.archived = true;
        let remote = chat("remote", "other", "/remote");
        let space = Space {
            id: "space".into(),
            device_id: "device".into(),
            path: "/space-worktree".into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: Some("space-checkout".into()),
            created_at: Utc::now(),
        };

        let references = workspace_references("device", &[closed, remote], &[space]);

        assert_eq!(references.len(), 2);
        assert!(
            references
                .iter()
                .any(|reference| reference.path == "/worktree")
        );
        assert!(
            references
                .iter()
                .any(|reference| reference.path == "/space-worktree")
        );
    }
}

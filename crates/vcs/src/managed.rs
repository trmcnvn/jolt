use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jolt_proto::{VcsKind, Worktree};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{VcsError, hex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceReference {
    pub checkout_id: Option<String>,
    pub path: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceCleanupReport {
    pub removed: usize,
    pub dirty: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedWorkspace {
    pub checkout_id: String,
    pub vcs_kind: VcsKind,
    pub repo_path: String,
    pub workspace_path: String,
    pub name: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub orphaned_at_ms: Option<i64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceSnapshot {
    updated_at_ms: i64,
    references: Vec<PersistedReference>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedReference {
    checkout_id: Option<String>,
    path: String,
}

pub(crate) struct ManagedWorkspaceStore {
    records_path: PathBuf,
    references_path: PathBuf,
    references_dir: PathBuf,
    lock: Mutex<()>,
}

impl ManagedWorkspaceStore {
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        let key = device_key(device_id);
        let root = data_dir.join("managed-workspaces");
        let references_dir = root.join("references");
        Self {
            records_path: root.join("owners").join(format!("{key}.json")),
            references_path: references_dir.join(format!("{key}.json")),
            references_dir,
            lock: Mutex::new(()),
        }
    }

    pub fn register(
        &self,
        vcs_kind: VcsKind,
        worktree: &Worktree,
        now_ms: i64,
    ) -> Result<(), VcsError> {
        let Some(checkout_id) = worktree.checkout_id.as_deref() else {
            return Err(VcsError::new("managed workspace has no checkout identity"));
        };
        let _guard = self.lock();
        let mut records = self.load_records()?;
        records.retain(|record| record.workspace_path != worktree.path);
        records.push(ManagedWorkspace {
            checkout_id: checkout_id.to_string(),
            vcs_kind,
            repo_path: worktree.repo_path.clone(),
            workspace_path: worktree.path.clone(),
            name: worktree.name.clone(),
            created_at_ms: now_ms,
            orphaned_at_ms: None,
        });
        self.save_records(&records)
    }

    pub fn forget_path(&self, path: &Path) -> Result<(), VcsError> {
        let _guard = self.lock();
        let mut records = self.load_records()?;
        let original_len = records.len();
        records.retain(|record| !same_path(Path::new(&record.workspace_path), path));
        if records.len() != original_len {
            self.save_records(&records)?;
        }
        Ok(())
    }

    pub fn reconcile(
        &self,
        references: &[WorkspaceReference],
        now_ms: i64,
        grace: Duration,
    ) -> Result<Vec<ManagedWorkspace>, VcsError> {
        let _guard = self.lock();
        self.save_references(references, now_ms)?;
        let all_references = self.load_all_references()?;
        let checkout_ids: HashSet<&str> = all_references
            .iter()
            .filter_map(|reference| reference.checkout_id.as_deref())
            .collect();
        let paths: Vec<&Path> = all_references
            .iter()
            .filter(|reference| !reference.path.is_empty())
            .map(|reference| Path::new(&reference.path))
            .collect();
        let grace_ms = i64::try_from(grace.as_millis()).unwrap_or(i64::MAX);
        let mut records = self.load_records()?;
        let mut due = Vec::new();
        let mut changed = false;
        for record in &mut records {
            let referenced = checkout_ids.contains(record.checkout_id.as_str())
                || paths
                    .iter()
                    .any(|path| same_path(Path::new(&record.workspace_path), path));
            if referenced {
                changed |= record.orphaned_at_ms.take().is_some();
                continue;
            }
            let orphaned_at = match record.orphaned_at_ms {
                Some(orphaned_at) => orphaned_at,
                None => {
                    record.orphaned_at_ms = Some(now_ms);
                    changed = true;
                    now_ms
                }
            };
            if now_ms.saturating_sub(orphaned_at) >= grace_ms {
                due.push(record.clone());
            }
        }
        if changed {
            self.save_records(&records)?;
        }
        Ok(due)
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn load_records(&self) -> Result<Vec<ManagedWorkspace>, VcsError> {
        load_json(&self.records_path)
    }

    fn save_records(&self, records: &[ManagedWorkspace]) -> Result<(), VcsError> {
        save_json(&self.records_path, records)
    }

    fn save_references(
        &self,
        references: &[WorkspaceReference],
        now_ms: i64,
    ) -> Result<(), VcsError> {
        let snapshot = ReferenceSnapshot {
            updated_at_ms: now_ms,
            references: references
                .iter()
                .map(|reference| PersistedReference {
                    checkout_id: reference.checkout_id.clone(),
                    path: reference.path.clone(),
                })
                .collect(),
        };
        save_json(&self.references_path, &snapshot)
    }

    fn load_all_references(&self) -> Result<Vec<PersistedReference>, VcsError> {
        let entries = match std::fs::read_dir(&self.references_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut references = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            references.extend(load_json::<ReferenceSnapshot>(&path)?.references);
        }
        Ok(references)
    }
}

fn device_key(device_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_id.as_bytes());
    hex(&hasher.finalize())
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn load_json<T>(path: &Path) -> Result<T, VcsError>
where
    T: serde::de::DeserializeOwned + Default,
{
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| VcsError::new(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_json<T>(path: &Path, value: &T) -> Result<(), VcsError>
where
    T: Serialize + ?Sized,
{
    let parent = path
        .parent()
        .ok_or_else(|| VcsError::new("managed workspace registry has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| VcsError::new(format!("{}: {error}", path.display())))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worktree(path: &Path) -> Worktree {
        Worktree {
            repo_path: "/repo".into(),
            path: path.to_string_lossy().into_owned(),
            branch: "jolt/test".into(),
            name: "test".into(),
            checkout_id: Some("checkout".into()),
        }
    }

    #[test]
    fn references_reset_orphan_grace() {
        let data = tempfile::tempdir().unwrap();
        let workspace = data.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let store = ManagedWorkspaceStore::new(data.path(), "device");
        store
            .register(VcsKind::Git, &worktree(&workspace), 10)
            .unwrap();

        assert!(
            store
                .reconcile(&[], 20, Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .reconcile(
                    &[WorkspaceReference {
                        checkout_id: Some("checkout".into()),
                        path: workspace.to_string_lossy().into_owned(),
                    }],
                    200,
                    Duration::from_millis(100),
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .reconcile(&[], 250, Duration::from_millis(100))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .reconcile(&[], 350, Duration::from_millis(100))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn another_scope_path_reference_prevents_cleanup() {
        let data = tempfile::tempdir().unwrap();
        let workspace = data.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let owner = ManagedWorkspaceStore::new(data.path(), "owner");
        let other = ManagedWorkspaceStore::new(data.path(), "other");
        owner
            .register(VcsKind::Git, &worktree(&workspace), 10)
            .unwrap();
        other
            .reconcile(
                &[WorkspaceReference {
                    checkout_id: None,
                    path: workspace.to_string_lossy().into_owned(),
                }],
                20,
                Duration::ZERO,
            )
            .unwrap();

        assert!(
            owner
                .reconcile(&[], 100, Duration::ZERO)
                .unwrap()
                .is_empty()
        );
    }
}

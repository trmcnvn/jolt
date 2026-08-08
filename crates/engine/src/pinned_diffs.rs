//! Durable leases for immutable working-copy diff revisions under review.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use jolt_proto::{CheckoutDiffManifest, CheckoutDiffPage};
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::diff_projection::DiffProjection;

#[derive(Clone)]
pub(crate) struct PinnedDiffStore {
    root: PathBuf,
    writes: Arc<tokio::sync::Mutex<()>>,
}

impl PinnedDiffStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            writes: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn pin(
        &self,
        projection: &DiffProjection,
        review_id: &str,
    ) -> Result<(), EngineError> {
        let _write = self.writes.lock().await;
        let revision = &projection.manifest.catalog_revision;
        if !is_digest(revision) {
            return Err(EngineError::Other("invalid diff revision".into()));
        }
        let entry = self.root.join(revision);
        let pages = entry.join("pages");
        let leases = entry.join("leases");
        tokio::fs::create_dir_all(&pages).await?;
        tokio::fs::create_dir_all(&leases).await?;
        for page in projection.pages() {
            let path = pages.join(format!("{}.json", page.id));
            if tokio::fs::try_exists(&path).await? {
                continue;
            }
            let bytes = serde_json::to_vec(page)
                .map_err(|error| EngineError::Other(format!("pinned diff page encode: {error}")))?;
            atomic_write(&path, &bytes).await?;
        }
        let manifest = serde_json::to_vec(&projection.manifest)
            .map_err(|error| EngineError::Other(format!("pinned diff manifest encode: {error}")))?;
        atomic_write(&entry.join("manifest.json"), &manifest).await?;
        atomic_write(&leases.join(lease_name(review_id)), b"").await
    }

    pub(crate) async fn release(&self, revision: &str, review_id: &str) -> Result<(), EngineError> {
        if !is_digest(revision) {
            return Ok(());
        }
        let _write = self.writes.lock().await;
        let entry = self.root.join(revision);
        let leases = entry.join("leases");
        match tokio::fs::remove_file(leases.join(lease_name(review_id))).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let mut remaining = match tokio::fs::read_dir(&leases).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if remaining.next_entry().await?.is_none() {
            tokio::fs::remove_dir_all(entry).await?;
        }
        Ok(())
    }

    pub(crate) fn page(
        &self,
        revision: &str,
        page_id: &str,
    ) -> Result<Option<CheckoutDiffPage>, EngineError> {
        if !is_digest(revision) || !is_digest(page_id) {
            return Ok(None);
        }
        let entry = self.root.join(revision);
        let manifest: CheckoutDiffManifest = match std::fs::read(entry.join("manifest.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                EngineError::Other(format!("pinned diff manifest decode: {error}"))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if manifest.catalog_revision != revision
            || !manifest.pages.iter().any(|page| page.id == page_id)
        {
            return Ok(None);
        }
        let bytes = match std::fs::read(entry.join("pages").join(format!("{page_id}.json"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let page: CheckoutDiffPage = serde_json::from_slice(&bytes)
            .map_err(|error| EngineError::Other(format!("pinned diff page decode: {error}")))?;
        if page.id != page_id || page.catalog_revision != revision {
            return Err(EngineError::Other(
                "stored pinned diff page does not match its manifest".into(),
            ));
        }
        Ok(Some(page))
    }
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    let parent = path
        .parent()
        .ok_or_else(|| EngineError::Other("pinned diff path has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, bytes).await?;
    if let Err(error) = tokio::fs::rename(&temporary, path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error.into());
    }
    Ok(())
}

fn lease_name(review_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(review_id.as_bytes());
    format!("{}-lease", jolt_vcs::hex(&hash.finalize()))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use jolt_proto::{DiffFileSummary, VcsKind};

    use super::*;
    use crate::diff_sync::DiffSnapshot;

    #[tokio::test]
    async fn pinned_pages_survive_projection_replacement_until_release() {
        let temp = tempfile::tempdir().unwrap();
        let store = PinnedDiffStore::new(temp.path().join("pins"));
        let patch = "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let projection = DiffProjection::build(
            "checkout",
            "device",
            "/repo",
            &DiffSnapshot {
                vcs: VcsKind::Git,
                label: None,
                branch: "main".into(),
                head_sha: Some("head".into()),
                patch: patch.into(),
                files: vec![DiffFileSummary {
                    path: "a.rs".into(),
                    old_path: None,
                    status: "modified".into(),
                    additions: 1,
                    deletions: 1,
                    binary: false,
                }],
                additions: 1,
                deletions: 1,
                truncated: false,
                checksum: "snapshot".into(),
            },
            Utc::now(),
        );
        let revision = projection.manifest.catalog_revision.clone();
        let page_id = projection.manifest.pages[0].id.clone();

        store.pin(&projection, "review").await.unwrap();
        let page = store.page(&revision, &page_id).unwrap().unwrap();
        assert!(page.patch.contains("+new"));

        store.release(&revision, "review").await.unwrap();
        assert!(store.page(&revision, &page_id).unwrap().is_none());
    }
}

//! Immutable assistant-turn diff capture and local page storage.
//!
//! Turn manifests are designed to travel with transcript metadata. Patch bodies
//! stay device-local, content-addressed, and independently fetchable by the
//! desktop Changes viewer.

use std::path::{Path, PathBuf};

use chrono::Utc;
use jolt_proto::{CheckoutDiffPage, TurnDiffAttribution, TurnDiffManifest};
use sha2::{Digest, Sha256};

use crate::EngineError;
use crate::diff_projection::DiffProjection;
use crate::diff_sync::{TurnDiffBaseline, capture_scoped_turn_diff, capture_turn_diff_baseline};
use jolt_vcs::Repos;

const TURN_DIFF_STORE_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredManifest {
    version: u32,
    manifest: TurnDiffManifest,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum StoredOrLegacyManifest {
    Stored(StoredManifest),
    Legacy(TurnDiffManifest),
}

#[derive(Clone)]
pub struct TurnDiffStore {
    root: PathBuf,
    repos: Repos,
    device_id: String,
}

impl TurnDiffStore {
    pub fn new(root: PathBuf, repos: Repos, device_id: String) -> Self {
        Self {
            root,
            repos,
            device_id,
        }
    }

    pub async fn capture_baseline(&self, cwd: &Path) -> Result<TurnDiffBaseline, EngineError> {
        capture_turn_diff_baseline(&self.repos, cwd).await
    }

    /// Capture and persist the net turn delta. A clean turn returns `None` and
    /// creates no transcript diff payload.
    pub async fn finalize(
        &self,
        chat_id: &str,
        assistant_message_id: &str,
        cwd: &Path,
        baseline: &TurnDiffBaseline,
        paths: &[String],
        attribution: TurnDiffAttribution,
    ) -> Result<Option<TurnDiffManifest>, EngineError> {
        let snapshot = capture_scoped_turn_diff(&self.repos, cwd, baseline, paths).await?;
        if snapshot.files.is_empty() {
            return Ok(None);
        }
        let completed_at = Utc::now();
        let projection = DiffProjection::build(
            assistant_message_id,
            &self.device_id,
            &cwd.to_string_lossy(),
            &snapshot,
            completed_at,
        );
        let checkout = &projection.manifest;
        let manifest = TurnDiffManifest {
            catalog_revision: checkout.catalog_revision.clone(),
            chat_id: chat_id.to_string(),
            assistant_message_id: assistant_message_id.to_string(),
            device_id: self.device_id.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            vcs: checkout.vcs,
            attribution,
            files: checkout.files.clone(),
            pages: checkout.pages.clone(),
            additions: checkout.additions,
            deletions: checkout.deletions,
            truncated: checkout.truncated,
            completed_at,
        };
        self.persist(&manifest, projection.pages()).await?;
        Ok(Some(manifest))
    }

    pub async fn page(
        &self,
        chat_id: &str,
        assistant_message_id: &str,
        catalog_revision: &str,
        page_id: &str,
    ) -> Result<Option<CheckoutDiffPage>, EngineError> {
        if !is_digest(page_id) {
            return Ok(None);
        }
        let manifest_path = self
            .entry_dir(chat_id, assistant_message_id)
            .join("manifest.json");
        let manifest: TurnDiffManifest = match tokio::fs::read(&manifest_path).await {
            Ok(bytes) => match serde_json::from_slice(&bytes).map_err(|error| {
                EngineError::Other(format!("turn diff manifest decode: {error}"))
            })? {
                StoredOrLegacyManifest::Stored(stored) => {
                    if stored.version != TURN_DIFF_STORE_VERSION {
                        return Err(EngineError::Other(format!(
                            "unsupported turn diff store version {}",
                            stored.version
                        )));
                    }
                    stored.manifest
                }
                StoredOrLegacyManifest::Legacy(manifest) => manifest,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if manifest.chat_id != chat_id
            || manifest.assistant_message_id != assistant_message_id
            || manifest.catalog_revision != catalog_revision
            || !manifest.pages.iter().any(|page| page.id == page_id)
        {
            return Ok(None);
        }
        let page_path = self
            .entry_dir(chat_id, assistant_message_id)
            .join("pages")
            .join(format!("{page_id}.json"));
        match tokio::fs::read(page_path).await {
            Ok(bytes) => {
                let page: CheckoutDiffPage = serde_json::from_slice(&bytes).map_err(|error| {
                    EngineError::Other(format!("turn diff page decode: {error}"))
                })?;
                if page.id != page_id
                    || page.catalog_revision != catalog_revision
                    || content_digest(page.patch.as_bytes()) != page_id
                {
                    return Err(EngineError::Other(
                        "stored turn diff page does not match its manifest".into(),
                    ));
                }
                Ok(Some(page))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn persist<'a>(
        &self,
        manifest: &TurnDiffManifest,
        pages: impl Iterator<Item = &'a CheckoutDiffPage>,
    ) -> Result<(), EngineError> {
        let entry = self.entry_dir(&manifest.chat_id, &manifest.assistant_message_id);
        let pages_dir = entry.join("pages");
        tokio::fs::create_dir_all(&pages_dir).await?;
        for page in pages {
            let bytes = serde_json::to_vec(page)
                .map_err(|error| EngineError::Other(format!("turn diff page encode: {error}")))?;
            crate::atomic_file::write_immutable(
                &pages_dir.join(format!("{}.json", page.id)),
                &bytes,
            )
            .await?;
        }
        let bytes = serde_json::to_vec(&StoredManifest {
            version: TURN_DIFF_STORE_VERSION,
            manifest: manifest.clone(),
        })
        .map_err(|error| EngineError::Other(format!("turn diff manifest encode: {error}")))?;
        // The manifest is the commit marker: readers cannot observe it until
        // every referenced immutable page has landed.
        crate::atomic_file::write_immutable(&entry.join("manifest.json"), &bytes).await
    }

    /// Remove every locally retained turn diff for a deleted chat.
    pub async fn remove_chat(&self, chat_id: &str) -> Result<(), EngineError> {
        let path = self.root.join(digest(&[chat_id.as_bytes()]));
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn entry_dir(&self, chat_id: &str, assistant_message_id: &str) -> PathBuf {
        self.root
            .join(digest(&[chat_id.as_bytes()]))
            .join(digest(&[assistant_message_id.as_bytes()]))
    }
}

fn digest(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part);
        hash.update([0]);
    }
    jolt_vcs::hex(&hash.finalize())
}

fn content_digest(bytes: &[u8]) -> String {
    jolt_vcs::hex(&Sha256::digest(bytes))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn finalized_pages_survive_a_store_restart() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "jolt@example.invalid"]);
        git(&repo, &["config", "user.name", "Jolt Test"]);
        std::fs::write(repo.join("file.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "initial"]);

        let repos =
            Repos::with_worktrees_root(temp.path(), "device", temp.path().join("worktrees"));
        let root = temp.path().join("turn-diffs");
        let store = TurnDiffStore::new(root.clone(), repos.clone(), "device".into());
        let baseline = store.capture_baseline(&repo).await.unwrap();
        std::fs::write(repo.join("file.txt"), "after\n").unwrap();
        let manifest = store
            .finalize(
                "chat",
                "assistant",
                &repo,
                &baseline,
                &[],
                TurnDiffAttribution::Exact,
            )
            .await
            .unwrap()
            .unwrap();
        let page_id = manifest.pages[0].id.clone();

        let reopened = TurnDiffStore::new(root, repos, "device".into());
        let page = reopened
            .page("chat", "assistant", &manifest.catalog_revision, &page_id)
            .await
            .unwrap()
            .unwrap();
        assert!(page.patch.contains("-before"));
        assert!(page.patch.contains("+after"));

        // Manifests written before the versioned storage envelope remain
        // readable after an upgrade.
        let legacy_path = reopened
            .entry_dir("chat", "assistant")
            .join("manifest.json");
        std::fs::write(&legacy_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(
            reopened
                .page("chat", "assistant", &manifest.catalog_revision, &page_id,)
                .await
                .unwrap()
                .is_some()
        );

        reopened.remove_chat("chat").await.unwrap();
        assert!(
            reopened
                .page("chat", "assistant", &manifest.catalog_revision, &page_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn corrupted_content_addressed_page_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "jolt@example.invalid"]);
        git(&repo, &["config", "user.name", "Jolt Test"]);
        std::fs::write(repo.join("file.txt"), "before\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "initial"]);

        let repos =
            Repos::with_worktrees_root(temp.path(), "device", temp.path().join("worktrees"));
        let root = temp.path().join("turn-diffs");
        let store = TurnDiffStore::new(root, repos, "device".into());
        let baseline = store.capture_baseline(&repo).await.unwrap();
        std::fs::write(repo.join("file.txt"), "after\n").unwrap();
        let manifest = store
            .finalize(
                "chat",
                "assistant",
                &repo,
                &baseline,
                &[],
                TurnDiffAttribution::Exact,
            )
            .await
            .unwrap()
            .unwrap();
        let page_id = manifest.pages[0].id.clone();
        let page_path = store
            .entry_dir("chat", "assistant")
            .join("pages")
            .join(format!("{page_id}.json"));
        let mut page: CheckoutDiffPage =
            serde_json::from_slice(&std::fs::read(&page_path).unwrap()).unwrap();
        page.patch.push_str("tampered");
        std::fs::write(page_path, serde_json::to_vec(&page).unwrap()).unwrap();

        assert!(
            store
                .page("chat", "assistant", &manifest.catalog_revision, &page_id,)
                .await
                .is_err()
        );
    }
}

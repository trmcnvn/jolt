//! Repos — this device's repositories, refs, working copies, and folder browser.
//!
//! Repos are device-local (paths differ per machine), so the known set is a plain
//! JSON list (`{data_dir}/repos.json`) — no sync. Existing repos can live anywhere
//! the user points us; cloned/created ones land in `{data_dir}/repos`. Git worktrees
//! are created under `~/.jolt/worktrees/<repoName>/<worktreeName>`, with an
//! auto-generated name and matching `jolt/<name>` branch. JJ workspaces use
//! `~/.jolt/workspaces/<repoName>/<workspaceName>`. `JOLT_WORKTREES_DIR` and
//! `JOLT_WORKSPACES_DIR` override those roots.
//!
//! VCS access is via subprocess (`tokio::process`). Exactly one device-local
//! backend is active at a time; see [`crate::vcs`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use jolt_proto::{
    FileSearchMatch, FolderEntry, FolderListing, Repo, RepoRef, RepoRefKind, VcsCommitResult,
    VcsKind, VcsPublicationState, VcsPublishTarget, VcsPushResult, VcsSettingsSnapshot, Worktree,
};

use crate::VcsError;
use crate::managed::{
    ManagedWorkspace, ManagedWorkspaceStore, WorkspaceCleanupReport, WorkspaceReference,
};
use crate::vcs::{Vcs, VcsCommand, compose_command_path};

/// Existence probe timeout for user-chosen / remembered paths, which can point at
/// dead network mounts where a bare `stat` hangs for minutes.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_secs(2);
/// Hard wall-clock ceiling for a folder listing (the walk runs in a disposable
/// blocking task; on expiry the caller unblocks and the task is abandoned).
const FOLDER_LIST_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap on returned folder entries (bounds response size).
const FOLDER_LIST_MAX_ENTRIES: usize = 500;
/// File mentions should remain responsive even in very large checkouts.
const FILE_SEARCH_MAX_RESULTS: usize = 8;
/// A dead network mount must not leave the composer search spinning forever.
const FILE_SEARCH_TIMEOUT: Duration = Duration::from_secs(6);
static TEMP_INDEX_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn parse_two_counts(value: &str) -> Result<(u32, u32), VcsError> {
    let mut fields = value.split_whitespace();
    let left = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| VcsError::new("VCS divergence output is malformed"))?;
    let right = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| VcsError::new("VCS divergence output is malformed"))?;
    Ok((left, right))
}

fn publication_from_divergence(
    target: VcsPublishTarget,
    ahead: u32,
    behind: u32,
    is_default_ref: bool,
) -> VcsPublicationState {
    match (ahead, behind) {
        (0, 0) => VcsPublicationState::NoCompletedChanges {
            target,
            is_default_ref,
        },
        (0, behind) => VcsPublicationState::Behind {
            target,
            behind,
            is_default_ref,
        },
        (ahead, 0) => VcsPublicationState::Ready {
            target,
            ahead,
            behind: 0,
            is_default_ref,
        },
        (ahead, behind) => VcsPublicationState::Diverged {
            target,
            ahead,
            behind,
            is_default_ref,
        },
    }
}

const ADJECTIVES: &[&str] = &[
    "swift", "calm", "bright", "bold", "keen", "brave", "clever", "lucky", "quiet", "warm", "cool",
    "sharp", "gentle", "vivid", "amber", "cobalt",
];
const NOUNS: &[&str] = &[
    "otter", "harbor", "falcon", "cedar", "meadow", "jolt", "delta", "ember", "lynx", "maple",
    "onyx", "quartz", "raven", "summit", "willow", "aspen",
];

/// Canonical identity shared by every chat operating in this exact worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutIdentity {
    /// `sha256(deviceId ‖ NUL ‖ backend ‖ NUL ‖ canonical metadata path)`.
    pub id: String,
    /// Canonical working-copy root, with symlinks resolved.
    pub root: PathBuf,
    /// Canonical backend metadata path (`.git` or JJ workspace metadata).
    pub metadata_dir: PathBuf,
}

/// Best-effort home directory (the `ListFolders` default and worktree root base).
#[doc(hidden)]
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where new worktrees live. Deliberately NOT under the backend data dir —
/// worktrees are user-facing working checkouts. `JOLT_WORKTREES_DIR` overrides
/// (test isolation); empty reads as unset.
fn default_worktrees_root() -> PathBuf {
    std::env::var_os("JOLT_WORKTREES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".jolt").join("worktrees"))
}

fn default_workspaces_root() -> PathBuf {
    std::env::var_os("JOLT_WORKSPACES_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".jolt").join("workspaces"))
}

struct ReposInner {
    data_dir: PathBuf,
    device_id: String,
    worktrees_root: PathBuf,
    workspaces_root: PathBuf,
    vcs: Vcs,
    managed_workspaces: ManagedWorkspaceStore,
    managed_workspace_lock: tokio::sync::Mutex<()>,
    file_searches: std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>,
    action_locks: std::sync::Mutex<HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>>,
}

#[derive(Clone)]
pub struct Repos {
    inner: std::sync::Arc<ReposInner>,
}

impl Repos {
    /// `data_dir` holds `repos.json` + cloned/created repos; the worktree root
    /// comes from `$JOLT_WORKTREES_DIR` or `~/.jolt/worktrees`.
    pub fn new(data_dir: &Path, device_id: &str) -> Self {
        Self::with_roots(
            data_dir,
            device_id,
            default_worktrees_root(),
            default_workspaces_root(),
            false,
        )
    }

    /// Explicit worktree root (tests).
    pub fn with_worktrees_root(data_dir: &Path, device_id: &str, worktrees_root: PathBuf) -> Self {
        Self::with_roots(
            data_dir,
            device_id,
            worktrees_root,
            data_dir.join("workspaces"),
            true,
        )
    }

    fn with_roots(
        data_dir: &Path,
        device_id: &str,
        worktrees_root: PathBuf,
        workspaces_root: PathBuf,
        prefer_git_for_tests: bool,
    ) -> Self {
        let vcs = Vcs::new(data_dir);
        if prefer_git_for_tests {
            let _ = vcs.set_selected(VcsKind::Git);
        }
        Self {
            inner: std::sync::Arc::new(ReposInner {
                data_dir: data_dir.to_path_buf(),
                device_id: device_id.to_string(),
                worktrees_root,
                workspaces_root,
                vcs,
                managed_workspaces: ManagedWorkspaceStore::new(data_dir, device_id),
                managed_workspace_lock: tokio::sync::Mutex::new(()),
                file_searches: std::sync::Mutex::new(HashMap::new()),
                action_locks: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn vcs_settings(&self) -> VcsSettingsSnapshot {
        self.inner.vcs.snapshot()
    }

    pub fn set_vcs(&self, kind: VcsKind) -> Result<VcsSettingsSnapshot, VcsError> {
        self.inner.vcs.set_selected(kind)
    }

    pub fn vcs_kind(&self) -> Option<VcsKind> {
        self.inner.vcs.active_kind()
    }

    #[doc(hidden)]
    pub fn vcs_command(&self) -> Result<VcsCommand, VcsError> {
        self.inner
            .vcs
            .active_command()
            .ok_or_else(|| VcsError::new("No supported VCS executable found"))
    }

    /// One mutation lane per canonical checkout. Commit and push are checkout
    /// operations even when several chats happen to point at that checkout.
    pub fn action_lock(&self, checkout_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .inner
            .action_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lock) = locks.get(checkout_id).and_then(std::sync::Weak::upgrade) {
            return lock;
        }
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(checkout_id.to_string(), std::sync::Arc::downgrade(&lock));
        lock
    }

    /// Publication state for the active backend. JJ deliberately publishes the
    /// completed change at `@-`; mutable edits in `@` are never included by Push.
    pub async fn publication_status(
        &self,
        path: &Path,
        title: &str,
    ) -> Result<(String, VcsPublicationState), VcsError> {
        self.publication_status_for_ref(path, title, None).await
    }

    pub async fn publication_status_for_ref(
        &self,
        path: &Path,
        title: &str,
        publish_ref: Option<&str>,
    ) -> Result<(String, VcsPublicationState), VcsError> {
        match self.vcs_kind() {
            Some(VcsKind::Git) => self.git_publication_status(path).await,
            Some(VcsKind::Jujutsu) => self.jj_publication_status(path, title, publish_ref).await,
            None => Err(VcsError::new("No supported VCS executable found")),
        }
    }

    async fn git_publication_status(
        &self,
        path: &Path,
    ) -> Result<(String, VcsPublicationState), VcsError> {
        let branch = self.current_branch(path).await?;
        if branch == "HEAD" {
            return Ok((
                branch,
                VcsPublicationState::Unavailable {
                    reason: "Checkout is detached; switch to a branch before pushing".into(),
                },
            ));
        }
        let revision = self
            .git(&["rev-parse", "HEAD"], Some(path))
            .await
            .unwrap_or_default();
        let remotes = self
            .git(&["remote"], Some(path))
            .await?
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if remotes.is_empty() {
            return Ok((branch, VcsPublicationState::NoRemote));
        }
        let configured_remote = self
            .git(
                &["config", "--get", &format!("branch.{branch}.remote")],
                Some(path),
            )
            .await
            .ok()
            .filter(|remote| remotes.contains(remote));
        let remote = configured_remote
            .or_else(|| {
                remotes
                    .contains(&"origin".to_string())
                    .then(|| "origin".to_string())
            })
            .or_else(|| (remotes.len() == 1).then(|| remotes[0].clone()))
            .ok_or_else(|| {
                VcsError::new("Several Git remotes are available; configure a push remote")
            })?;
        let upstream = self
            .git(
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    "--symbolic-full-name",
                    "@{upstream}",
                ],
                Some(path),
            )
            .await
            .ok();
        let remote_branch = upstream
            .as_deref()
            .and_then(|value| value.strip_prefix(&format!("{remote}/")))
            .unwrap_or(&branch)
            .to_string();
        let default_ref = self
            .git(
                &[
                    "symbolic-ref",
                    "--short",
                    &format!("refs/remotes/{remote}/HEAD"),
                ],
                Some(path),
            )
            .await
            .ok()
            .and_then(|value| {
                value
                    .strip_prefix(&format!("{remote}/"))
                    .map(str::to_string)
            });
        let remote_branch_exists = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/{remote}/{remote_branch}"),
                ],
                Some(path),
            )
            .await
            .is_ok();
        let compare_ref = if let Some(upstream) = upstream.as_deref() {
            Some(upstream.to_string())
        } else if remote_branch_exists {
            Some(format!("{remote}/{remote_branch}"))
        } else {
            default_ref.as_ref().map(|name| format!("{remote}/{name}"))
        };
        let (ahead, behind) = match compare_ref {
            Some(compare_ref) => self
                .git_divergence(path, &compare_ref)
                .await
                .unwrap_or((0, 0)),
            None => (1, 0),
        };
        let target = VcsPublishTarget {
            ref_name: remote_branch.clone(),
            remote: remote.clone(),
            remote_ref: format!("{remote}/{remote_branch}"),
            revision,
            creates_ref: upstream.is_none() && !remote_branch_exists,
            sets_upstream: upstream.is_none(),
        };
        let is_default_ref = default_ref.as_deref() == Some(remote_branch.as_str());
        Ok((
            branch,
            publication_from_divergence(target, ahead, behind, is_default_ref),
        ))
    }

    async fn git_divergence(&self, path: &Path, compare_ref: &str) -> Result<(u32, u32), VcsError> {
        let output = self
            .git(
                &[
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("HEAD...{compare_ref}"),
                ],
                Some(path),
            )
            .await?;
        parse_two_counts(&output)
    }

    async fn jj_publication_status(
        &self,
        path: &Path,
        title: &str,
        publish_ref: Option<&str>,
    ) -> Result<(String, VcsPublicationState), VcsError> {
        let reference = self.working_copy_label(path).await?;
        let parent = self
            .jj(
                &[
                    "log",
                    "-r",
                    "@-",
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\"",
                ],
                Some(path),
                true,
            )
            .await?;
        let parents = parent
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        if parents.len() != 1 {
            return Ok((
                reference,
                VcsPublicationState::Unavailable {
                    reason: "Commit the current JJ change before pushing this merge".into(),
                },
            ));
        }
        let revision = parents[0].trim().to_string();
        let remotes = self
            .jj(&["git", "remote", "list"], Some(path), true)
            .await?
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| *name != "git")
            .map(str::to_string)
            .collect::<Vec<_>>();
        if remotes.is_empty() {
            return Ok((reference, VcsPublicationState::NoRemote));
        }
        let nearest_tracked_remotes = self.nearest_tracked_remotes(path, &remotes).await?;
        if nearest_tracked_remotes.len() > 1 {
            return Ok((
                reference,
                VcsPublicationState::Unavailable {
                    reason: "Nearest tracked JJ bookmarks use several remotes".into(),
                },
            ));
        }
        let remote = if let Some(remote) = nearest_tracked_remotes.first() {
            remote.clone()
        } else if remotes.contains(&"origin".to_string()) {
            "origin".to_string()
        } else if remotes.len() == 1 {
            remotes[0].clone()
        } else {
            return Ok((
                reference,
                VcsPublicationState::Unavailable {
                    reason: "Several JJ Git remotes are available; configure git.push".into(),
                },
            ));
        };
        let nearest = self.nearest_jolt_bookmarks(path).await?;
        let desired = worktree_branch_from_title(title);
        let ref_name = if let Some(requested) = publish_ref {
            if !nearest.iter().any(|name| name == requested) {
                return Err(VcsError::new(
                    "Selected Jolt bookmark is no longer a publish candidate",
                ));
            }
            requested.to_string()
        } else if nearest.iter().any(|name| name == &desired) {
            desired
        } else if nearest.len() == 1 {
            nearest[0].clone()
        } else if nearest.len() > 1 {
            let candidates = nearest
                .iter()
                .map(|name| VcsPublishTarget {
                    ref_name: name.clone(),
                    remote: remote.clone(),
                    remote_ref: format!("{remote}/{name}"),
                    revision: revision.clone(),
                    creates_ref: false,
                    // Recomputed after the user selects this candidate.
                    sets_upstream: true,
                })
                .collect();
            return Ok((reference, VcsPublicationState::Ambiguous { candidates }));
        } else {
            self.available_jolt_bookmark(path, &desired).await?
        };
        let remote_ref = format!("{ref_name}@{remote}");
        let remote_exists = self
            .jj(
                &[
                    "log",
                    "-r",
                    &remote_ref,
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\"",
                ],
                Some(path),
                true,
            )
            .await
            .is_ok();
        let (ahead, behind) = if remote_exists {
            let ahead = self
                .jj_revset_count(path, &format!("{remote_ref}..@-"))
                .await?;
            let behind = self
                .jj_revset_count(path, &format!("@-..{remote_ref}"))
                .await?;
            (ahead, behind)
        } else {
            (
                self.jj_revset_count(path, &format!("@- ~ ::remote_bookmarks(remote={remote})"))
                    .await?,
                0,
            )
        };
        let target = VcsPublishTarget {
            ref_name: ref_name.clone(),
            remote: remote.clone(),
            remote_ref: format!("{remote}/{ref_name}"),
            revision,
            creates_ref: nearest.is_empty(),
            sets_upstream: !remote_exists,
        };
        Ok((
            reference,
            publication_from_divergence(target, ahead, behind, false),
        ))
    }

    async fn nearest_tracked_remotes(
        &self,
        path: &Path,
        configured_remotes: &[String],
    ) -> Result<Vec<String>, VcsError> {
        let nearest = self
            .jj(
                &[
                    "log",
                    "-r",
                    "latest(::@- & bookmarks())",
                    "--no-graph",
                    "-T",
                    "bookmarks.map(|b| b.name()).join(\"\\n\") ++ \"\\n\"",
                ],
                Some(path),
                true,
            )
            .await?
            .lines()
            .map(str::to_string)
            .collect::<HashSet<_>>();
        if nearest.is_empty() {
            return Ok(Vec::new());
        }
        let listing = self
            .jj(
                &[
                    "bookmark",
                    "list",
                    "--all-remotes",
                    "-T",
                    "name ++ \"\\t\" ++ remote ++ \"\\n\"",
                ],
                Some(path),
                true,
            )
            .await?;
        let mut candidates = listing
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(name, remote)| {
                nearest.contains(*name)
                    && !remote.is_empty()
                    && *remote != "git"
                    && configured_remotes
                        .iter()
                        .any(|candidate| candidate == remote)
            })
            .map(|(_, remote)| remote.to_string())
            .collect::<Vec<_>>();
        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }

    async fn nearest_jolt_bookmarks(&self, path: &Path) -> Result<Vec<String>, VcsError> {
        let output = self
            .jj(
                &[
                    "log",
                    "-r",
                    "latest(::@- & bookmarks(glob:\"jolt/*\"))",
                    "--no-graph",
                    "-T",
                    "bookmarks.map(|b| b.name()).join(\"\\n\") ++ \"\\n\"",
                ],
                Some(path),
                true,
            )
            .await?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect())
    }

    async fn available_jolt_bookmark(
        &self,
        path: &Path,
        desired: &str,
    ) -> Result<String, VcsError> {
        let output = self
            .jj(
                &["bookmark", "list", "-T", "name ++ \"\\n\""],
                Some(path),
                true,
            )
            .await?;
        let existing = output.lines().collect::<HashSet<_>>();
        if !existing.contains(desired) {
            return Ok(desired.to_string());
        }
        for suffix in 2..1000 {
            let candidate = format!("{desired}-{suffix}");
            if !existing.contains(candidate.as_str()) {
                return Ok(candidate);
            }
        }
        Err(VcsError::new("Could not allocate a Jolt bookmark"))
    }

    async fn jj_revset_count(&self, path: &Path, revset: &str) -> Result<u32, VcsError> {
        let output = self
            .jj(
                &["log", "-r", revset, "--no-graph", "-T", "\"x\\n\""],
                Some(path),
                true,
            )
            .await?;
        Ok(output.lines().count().try_into().unwrap_or(u32::MAX))
    }

    /// Commit complete selected files and leave every other working-copy change
    /// untouched. The caller validates paths against one diff catalog first.
    pub async fn commit_changes(
        &self,
        path: &Path,
        paths: Option<&[String]>,
        message: &str,
    ) -> Result<VcsCommitResult, VcsError> {
        let subject = message.lines().next().unwrap_or_default().trim();
        if subject.is_empty() {
            return Err(VcsError::new("Commit message cannot be empty"));
        }
        match self.vcs_kind() {
            Some(VcsKind::Git) => {
                let body = message
                    .lines()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                if let Some(paths) = paths {
                    self.git_commit_selected(path, paths, subject, &body)
                        .await?;
                } else {
                    self.git(&["add", "-A"], Some(path)).await?;
                    let mut args = vec!["commit", "-m", subject];
                    if !body.is_empty() {
                        args.extend(["-m", body.as_str()]);
                    }
                    self.git(&args, Some(path)).await?;
                }
                let revision = self.git(&["rev-parse", "HEAD"], Some(path)).await?;
                let remaining_changes = !self
                    .git(&["status", "--porcelain"], Some(path))
                    .await?
                    .is_empty();
                Ok(VcsCommitResult {
                    revision,
                    subject: subject.to_string(),
                    remaining_changes,
                    advanced_ref: None,
                })
            }
            Some(VcsKind::Jujutsu) => {
                let bookmarks = self.nearest_jolt_bookmarks(path).await?;
                let mut args = vec!["commit", "-m", message, "--"];
                if let Some(paths) = paths {
                    args.extend(paths.iter().map(String::as_str));
                } else {
                    args.pop();
                }
                self.jj(&args, Some(path), false).await?;
                let revision = self
                    .jj(
                        &[
                            "log",
                            "-r",
                            "@-",
                            "--no-graph",
                            "-T",
                            "commit_id ++ \"\\n\"",
                        ],
                        Some(path),
                        true,
                    )
                    .await?;
                let advanced_ref = if bookmarks.len() == 1 {
                    self.jj(
                        &["bookmark", "set", &bookmarks[0], "-r", "@-"],
                        Some(path),
                        false,
                    )
                    .await?;
                    Some(bookmarks[0].clone())
                } else {
                    None
                };
                let remaining_changes = !self
                    .jj(&["diff", "--summary"], Some(path), true)
                    .await?
                    .is_empty();
                Ok(VcsCommitResult {
                    revision: revision.trim().to_string(),
                    subject: subject.to_string(),
                    remaining_changes,
                    advanced_ref,
                })
            }
            None => Err(VcsError::new("No supported VCS executable found")),
        }
    }

    async fn git_commit_selected(
        &self,
        path: &Path,
        paths: &[String],
        subject: &str,
        body: &str,
    ) -> Result<(), VcsError> {
        let index = self
            .git(
                &["rev-parse", "--path-format=absolute", "--git-path", "index"],
                Some(path),
            )
            .await?;
        let parent = Path::new(&index)
            .parent()
            .ok_or_else(|| VcsError::new("Git index path has no parent"))?;
        let sequence = TEMP_INDEX_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary_index = parent.join(format!("jolt-index-{}-{sequence}", std::process::id()));
        let result = async {
            if self
                .git(&["rev-parse", "--verify", "HEAD"], Some(path))
                .await
                .is_ok()
            {
                self.git_with_index(&temporary_index, &["read-tree", "HEAD"], path)
                    .await?;
            } else {
                self.git_with_index(&temporary_index, &["read-tree", "--empty"], path)
                    .await?;
            }
            let mut add = vec!["--literal-pathspecs", "add", "-A", "--"];
            add.extend(paths.iter().map(String::as_str));
            self.git_with_index(&temporary_index, &add, path).await?;

            let mut commit = vec!["commit", "-m", subject];
            if !body.is_empty() {
                commit.extend(["-m", body]);
            }
            self.git_with_index(&temporary_index, &commit, path).await?;
            Ok::<(), VcsError>(())
        }
        .await;
        let _ = tokio::fs::remove_file(&temporary_index).await;
        let _ = tokio::fs::remove_file(temporary_index.with_extension("lock")).await;
        result?;

        // HEAD now contains the selected files. Align just those entries in the
        // user's real index with HEAD; every unrelated staged entry is retained.
        let mut reset = vec!["--literal-pathspecs", "reset", "HEAD", "--"];
        reset.extend(paths.iter().map(String::as_str));
        self.git(&reset, Some(path)).await?;
        Ok(())
    }

    pub async fn push_completed(
        &self,
        path: &Path,
        title: &str,
        publish_ref: Option<&str>,
        allow_default_ref: bool,
    ) -> Result<VcsPushResult, VcsError> {
        let (_, status) = self
            .publication_status_for_ref(path, title, publish_ref)
            .await?;
        let (target, is_default_ref) = match status {
            VcsPublicationState::Ready {
                target,
                is_default_ref,
                ..
            } => (target, is_default_ref),
            VcsPublicationState::NoCompletedChanges { .. } => {
                return Err(VcsError::new("No completed changes to push"));
            }
            VcsPublicationState::NoRemote => {
                return Err(VcsError::new("No remote is configured"));
            }
            VcsPublicationState::Behind { .. } => {
                return Err(VcsError::new(
                    "Remote ref is ahead; update the checkout first",
                ));
            }
            VcsPublicationState::Diverged { .. } => {
                return Err(VcsError::new("Local and remote refs have diverged"));
            }
            VcsPublicationState::Ambiguous { .. } => {
                return Err(VcsError::new("Choose a Jolt bookmark to push"));
            }
            VcsPublicationState::Unavailable { reason } => return Err(VcsError::new(reason)),
        };
        if is_default_ref && !allow_default_ref {
            return Err(VcsError::new(format!(
                "Pushing the default ref {} requires confirmation",
                target.ref_name
            )));
        }
        match self.vcs_kind() {
            Some(VcsKind::Git) => {
                if target.sets_upstream {
                    self.git(
                        &[
                            "push",
                            "-u",
                            &target.remote,
                            &format!("HEAD:refs/heads/{}", target.ref_name),
                        ],
                        Some(path),
                    )
                    .await?;
                } else {
                    self.git(
                        &[
                            "push",
                            &target.remote,
                            &format!("HEAD:refs/heads/{}", target.ref_name),
                        ],
                        Some(path),
                    )
                    .await?;
                }
            }
            Some(VcsKind::Jujutsu) => {
                self.jj(
                    &["bookmark", "set", &target.ref_name, "-r", "@-"],
                    Some(path),
                    false,
                )
                .await?;
                self.jj(
                    &[
                        "git",
                        "push",
                        "--remote",
                        &target.remote,
                        "--bookmark",
                        &target.ref_name,
                    ],
                    Some(path),
                    false,
                )
                .await?;
            }
            None => return Err(VcsError::new("No supported VCS executable found")),
        }
        Ok(VcsPushResult {
            revision: target.revision,
            ref_name: target.ref_name,
            remote: target.remote,
            remote_ref: target.remote_ref,
            created_ref: target.creates_ref,
            set_upstream: target.sets_upstream,
            up_to_date: false,
        })
    }

    // ── registry (repos.json) ───────────────────────────────────────────────

    fn registry_path(&self) -> PathBuf {
        self.inner.data_dir.join("repos.json")
    }

    fn load_paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.registry_path())
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
    }

    fn save_paths(&self, paths: &[String]) -> Result<(), VcsError> {
        let mut seen = HashSet::new();
        let deduped: Vec<&String> = paths.iter().filter(|p| seen.insert(p.as_str())).collect();
        let json = serde_json::to_string_pretty(&deduped)
            .map_err(|e| VcsError::new(format!("repos registry serialize: {e}")))?;
        std::fs::create_dir_all(&self.inner.data_dir)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    fn register(&self, path: &str) -> Result<(), VcsError> {
        let mut paths = self.load_paths();
        paths.push(path.to_string());
        self.save_paths(&paths)
    }

    // ── VCS plumbing ────────────────────────────────────────────────────────

    async fn command(
        &self,
        expected: VcsKind,
        args: &[&str],
        cwd: Option<&Path>,
    ) -> Result<String, VcsError> {
        let backend = self
            .inner
            .vcs
            .active_command()
            .ok_or_else(|| VcsError::new("No supported VCS executable found"))?;
        if backend.kind != expected {
            return Err(VcsError::new(format!(
                "{} is not the active VCS backend",
                expected.label()
            )));
        }
        let mut cmd = tokio::process::Command::new(&backend.executable);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        compose_command_path(&mut cmd, &backend.executable);
        cmd.stdin(std::process::Stdio::null());
        let output = cmd
            .output()
            .await
            .map_err(|e| VcsError::new(format!("{} spawn failed: {e}", expected.label())))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(VcsError::new(if message.is_empty() {
                format!(
                    "{} {} failed ({})",
                    expected.label(),
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("{}: {message}", expected.label())
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, VcsError> {
        self.command(VcsKind::Git, args, cwd).await
    }

    async fn git_with_index(
        &self,
        index: &Path,
        args: &[&str],
        cwd: &Path,
    ) -> Result<String, VcsError> {
        let backend = self
            .inner
            .vcs
            .active_command()
            .ok_or_else(|| VcsError::new("No supported VCS executable found"))?;
        if backend.kind != VcsKind::Git {
            return Err(VcsError::new("Git is not the active VCS backend"));
        }
        let mut command = tokio::process::Command::new(&backend.executable);
        command
            .args(args)
            .current_dir(cwd)
            .env("GIT_INDEX_FILE", index)
            .stdin(std::process::Stdio::null());
        compose_command_path(&mut command, &backend.executable);
        let output = command
            .output()
            .await
            .map_err(|error| VcsError::new(format!("Git spawn failed: {error}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let message = stderr.trim();
            return Err(VcsError::new(if message.is_empty() {
                format!(
                    "Git {} failed ({})",
                    args.first().unwrap_or(&"?"),
                    output.status
                )
            } else {
                format!("Git: {message}")
            }));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn jj(
        &self,
        args: &[&str],
        cwd: Option<&Path>,
        ignore_working_copy: bool,
    ) -> Result<String, VcsError> {
        let mut full = vec!["--no-pager", "--color=never"];
        if ignore_working_copy {
            full.push("--ignore-working-copy");
        }
        full.extend_from_slice(args);
        self.command(VcsKind::Jujutsu, &full, cwd).await
    }

    /// Async existence probe with a timeout: a wedged network mount just reads
    /// as "gone" instead of hanging every caller.
    async fn path_exists(path: &Path) -> bool {
        let path = path.to_path_buf();
        matches!(
            tokio::time::timeout(PATH_EXISTS_TIMEOUT, tokio::fs::metadata(path)).await,
            Ok(Ok(_))
        )
    }

    /// Is `path` inside the active backend's working copy?
    pub async fn is_repo(&self, path: &Path) -> bool {
        match self.vcs_kind() {
            Some(VcsKind::Git) => matches!(
                self.git(&["rev-parse", "--is-inside-work-tree"], Some(path)).await,
                Ok(out) if out == "true"
            ),
            Some(VcsKind::Jujutsu) => self.jj(&["root"], Some(path), true).await.is_ok(),
            None => false,
        }
    }

    /// The branch currently checked out at a repo/worktree path (`"HEAD"` when detached).
    pub async fn current_branch(&self, path: &Path) -> Result<String, VcsError> {
        match self.vcs_kind() {
            Some(VcsKind::Git) => {
                let branch = self.git(&["branch", "--show-current"], Some(path)).await?;
                Ok(if branch.is_empty() {
                    "HEAD".to_string()
                } else {
                    branch
                })
            }
            Some(VcsKind::Jujutsu) => self.working_copy_label(path).await,
            None => Err(VcsError::new("No supported VCS executable found")),
        }
    }

    async fn working_copy_label(&self, path: &Path) -> Result<String, VcsError> {
        let id = self
            .jj(
                &[
                    "log",
                    "-r",
                    "@",
                    "--no-graph",
                    "-T",
                    "change_id.shortest(8)",
                ],
                Some(path),
                true,
            )
            .await?;
        Ok(id)
    }

    /// Branches/bookmarks that can identify the current checkout to a remote
    /// code-review provider. JJ working copies inherit the nearest bookmarked
    /// ancestor because the mutable `@` commit itself is normally unbookmarked.
    pub(crate) async fn review_references(&self, path: &Path) -> Result<Vec<String>, VcsError> {
        let output = match self.vcs_kind() {
            Some(VcsKind::Git) => {
                let branch = self.current_branch(path).await?;
                return Ok((branch != "HEAD").then_some(branch).into_iter().collect());
            }
            Some(VcsKind::Jujutsu) => {
                self.jj(
                    &[
                        "log",
                        "-r",
                        "latest(ancestors(@) & bookmarks())",
                        "--no-graph",
                        "-T",
                        "bookmarks.map(|b| b.name()).join(\"\\n\") ++ \"\\n\"",
                    ],
                    Some(path),
                    true,
                )
                .await?
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        };
        let mut seen = HashSet::new();
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty() && seen.insert((*name).to_string()))
            .map(str::to_string)
            .collect())
    }

    /// Fetch remote URLs from the active VCS without assigning meaning to the
    /// hosting service. Forge adapters decide which URLs they understand.
    pub(crate) async fn remote_urls(&self, path: &Path) -> Result<Vec<String>, VcsError> {
        let output = match self.vcs_kind() {
            Some(VcsKind::Git) => self.git(&["remote", "-v"], Some(path)).await?,
            Some(VcsKind::Jujutsu) => {
                self.jj(&["git", "remote", "list"], Some(path), true)
                    .await?
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        };
        let mut seen = HashSet::new();
        Ok(output
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .filter(|url| seen.insert((*url).to_string()))
            .map(str::to_string)
            .collect())
    }

    /// Canonical identity shared by every chat using this exact working copy.
    pub async fn checkout_identity(&self, path: &Path) -> Result<CheckoutIdentity, VcsError> {
        let (root, metadata_dir) = match self.vcs_kind() {
            Some(VcsKind::Git) => {
                let root = self
                    .git(&["rev-parse", "--show-toplevel"], Some(path))
                    .await?;
                let metadata = self
                    .git(
                        &["rev-parse", "--path-format=absolute", "--git-dir"],
                        Some(path),
                    )
                    .await?;
                (root, PathBuf::from(metadata))
            }
            Some(VcsKind::Jujutsu) => {
                let root = self.jj(&["root"], Some(path), true).await?;
                let metadata = PathBuf::from(&root).join(".jj").join("working_copy");
                (root, metadata)
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        };
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));
        let canonical_metadata_dir = std::fs::canonicalize(&metadata_dir).unwrap_or(metadata_dir);
        let mut hasher = Sha256::new();
        hasher.update(self.inner.device_id.as_bytes());
        hasher.update([0u8]);
        hasher.update(canonical_metadata_dir.to_string_lossy().as_bytes());
        let id = hex(&hasher.finalize());
        Ok(CheckoutIdentity {
            id,
            root: canonical_root,
            metadata_dir: canonical_metadata_dir,
        })
    }

    async fn to_repo(&self, path: &Path) -> Result<Repo, VcsError> {
        let branch = self.current_branch(path).await.ok();
        Ok(Repo {
            path: path.to_string_lossy().to_string(),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string()),
            default_branch: branch,
        })
    }

    // ── ListRepos / AddRepo / CloneRepo / CreateRepo ────────────────────────

    /// Known repos that still exist, each with its current branch. Never fails:
    /// vanished paths and non-repos are silently dropped.
    pub async fn list(&self) -> Vec<Repo> {
        let mut repos = Vec::new();
        for path in self.load_paths() {
            let path = PathBuf::from(path);
            if !Self::path_exists(&path).await || !self.is_repo(&path).await {
                continue;
            }
            match self.to_repo(&path).await {
                Ok(repo) => repos.push(repo),
                Err(err) => {
                    tracing::debug!(path = %path.display(), error = %err, "repo listing skip")
                }
            }
        }
        repos
    }

    /// Remember an existing repository the user pointed us at.
    pub async fn add(&self, path: &str) -> Result<Repo, VcsError> {
        let abs = absolutize(Path::new(path));
        if !Self::path_exists(&abs).await {
            return Err(VcsError::new(format!("No such folder: {}", abs.display())));
        }
        if !self.is_repo(&abs).await {
            return Err(VcsError::new(format!(
                "Not a {} repository: {}",
                self.vcs_kind().map(VcsKind::label).unwrap_or("VCS"),
                abs.display()
            )));
        }
        self.register(&abs.to_string_lossy())?;
        self.to_repo(&abs).await
    }

    /// `git clone <url>` under `{data_dir}/repos`. (Named `clone_repo` to keep
    /// `Clone::clone` unambiguous on the service handle.)
    pub async fn clone_repo(&self, url: &str) -> Result<Repo, VcsError> {
        let trimmed = url.trim().trim_end_matches('/');
        let name = trimmed
            .trim_end_matches(".git")
            .rsplit(['/', ':'])
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("repo")
            .to_string();
        let repos_dir = self.inner.data_dir.join("repos");
        let target = repos_dir.join(&name);
        if target.exists() {
            return Err(VcsError::new(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&repos_dir)?;
        match self.vcs_kind() {
            Some(VcsKind::Git) => {
                self.git(&["clone", trimmed, &target.to_string_lossy()], None)
                    .await?;
            }
            Some(VcsKind::Jujutsu) => {
                self.jj(
                    &[
                        "git",
                        "clone",
                        "--colocate",
                        trimmed,
                        &target.to_string_lossy(),
                    ],
                    None,
                    false,
                )
                .await?;
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        }
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    /// `git init -b main` a fresh repository under `{data_dir}/repos`.
    pub async fn create(&self, name: &str) -> Result<Repo, VcsError> {
        let clean: String = name
            .trim()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if clean.is_empty() || clean.chars().all(|c| c == '-' || c == '.') {
            return Err(VcsError::new("Invalid repository name"));
        }
        let target = self.inner.data_dir.join("repos").join(&clean);
        if target.exists() {
            return Err(VcsError::new(format!(
                "Already exists: {}",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)?;
        match self.vcs_kind() {
            Some(VcsKind::Git) => {
                self.git(&["init", "-b", "main"], Some(&target)).await?;
            }
            Some(VcsKind::Jujutsu) => {
                self.jj(&["git", "init", "--colocate", "."], Some(&target), false)
                    .await?;
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        }
        self.register(&target.to_string_lossy())?;
        self.to_repo(&target).await
    }

    // ── branches ────────────────────────────────────────────────────────────

    /// All branches (`git branch -a`), local first, deduped against their remote
    /// counterparts, with the repo's default branch first.
    pub async fn branches(&self, repo_path: &Path) -> Result<Vec<String>, VcsError> {
        if self.vcs_kind() == Some(VcsKind::Jujutsu) {
            return Ok(self
                .jj_bookmarks(repo_path)
                .await?
                .into_iter()
                .map(|row| row.name)
                .collect());
        }
        let out = self
            .git(&["branch", "-a", "--format=%(refname)"], Some(repo_path))
            .await?;
        let mut names: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |name: &str| {
            if !name.is_empty() && name != "HEAD" && seen.insert(name.to_string()) {
                names.push(name.to_string());
            }
        };
        // Locals first, then remote-only branches (stripped of their remote prefix).
        for line in out.lines().map(str::trim) {
            if let Some(local) = line.strip_prefix("refs/heads/") {
                push(local);
            }
        }
        for line in out.lines().map(str::trim) {
            if let Some(remote) = line.strip_prefix("refs/remotes/")
                && let Some((_, name)) = remote.split_once('/')
            {
                push(name);
            }
        }
        // Default branch first: origin/HEAD's target, else the checked-out branch.
        let default = match self
            .git(
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
                Some(repo_path),
            )
            .await
        {
            Ok(short) => short.split_once('/').map(|(_, b)| b.to_string()),
            Err(_) => None,
        };
        let default = match default {
            Some(branch) => Some(branch),
            None => self
                .current_branch(repo_path)
                .await
                .ok()
                .filter(|b| b != "HEAD"),
        };
        if let Some(default) = default
            && let Some(pos) = names.iter().position(|n| *n == default)
        {
            let head = names.remove(pos);
            names.insert(0, head);
        }
        Ok(names)
    }

    fn user_worktree_path(&self, path: &str) -> String {
        let path = PathBuf::from(path);
        let canonical_root = std::fs::canonicalize(&self.inner.worktrees_root)
            .unwrap_or_else(|_| self.inner.worktrees_root.clone());
        path.strip_prefix(&canonical_root)
            .ok()
            .map(|relative| self.inner.worktrees_root.join(relative))
            .filter(|candidate| candidate.exists())
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// [`Self::branches`] enriched with checkout state: which branch the MAIN
    /// folder has checked out (`current`) and which branches are materialized
    /// as linked worktrees (`worktree_path`). Feeds the composer's ref picker
    /// and its checkout-kind selector.
    pub async fn refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, VcsError> {
        if self.vcs_kind() == Some(VcsKind::Jujutsu) {
            return self.jj_refs(repo_path).await;
        }
        let names = self.branches(repo_path).await?;
        let current = self.current_branch(repo_path).await.ok();
        // `git worktree list --porcelain`: stanzas of `worktree <path>` /
        // `HEAD <sha>` / `branch refs/heads/<name>`. The first stanza is the
        // main checkout — excluded (it's `current`, not a linked worktree).
        let mut worktrees: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        if let Ok(out) = self
            .git(&["worktree", "list", "--porcelain"], Some(repo_path))
            .await
        {
            let mut stanza = 0usize;
            let mut path: Option<String> = None;
            for line in out.lines().map(str::trim) {
                if let Some(p) = line.strip_prefix("worktree ") {
                    stanza += 1;
                    // The first stanza is the main checkout, not a linked tree.
                    path = (stanza > 1).then(|| self.user_worktree_path(p));
                } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
                    && let Some(path) = path.take()
                {
                    worktrees.insert(branch.to_string(), path);
                }
            }
        }
        Ok(names
            .into_iter()
            .map(|name| RepoRef {
                revision: name.clone(),
                kind: RepoRefKind::Branch,
                current: current.as_deref() == Some(name.as_str()),
                worktree_path: worktrees.get(&name).cloned(),
                name,
            })
            .collect())
    }

    async fn jj_bookmarks(&self, repo_path: &Path) -> Result<Vec<RepoRef>, VcsError> {
        let out = self
            .jj(
                &[
                    "bookmark",
                    "list",
                    "--all-remotes",
                    "-T",
                    "name ++ \"\\t\" ++ remote ++ \"\\n\"",
                ],
                Some(repo_path),
                true,
            )
            .await?;
        let mut seen = HashSet::new();
        Ok(out
            .lines()
            .filter_map(|line| {
                let (name, remote) = line.split_once('\t')?;
                (remote.is_empty() && !name.is_empty() && seen.insert(name.to_string())).then(
                    || RepoRef {
                        name: name.to_string(),
                        revision: name.to_string(),
                        kind: RepoRefKind::Bookmark,
                        current: false,
                        worktree_path: None,
                    },
                )
            })
            .collect())
    }

    async fn jj_refs(&self, repo_path: &Path) -> Result<Vec<RepoRef>, VcsError> {
        let current_root = self.jj(&["root"], Some(repo_path), true).await?;
        let current_root =
            std::fs::canonicalize(&current_root).unwrap_or_else(|_| PathBuf::from(&current_root));
        let out = self
            .jj(
                &[
                    "workspace",
                    "list",
                    "-T",
                    "name ++ \"\\t\" ++ target.change_id().shortest(8) ++ \"\\t\" ++ target.current_working_copy() ++ \"\\t\" ++ root ++ \"\\n\"",
                ],
                Some(repo_path),
                true,
            )
            .await?;
        let mut rows = Vec::new();
        for line in out.lines() {
            let mut fields = line.splitn(4, '\t');
            let (Some(workspace), Some(change_id), Some(target_is_current), Some(root)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            // Repositories initialized by older JJ versions can omit the current
            // workspace path from the repo store. JJ renders `root` as an error
            // string in that case even though `jj root` above resolved it. Never
            // expose that diagnostic as a selectable checkout path.
            let missing_current_root = target_is_current == "true"
                && root.starts_with("<Error: Workspace has no recorded path:");
            let root_path = if missing_current_root {
                current_root.clone()
            } else {
                PathBuf::from(root)
            };
            let canonical = std::fs::canonicalize(&root_path).unwrap_or_else(|_| root_path.clone());
            let current = canonical == current_root;
            rows.push(RepoRef {
                name: change_id.to_string(),
                revision: if current {
                    "@".to_string()
                } else {
                    format!("{workspace}@")
                },
                kind: RepoRefKind::WorkingCopy,
                current,
                worktree_path: (!current).then(|| root_path.to_string_lossy().into_owned()),
            });
        }
        rows.sort_by_key(|row| !row.current);
        rows.extend(self.jj_bookmarks(repo_path).await?);
        Ok(rows)
    }

    /// Whether `candidate` is the repository root or one of its linked
    /// worktrees. Filesystem resolution happens on a disposable thread because
    /// user-selected paths may be dead mounts.
    pub async fn workspace_checkout(&self, repo_path: &Path, candidate: &Path) -> Option<PathBuf> {
        let repo_path = repo_path.to_path_buf();
        let candidate = candidate.to_path_buf();
        let worktrees: Vec<_> = self
            .refs(&repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.worktree_path.map(PathBuf::from))
            .collect();
        disposable_worker("checkout-auth", move || {
            let candidate = std::fs::canonicalize(candidate).ok()?;
            std::iter::once(repo_path)
                .chain(worktrees)
                .filter_map(|path| std::fs::canonicalize(path).ok())
                .any(|path| path == candidate)
                .then_some(candidate)
        })
        .await
        .flatten()
    }

    /// Switch the checkout at `cwd` (a main folder OR a linked worktree) to
    /// `ref_name` — the t3code `switchRef` port: an existing local branch is
    /// checked out directly; a remote-only branch gets a local tracking
    /// branch (`checkout --track origin/<ref>`). A dirty tree or a branch
    /// already checked out in another worktree fails with git's own message.
    /// Returns the resulting current branch.
    pub async fn switch_ref(&self, cwd: &Path, ref_name: &str) -> Result<String, VcsError> {
        if self.vcs_kind() == Some(VcsKind::Jujutsu) {
            if ref_name != "@" {
                self.jj(&["new", ref_name], Some(cwd), false).await?;
            }
            return self.working_copy_label(cwd).await;
        }
        let local = self
            .git(
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{ref_name}"),
                ],
                Some(cwd),
            )
            .await
            .is_ok();
        if local {
            self.git(&["checkout", ref_name], Some(cwd)).await?;
        } else {
            let remote = format!("origin/{ref_name}");
            let has_remote = self
                .git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/remotes/{remote}"),
                    ],
                    Some(cwd),
                )
                .await
                .is_ok();
            if has_remote {
                self.git(&["checkout", "--track", &remote], Some(cwd))
                    .await?;
            } else {
                // Unknown ref: let git produce the authoritative error.
                self.git(&["checkout", ref_name], Some(cwd)).await?;
            }
        }
        let out = self.git(&["branch", "--show-current"], Some(cwd)).await?;
        Ok(out.trim().to_string())
    }

    // ── worktrees ───────────────────────────────────────────────────────────

    /// `git worktree add` an isolated checkout under
    /// `{worktrees_root}/<repoName>/<generatedName>`, on a fresh `jolt/<name>`
    /// branch off `branch`.
    pub async fn create_worktree(
        &self,
        repo_path: &Path,
        branch: &str,
    ) -> Result<Worktree, VcsError> {
        let _managed_guard = self.inner.managed_workspace_lock.lock().await;
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let base = match self.vcs_kind() {
            Some(VcsKind::Jujutsu) => self.inner.workspaces_root.join(&repo_name),
            _ => self.inner.worktrees_root.join(&repo_name),
        };
        std::fs::create_dir_all(&base)?;
        // Auto-generate a name colliding with neither an existing dir nor branch.
        let existing: HashSet<String> = self
            .branches(repo_path)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut name = None;
        for attempt in 0..50u64 {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64)
                .unwrap_or(attempt)
                .wrapping_add(attempt.wrapping_mul(0x9E37_79B9));
            let candidate = format!(
                "{}-{}",
                ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize],
                NOUNS[((seed / 31) % NOUNS.len() as u64) as usize]
            );
            if !base.join(&candidate).exists() && !existing.contains(&format!("jolt/{candidate}")) {
                name = Some(candidate);
                break;
            }
        }
        let name = name.ok_or_else(|| VcsError::new("Could not allocate a worktree name"))?;
        let path = base.join(&name);
        let branch_name = match self.vcs_kind() {
            Some(VcsKind::Git) => {
                let branch_name = format!("jolt/{name}");
                self.git(
                    &[
                        "worktree",
                        "add",
                        "-b",
                        &branch_name,
                        &path.to_string_lossy(),
                        branch,
                    ],
                    Some(repo_path),
                )
                .await?;
                branch_name
            }
            Some(VcsKind::Jujutsu) => {
                self.jj(
                    &[
                        "workspace",
                        "add",
                        "--name",
                        &name,
                        "-r",
                        branch,
                        &path.to_string_lossy(),
                    ],
                    Some(repo_path),
                    false,
                )
                .await?;
                self.working_copy_label(&path).await?
            }
            None => {
                return Err(VcsError::new("No supported VCS executable found"));
            }
        };
        let checkout = self.checkout_identity(&path).await?;
        let worktree = Worktree {
            repo_path: repo_path.to_string_lossy().to_string(),
            path: path.to_string_lossy().to_string(),
            branch: branch_name,
            name,
            checkout_id: Some(checkout.id),
        };
        let Some(vcs_kind) = self.vcs_kind() else {
            return Err(VcsError::new("No supported VCS executable found"));
        };
        if let Err(error) =
            self.inner
                .managed_workspaces
                .register(vcs_kind, &worktree, unix_now_ms())
        {
            let _ = self
                .delete_worktree(repo_path, Path::new(&worktree.path))
                .await;
            return Err(error);
        }
        Ok(worktree)
    }

    async fn branch_exists(&self, path: &Path, branch: &str) -> bool {
        self.git(
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            Some(path),
        )
        .await
        .is_ok()
    }

    /// Rename a Jolt-created worktree branch after its chat's generated title.
    /// Guards:
    /// - respect an external checkout/rename: only act while the worktree is still
    ///   on `expected_branch` AND that branch is the original `jolt/<folderName>`;
    /// - a title-slug collision gets a stable 6-hex suffix (hash of the worktree
    ///   path); a collision on THAT too fails.
    ///
    /// Returns the branch the worktree ends up on (re-read after the rename so a
    /// concurrent external checkout always wins the metadata race).
    pub async fn rename_worktree_branch(
        &self,
        worktree_path: &Path,
        expected_branch: &str,
        title: &str,
    ) -> Result<String, VcsError> {
        let current = self.current_branch(worktree_path).await?;
        if self.vcs_kind() == Some(VcsKind::Jujutsu) {
            return self.current_branch(worktree_path).await;
        }
        let folder = worktree_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if current != expected_branch || expected_branch != format!("jolt/{folder}") {
            return Ok(current);
        }
        let preferred = worktree_branch_from_title(title);
        if preferred == current {
            return Ok(current);
        }
        let mut hasher = Sha256::new();
        hasher.update(worktree_path.to_string_lossy().as_bytes());
        let suffix = &hex(&hasher.finalize())[..6];
        let target = if self.branch_exists(worktree_path, &preferred).await {
            format!("{preferred}-{suffix}")
        } else {
            preferred
        };
        if self.branch_exists(worktree_path, &target).await {
            return Err(VcsError::new(format!("Branch already exists: {target}")));
        }
        self.git(
            &["branch", "-m", "--", &current, &target],
            Some(worktree_path),
        )
        .await?;
        self.current_branch(worktree_path).await
    }

    /// Best-effort worktree removal (if it still exists), then prune stale refs.
    /// Deletes the worktree's branch ONLY when jolt created it (`jolt/…`) — the
    /// user may have checked out their own branch inside the worktree.
    pub async fn delete_worktree(
        &self,
        repo_path: &Path,
        worktree_path: &Path,
    ) -> Result<(), VcsError> {
        if self.vcs_kind() == Some(VcsKind::Jujutsu) {
            let out = self
                .jj(
                    &[
                        "workspace",
                        "list",
                        "-T",
                        "name ++ \"\\t\" ++ root ++ \"\\n\"",
                    ],
                    Some(repo_path),
                    true,
                )
                .await?;
            let canonical = std::fs::canonicalize(worktree_path)
                .unwrap_or_else(|_| worktree_path.to_path_buf());
            let workspace = out.lines().find_map(|line| {
                let (name, root) = line.split_once('\t')?;
                let root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
                (root == canonical).then(|| name.to_string())
            });
            if let Some(workspace) = workspace {
                self.jj(&["workspace", "forget", &workspace], Some(repo_path), true)
                    .await?;
            }
            if worktree_path.exists() {
                std::fs::remove_dir_all(worktree_path)?;
            }
            self.inner.managed_workspaces.forget_path(worktree_path)?;
            return Ok(());
        }
        let branch = if worktree_path.exists() {
            self.current_branch(worktree_path).await.unwrap_or_default()
        } else {
            String::new()
        };
        if worktree_path.exists() {
            let removed = self
                .git(
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &worktree_path.to_string_lossy(),
                    ],
                    Some(repo_path),
                )
                .await;
            if removed.is_err() {
                // git refused (or the dir is half-gone) — delete the folder directly.
                let _ = std::fs::remove_dir_all(worktree_path);
            }
        }
        let _ = self.git(&["worktree", "prune"], Some(repo_path)).await;
        if branch.starts_with("jolt/") {
            let _ = self.git(&["branch", "-D", &branch], Some(repo_path)).await;
        }
        self.inner.managed_workspaces.forget_path(worktree_path)?;
        Ok(())
    }

    /// Publish this runtime scope's live checkout references without removing
    /// any workspace. Reference changes still start or reset orphan grace.
    pub async fn publish_managed_workspace_references(
        &self,
        references: &[WorkspaceReference],
    ) -> Result<(), VcsError> {
        let _managed_guard = self.inner.managed_workspace_lock.lock().await;
        self.inner
            .managed_workspaces
            .reconcile(references, unix_now_ms(), Duration::MAX)?;
        Ok(())
    }

    /// Publish this runtime scope's live checkout references and reap clean,
    /// Jolt-created workspaces whose orphan grace period has elapsed.
    pub async fn reconcile_managed_workspaces(
        &self,
        references: &[WorkspaceReference],
        grace: Duration,
    ) -> Result<WorkspaceCleanupReport, VcsError> {
        self.reconcile_managed_workspaces_at(references, unix_now_ms(), grace)
            .await
    }

    async fn reconcile_managed_workspaces_at(
        &self,
        references: &[WorkspaceReference],
        now_ms: i64,
        grace: Duration,
    ) -> Result<WorkspaceCleanupReport, VcsError> {
        let _managed_guard = self.inner.managed_workspace_lock.lock().await;
        let due = self
            .inner
            .managed_workspaces
            .reconcile(references, now_ms, grace)?;
        let mut report = WorkspaceCleanupReport::default();
        for record in due {
            let action_lock = self.action_lock(&record.checkout_id);
            let _action_guard = action_lock.lock().await;
            let still_due = self
                .inner
                .managed_workspaces
                .reconcile(references, unix_now_ms().max(now_ms), grace)?
                .into_iter()
                .any(|candidate| candidate.checkout_id == record.checkout_id);
            if !still_due {
                continue;
            }
            let path = Path::new(&record.workspace_path);
            if !path.exists() {
                match self.prune_missing_managed_workspace(&record).await {
                    Ok(()) => {
                        self.inner.managed_workspaces.forget_path(path)?;
                        report.removed += 1;
                    }
                    Err(error) => {
                        report.failed += 1;
                        tracing::debug!(
                            path = %record.workspace_path,
                            error = %error,
                            "missing managed workspace metadata cleanup skipped"
                        );
                    }
                }
                continue;
            }
            match self.managed_workspace_is_clean(&record).await {
                Ok(true) => match self.reap_managed_workspace(&record).await {
                    Ok(()) => {
                        self.inner.managed_workspaces.forget_path(path)?;
                        report.removed += 1;
                    }
                    Err(error) => {
                        report.failed += 1;
                        tracing::warn!(
                            path = %record.workspace_path,
                            error = %error,
                            "managed workspace cleanup failed"
                        );
                    }
                },
                Ok(false) => report.dirty += 1,
                Err(error) => {
                    report.failed += 1;
                    tracing::debug!(
                        path = %record.workspace_path,
                        error = %error,
                        "managed workspace cleanup skipped"
                    );
                }
            }
        }
        Ok(report)
    }

    async fn managed_workspace_is_clean(
        &self,
        record: &ManagedWorkspace,
    ) -> Result<bool, VcsError> {
        if self.vcs_kind() != Some(record.vcs_kind) {
            return Err(VcsError::new("managed workspace backend is not active"));
        }
        let root = match record.vcs_kind {
            VcsKind::Git => &self.inner.worktrees_root,
            VcsKind::Jujutsu => &self.inner.workspaces_root,
        };
        let root = std::fs::canonicalize(root)?;
        let path = std::fs::canonicalize(&record.workspace_path)?;
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| VcsError::new("managed workspace escaped its Jolt root"))?;
        if relative.components().count() < 2 {
            return Err(VcsError::new(
                "managed workspace path is not a child workspace",
            ));
        }
        if self
            .workspace_checkout(Path::new(&record.repo_path), &path)
            .await
            .is_none()
        {
            return Err(VcsError::new(
                "managed workspace no longer belongs to its recorded repository",
            ));
        }
        let status = match record.vcs_kind {
            VcsKind::Git => {
                self.git(
                    &["status", "--porcelain=v1", "--untracked-files=all"],
                    Some(&path),
                )
                .await?
            }
            VcsKind::Jujutsu => {
                self.jj(&["diff", "-r", "@", "--summary"], Some(&path), false)
                    .await?
            }
        };
        Ok(status.trim().is_empty())
    }

    async fn reap_managed_workspace(&self, record: &ManagedWorkspace) -> Result<(), VcsError> {
        let repo_path = Path::new(&record.repo_path);
        let workspace_path = Path::new(&record.workspace_path);
        match record.vcs_kind {
            VcsKind::Git => {
                self.git(
                    &["worktree", "remove", &workspace_path.to_string_lossy()],
                    Some(repo_path),
                )
                .await?;
                let _ = self.git(&["worktree", "prune"], Some(repo_path)).await;
            }
            VcsKind::Jujutsu => {
                let workspace = self
                    .jj_workspace_name(repo_path, workspace_path)
                    .await?
                    .ok_or_else(|| VcsError::new("JJ workspace is no longer registered"))?;
                let suffix = record.checkout_id.get(..8).unwrap_or(&record.checkout_id);
                let retained = format!("jolt-retained/{}-{suffix}", record.name);
                self.jj(
                    &["bookmark", "set", &retained, "-r", "@"],
                    Some(workspace_path),
                    false,
                )
                .await?;
                std::fs::remove_dir_all(workspace_path)?;
                self.jj(&["workspace", "forget", &workspace], Some(repo_path), true)
                    .await?;
            }
        }
        Ok(())
    }

    async fn prune_missing_managed_workspace(
        &self,
        record: &ManagedWorkspace,
    ) -> Result<(), VcsError> {
        if self.vcs_kind() != Some(record.vcs_kind) {
            return Err(VcsError::new("managed workspace backend is not active"));
        }
        let repo_path = Path::new(&record.repo_path);
        match record.vcs_kind {
            VcsKind::Git => {
                self.git(&["worktree", "prune"], Some(repo_path)).await?;
            }
            VcsKind::Jujutsu => {
                if let Some(workspace) = self
                    .jj_workspace_name(repo_path, Path::new(&record.workspace_path))
                    .await?
                {
                    self.jj(&["workspace", "forget", &workspace], Some(repo_path), true)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn jj_workspace_name(
        &self,
        repo_path: &Path,
        workspace_path: &Path,
    ) -> Result<Option<String>, VcsError> {
        let out = self
            .jj(
                &[
                    "workspace",
                    "list",
                    "-T",
                    "name ++ \"\\t\" ++ root ++ \"\\n\"",
                ],
                Some(repo_path),
                true,
            )
            .await?;
        let canonical =
            std::fs::canonicalize(workspace_path).unwrap_or_else(|_| workspace_path.to_path_buf());
        Ok(out.lines().find_map(|line| {
            let (name, root) = line.split_once('\t')?;
            let root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
            (root == canonical).then(|| name.to_string())
        }))
    }

    // ── ListFolders ─────────────────────────────────────────────────────────

    /// One directory level (home by default): dotfiles hidden, directories first,
    /// capped at [`FOLDER_LIST_MAX_ENTRIES`] with a `truncated` flag. The walk runs
    /// in a spawned blocking task under a 6s wall-clock ceiling — a wedged path
    /// (dead mount, permission-gated folder) fails this listing without blocking
    /// anything else; the abandoned task unwinds on its own thread.
    pub async fn list_folders(&self, path: Option<String>) -> Result<FolderListing, VcsError> {
        self.list_folders_with(path, FOLDER_LIST_TIMEOUT, false)
            .await
    }

    /// Search a checkout's files and directories by fuzzy relative path. The
    /// `ignore` walker honors `.gitignore`, `.ignore`, and global git excludes.
    /// Dotfiles remain searchable; only repository metadata is always pruned.
    pub async fn search_files(
        &self,
        root: PathBuf,
        query: String,
        featured_paths: Vec<String>,
    ) -> Result<Vec<FileSearchMatch>, VcsError> {
        let deadline = tokio::time::Instant::now() + FILE_SEARCH_TIMEOUT;
        let gate = {
            let mut searches = self
                .inner
                .file_searches
                .lock()
                .map_err(|_| VcsError::new("file search registry poisoned"))?;
            if let Some(gate) = searches.get(&root).and_then(std::sync::Weak::upgrade) {
                gate
            } else {
                let gate = std::sync::Arc::new(tokio::sync::Mutex::new(()));
                searches.insert(root.clone(), std::sync::Arc::downgrade(&gate));
                gate
            }
        };
        let gate = tokio::time::timeout_at(deadline, gate.lock_owned())
            .await
            .map_err(|_| VcsError::new("file search timed out"))?;
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let worker_cancelled = cancelled.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("file-search".to_string())
            .spawn(move || {
                let _gate = gate;
                let _ = tx.send(search_files_blocking_with_cancel(
                    &root,
                    &query,
                    &featured_paths,
                    || worker_cancelled.load(Ordering::Relaxed),
                ));
            })
            .map_err(|e| VcsError::new(format!("file search failed: {e}")))?;
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(VcsError::new("file search worker exited")),
            Err(_) => Err(VcsError::new("file search timed out")),
        }
    }

    /// `hang_for_test` makes the worker never respond — exercises the timeout path.
    ///
    /// The walk runs on a DETACHED OS thread (not the tokio blocking pool): a
    /// readdir wedged in the kernel can't be cancelled, and a poisoned blocking
    /// pool — or a runtime shutdown waiting on it — must never be possible. On
    /// timeout the thread is simply abandoned (the jolt backend's disposable
    /// worker, minus the terminate()).
    #[doc(hidden)]
    pub async fn list_folders_with(
        &self,
        path: Option<String>,
        timeout: Duration,
        hang_for_test: bool,
    ) -> Result<FolderListing, VcsError> {
        let target = match path.filter(|p| !p.trim().is_empty()) {
            Some(p) => absolutize(Path::new(&p)),
            None => home_dir(),
        };
        let vcs_kind = self.vcs_kind();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("folder-list".to_string())
            .spawn(move || {
                if hang_for_test {
                    // Hold the sender without responding (detached thread; process
                    // exit reclaims it) — the caller must hit its timeout.
                    std::thread::sleep(Duration::from_secs(3600));
                }
                let _ = tx.send(list_folders_blocking(&target, vcs_kind));
            });
        if let Err(err) = spawned {
            return Err(VcsError::new(format!("folder listing failed: {err}")));
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(VcsError::new("folder listing worker exited")),
            Err(_) => Err(VcsError::new("folder listing timed out on the device")),
        }
    }
}

struct CancelOnDrop(std::sync::Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn disposable_worker<T: Send + 'static>(
    name: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

/// The blocking walk: ONE readdir of the target; `is_repo` is a cheap `.git`
/// existence probe per directory entry.
fn list_folders_blocking(
    target: &Path,
    vcs_kind: Option<VcsKind>,
) -> Result<FolderListing, VcsError> {
    let read = std::fs::read_dir(target).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            VcsError::new("Jolt doesn't have access to this folder on the device.")
        }
        _ => VcsError::new(format!("could not read that folder: {e}")),
    })?;
    let mut entries: Vec<FolderEntry> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let metadata = match vcs_kind {
            Some(VcsKind::Jujutsu) => ".jj",
            _ => ".git",
        };
        let is_repo = is_dir && entry.path().join(metadata).exists();
        entries.push(FolderEntry {
            name,
            is_dir,
            is_repo,
        });
    }
    // Directories first, each group name-sorted (case-insensitive).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FOLDER_LIST_MAX_ENTRIES;
    entries.truncate(FOLDER_LIST_MAX_ENTRIES);
    Ok(FolderListing {
        path: target.to_string_lossy().to_string(),
        entries,
        truncated,
    })
}

/// Case-insensitive subsequence score. Lower is better: adjacent and earlier
/// characters win, while still allowing `cmp rs` to find `composer.rs`.
fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    let candidate = candidate.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .try_fold(0usize, |total, term| {
            let mut at = 0;
            let mut score = 0usize;
            let mut previous_end = None;
            for needle in term.chars() {
                let found = candidate[at..].find(needle)? + at;
                score += found;
                if previous_end == Some(found) {
                    score = score.saturating_sub(2);
                }
                at = found + needle.len_utf8();
                previous_end = Some(at);
            }
            Some(total.saturating_add(score))
        })
}

type RankedFileMatch = (Option<usize>, usize, String, bool);

fn compare_file_matches(
    query: &str,
    (featured_a, score_a, path_a, dir_a): &RankedFileMatch,
    (featured_b, score_b, path_b, dir_b): &RankedFileMatch,
) -> std::cmp::Ordering {
    let empty_query = query.trim().is_empty();
    featured_a
        .is_none()
        .cmp(&featured_b.is_none())
        .then_with(|| featured_a.cmp(featured_b))
        .then_with(|| score_a.cmp(score_b))
        .then_with(|| {
            if empty_query {
                path_a.split('/').count().cmp(&path_b.split('/').count())
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| {
            if empty_query {
                dir_a.cmp(dir_b)
            } else {
                dir_b.cmp(dir_a)
            }
        })
        .then_with(|| path_a.len().cmp(&path_b.len()))
        .then_with(|| path_a.cmp(path_b))
}

#[cfg(test)]
fn search_files_blocking(
    root: &Path,
    query: &str,
    featured_paths: &[String],
) -> Result<Vec<FileSearchMatch>, VcsError> {
    search_files_blocking_with_cancel(root, query, featured_paths, || false)
}

fn search_files_blocking_with_cancel<F: Fn() -> bool>(
    root: &Path,
    query: &str,
    featured_paths: &[String],
    cancelled: F,
) -> Result<Vec<FileSearchMatch>, VcsError> {
    let root = std::fs::canonicalize(root)
        .map_err(|e| VcsError::new(format!("could not search workspace: {e}")))?;
    let featured: HashMap<String, usize> = featured_paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let full = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let canonical = std::fs::canonicalize(full).ok()?;
            let relative = canonical.strip_prefix(&root).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .enumerate()
        .fold(HashMap::new(), |mut paths, (rank, path)| {
            paths.entry(path).or_insert(rank);
            paths
        });
    let mut matches = Vec::new();
    let walker = ignore::WalkBuilder::new(&root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|entry| {
            entry.depth() == 0 || (entry.file_name() != ".git" && entry.file_name() != ".jj")
        })
        .build();
    for entry in walker {
        if cancelled() {
            return Err(VcsError::new("file search cancelled"));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                tracing::debug!(%err, "file mention search walk skipped entry");
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let Ok(relative) = path.strip_prefix(&root) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.starts_with(".git/") || relative == ".git" {
            continue;
        }
        let Some(path_score) = fuzzy_score(query, &relative) else {
            continue;
        };
        let score = relative
            .rsplit('/')
            .next()
            .and_then(|name| fuzzy_score(query, name))
            .unwrap_or_else(|| path_score.saturating_add(1_000));
        matches.push((
            featured.get(&relative).copied(),
            score,
            relative,
            entry.file_type().is_some_and(|kind| kind.is_dir()),
        ));
        if matches.len() > FILE_SEARCH_MAX_RESULTS {
            matches.sort_by(|a, b| compare_file_matches(query, a, b));
            matches.truncate(FILE_SEARCH_MAX_RESULTS);
        }
    }
    matches.sort_by(|a, b| compare_file_matches(query, a, b));
    Ok(matches
        .into_iter()
        .map(|(_, _, path, is_dir)| FileSearchMatch { path, is_dir })
        .collect())
}

/// Turn a generated chat title into the semantic portion of a Jolt branch.
/// Generated titles are English, so non-ASCII characters collapse into the
/// `-` separator.
pub fn worktree_branch_from_title(title: &str) -> String {
    let mut slug = String::new();
    for c in title.trim().chars() {
        if matches!(c, '\'' | '"' | '`') {
            continue; // dropped entirely (cafe's → cafes), not a separator
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(48);
    let slug = slug.trim_matches('-');
    format!("jolt/{}", if slug.is_empty() { "update" } else { slug })
}

/// Absolute form of a possibly-relative path (no filesystem access).
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

#[doc(hidden)]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_score_matches_a_path_subsequence() {
        assert!(fuzzy_score("cmp rs", "crates/ui/src/composer.rs").is_some());
        assert!(fuzzy_score("composer crates", "crates/ui/src/composer.rs").is_some());
        assert!(fuzzy_score("xyz", "crates/ui/src/composer.rs").is_none());
    }

    #[test]
    fn search_files_obeys_gitignore_and_returns_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();
        std::fs::write(root.path().join(".secret"), "").unwrap();
        std::fs::write(root.path().join(".gitignore"), "ignored\n").unwrap();
        std::fs::create_dir(root.path().join("ignored")).unwrap();
        std::fs::write(root.path().join("ignored/nope.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "src", &[]).unwrap();
        assert!(
            matches
                .iter()
                .any(|entry| entry.path == "src" && entry.is_dir)
        );
        assert!(matches.iter().any(|entry| entry.path == "src/composer.rs"));
        assert!(
            !matches
                .iter()
                .any(|entry| entry.path.starts_with("ignored"))
        );
        assert!(
            search_files_blocking(root.path(), "secret", &[])
                .unwrap()
                .iter()
                .any(|entry| entry.path == ".secret")
        );
        assert!(!matches.iter().any(|entry| entry.path.starts_with(".git/")));
    }

    #[test]
    fn empty_search_features_recent_paths_before_shallow_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        std::fs::write(root.path().join("src/deep.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "", &["src/deep.rs".to_string()]).unwrap();
        assert_eq!(
            matches.first().map(|entry| entry.path.as_str()),
            Some("src/deep.rs")
        );
        assert!(matches.iter().any(|entry| entry.path == "README.md"));
    }

    #[test]
    fn search_files_prefers_filename_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("composer/docs")).unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("composer/docs/readme.md"), "").unwrap();
        std::fs::write(root.path().join("src/composer.rs"), "").unwrap();

        let matches = search_files_blocking(root.path(), "composer", &[]).unwrap();
        let composer = matches
            .iter()
            .position(|entry| entry.path == "src/composer.rs")
            .unwrap();
        let path_only = matches
            .iter()
            .position(|entry| entry.path == "composer/docs/readme.md")
            .unwrap();
        assert!(composer < path_only);
    }

    #[test]
    fn cancelled_search_stops_before_walking() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        let cancelled = AtomicBool::new(true);

        let err = search_files_blocking_with_cancel(root.path(), "", &[], || {
            cancelled.load(Ordering::Relaxed)
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"));
    }

    async fn git_repo_with_initial_commit(repos: &Repos, name: &str) -> PathBuf {
        let repo = repos.create(name).await.unwrap();
        let root = PathBuf::from(repo.path);
        repos
            .git(&["config", "user.email", "test@example.com"], Some(&root))
            .await
            .unwrap();
        repos
            .git(&["config", "user.name", "Test User"], Some(&root))
            .await
            .unwrap();
        std::fs::write(root.join("README.md"), "initial\n").unwrap();
        repos
            .commit_changes(&root, None, "Initial commit")
            .await
            .unwrap();
        root
    }

    #[tokio::test]
    async fn orphan_cleanup_removes_clean_worktree_but_preserves_branch() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let root = git_repo_with_initial_commit(&repos, "cleanup-clean").await;
        let worktree = repos.create_worktree(&root, "main").await.unwrap();

        let report = repos
            .reconcile_managed_workspaces_at(&[], 100, Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(report.removed, 1);
        assert!(!Path::new(&worktree.path).exists());
        repos
            .git(
                &[
                    "show-ref",
                    "--verify",
                    &format!("refs/heads/{}", worktree.branch),
                ],
                Some(&root),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn orphan_cleanup_skips_dirty_worktree() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let root = git_repo_with_initial_commit(&repos, "cleanup-dirty").await;
        let worktree = repos.create_worktree(&root, "main").await.unwrap();
        std::fs::write(Path::new(&worktree.path).join("dirty.txt"), "keep\n").unwrap();

        let report = repos
            .reconcile_managed_workspaces_at(&[], 100, Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(report.dirty, 1);
        assert_eq!(report.removed, 0);
        assert!(Path::new(&worktree.path).exists());

        std::fs::remove_file(Path::new(&worktree.path).join("dirty.txt")).unwrap();
        let report = repos
            .reconcile_managed_workspaces_at(&[], 200, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(report.removed, 1);
        assert!(!Path::new(&worktree.path).exists());
    }

    #[tokio::test]
    async fn git_review_context_uses_current_branch_and_dedupes_remote_urls() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let repo = repos.create("review-context").await.unwrap();
        let root = PathBuf::from(repo.path);
        repos
            .git(
                &["remote", "add", "origin", "git@github.com:owner/repo.git"],
                Some(&root),
            )
            .await
            .unwrap();

        assert_eq!(repos.review_references(&root).await.unwrap(), ["main"]);
        assert_eq!(
            repos.remote_urls(&root).await.unwrap(),
            ["git@github.com:owner/repo.git"]
        );
    }

    #[tokio::test]
    async fn jujutsu_repo_working_copy_and_workspace_round_trip() {
        let data = tempfile::tempdir().unwrap();
        let repos = Repos::with_roots(
            data.path(),
            "device",
            data.path().join("git-worktrees"),
            data.path().join("jj-workspaces"),
            false,
        );
        if repos.set_vcs(VcsKind::Jujutsu).is_err() {
            return; // jj 0.43+ is optional on test hosts
        }

        let repo = repos.create("jj-example").await.unwrap();
        let root = PathBuf::from(&repo.path);
        assert!(root.join(".jj").is_dir());
        assert!(root.join(".git").is_dir(), "JJ repos are always colocated");
        std::fs::write(root.join("hello.txt"), "hello\n").unwrap();

        let branch = repos.current_branch(&root).await.unwrap();
        assert!(!branch.is_empty());
        assert!(!branch.contains('·'));

        let refs = repos.refs(&root).await.unwrap();
        let current = refs.iter().find(|row| row.current).unwrap();
        assert_eq!(current.kind, RepoRefKind::WorkingCopy);
        assert_eq!(current.revision, "@");
        assert_eq!(current.name, branch);

        // Older JJ repositories did not persist the default workspace path.
        // `workspace list` renders a diagnostic in its place; it must still map
        // to this repository root rather than becoming a fake worktree path.
        std::fs::write(root.join(".jj/repo/workspace_store/index"), []).unwrap();
        let refs = repos.refs(&root).await.unwrap();
        let current = refs.iter().find(|row| row.current).unwrap();
        assert_eq!(current.kind, RepoRefKind::WorkingCopy);
        assert_eq!(current.revision, "@");
        assert_eq!(current.name, branch);
        assert_eq!(current.worktree_path, None);

        let workspace = repos.create_worktree(&root, "@").await.unwrap();
        assert!(Path::new(&workspace.path).join(".jj").is_dir());
        assert!(!workspace.branch.contains('·'));
        let refs = repos.refs(&root).await.unwrap();
        let workspace_path = std::fs::canonicalize(&workspace.path).unwrap();
        let workspace_ref = refs
            .iter()
            .find(|row| {
                row.worktree_path
                    .as_deref()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    .as_ref()
                    == Some(&workspace_path)
            })
            .unwrap();
        assert_eq!(workspace_ref.name, workspace.branch);
        repos
            .delete_worktree(&root, Path::new(&workspace.path))
            .await
            .unwrap();
        assert!(!Path::new(&workspace.path).exists());
    }

    #[tokio::test]
    async fn orphan_cleanup_removes_clean_jj_workspace_and_retains_revision() {
        let data = tempfile::tempdir().unwrap();
        let repos = Repos::with_roots(
            data.path(),
            "device",
            data.path().join("git-worktrees"),
            data.path().join("jj-workspaces"),
            false,
        );
        if repos.set_vcs(VcsKind::Jujutsu).is_err() {
            return;
        }
        let repo = repos.create("jj-cleanup").await.unwrap();
        let root = PathBuf::from(&repo.path);
        let workspace = repos.create_worktree(&root, "@").await.unwrap();
        let checkout_id = workspace.checkout_id.as_deref().unwrap();
        let retained = format!(
            "jolt-retained/{}-{}",
            workspace.name,
            checkout_id.get(..8).unwrap_or(checkout_id)
        );

        let report = repos
            .reconcile_managed_workspaces_at(&[], 100, Duration::ZERO)
            .await
            .unwrap();

        assert_eq!(report.removed, 1);
        assert!(!Path::new(&workspace.path).exists());
        repos
            .jj(&["log", "-r", &retained], Some(&root), true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn workspace_checkout_rejects_sibling_paths() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let sibling = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        assert_eq!(
            repos.workspace_checkout(root.path(), root.path()).await,
            std::fs::canonicalize(root.path()).ok()
        );
        assert!(
            repos
                .workspace_checkout(root.path(), sibling.path())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn concurrent_searches_of_one_checkout_do_not_cancel_each_other() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("alpha.rs"), "").unwrap();
        std::fs::write(root.path().join("beta.rs"), "").unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));

        let alpha = repos.search_files(root.path().into(), "alpha".to_string(), Vec::new());
        let beta = repos.search_files(root.path().into(), "beta".to_string(), Vec::new());
        let (alpha, beta) = tokio::join!(alpha, beta);

        assert_eq!(alpha.unwrap()[0].path, "alpha.rs");
        assert_eq!(beta.unwrap()[0].path, "beta.rs");
    }

    #[tokio::test]
    async fn git_commit_selection_leaves_other_files_uncommitted() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let repo = repos.create("git-actions").await.unwrap();
        let root = PathBuf::from(repo.path);
        repos
            .git(&["config", "user.email", "test@example.com"], Some(&root))
            .await
            .unwrap();
        repos
            .git(&["config", "user.name", "Test User"], Some(&root))
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "one\n").unwrap();
        repos
            .commit_changes(&root, None, "Initial files")
            .await
            .unwrap();

        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        std::fs::write(root.join("c.txt"), "new\n").unwrap();
        repos.git(&["add", "b.txt"], Some(&root)).await.unwrap();
        let result = repos
            .commit_changes(&root, Some(&["a.txt".into(), "c.txt".into()]), "Update a")
            .await
            .unwrap();

        assert!(result.remaining_changes);
        assert_eq!(
            repos
                .git(&["show", "HEAD:a.txt"], Some(&root))
                .await
                .unwrap(),
            "two"
        );
        assert_eq!(
            repos
                .git(&["show", "HEAD:c.txt"], Some(&root))
                .await
                .unwrap(),
            "new"
        );
        assert_eq!(
            repos
                .git(&["diff", "--cached", "--name-only"], Some(&root))
                .await
                .unwrap(),
            "b.txt",
            "Partial commit must preserve unrelated staged changes"
        );
    }

    #[tokio::test]
    async fn git_first_push_sets_upstream_without_committing_dirty_files() {
        let data = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(data.path(), "device", data.path().join("worktrees"));
        let repo = repos.create("git-push").await.unwrap();
        let root = PathBuf::from(repo.path);
        repos
            .git(&["config", "user.email", "test@example.com"], Some(&root))
            .await
            .unwrap();
        repos
            .git(&["config", "user.name", "Test User"], Some(&root))
            .await
            .unwrap();
        std::fs::write(root.join("published.txt"), "published\n").unwrap();
        let committed = repos
            .commit_changes(&root, None, "Publish this")
            .await
            .unwrap();
        std::fs::write(root.join("dirty.txt"), "not published\n").unwrap();
        let remote = data.path().join("git-push-remote.git");
        repos
            .git(&["init", "--bare", remote.to_string_lossy().as_ref()], None)
            .await
            .unwrap();
        repos
            .git(
                &["remote", "add", "origin", remote.to_string_lossy().as_ref()],
                Some(&root),
            )
            .await
            .unwrap();

        let (_, publication) = repos.publication_status(&root, "ignored").await.unwrap();
        assert!(matches!(
            publication,
            VcsPublicationState::Ready {
                target: VcsPublishTarget {
                    creates_ref: true,
                    sets_upstream: true,
                    ..
                },
                ..
            }
        ));
        let pushed = repos
            .push_completed(&root, "ignored", None, true)
            .await
            .unwrap();
        assert!(pushed.created_ref);
        assert!(pushed.set_upstream);
        assert_eq!(pushed.revision, committed.revision);
        assert!(
            repos
                .git(&["status", "--porcelain"], Some(&root))
                .await
                .unwrap()
                .contains("dirty.txt")
        );
        assert_eq!(
            repos
                .git(&["rev-parse", "--abbrev-ref", "@{upstream}"], Some(&root))
                .await
                .unwrap(),
            "origin/main"
        );
    }

    #[tokio::test]
    async fn jj_partial_commit_and_push_publish_parent_with_jolt_bookmark() {
        let data = tempfile::tempdir().unwrap();
        let repos = Repos::with_roots(
            data.path(),
            "device",
            data.path().join("git-worktrees"),
            data.path().join("jj-workspaces"),
            false,
        );
        if repos.set_vcs(VcsKind::Jujutsu).is_err() {
            return;
        }
        let repo = repos.create("jj-actions").await.unwrap();
        let root = PathBuf::from(repo.path);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "one\n").unwrap();
        let initial = repos
            .commit_changes(&root, None, "Initial files")
            .await
            .unwrap();
        repos
            .jj(
                &["bookmark", "set", "feature", "-r", "@-"],
                Some(&root),
                false,
            )
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        let committed = repos
            .commit_changes(&root, Some(&["a.txt".into()]), "Update a")
            .await
            .unwrap();
        assert!(committed.remaining_changes);
        assert_eq!(
            repos
                .jj(
                    &[
                        "log",
                        "-r",
                        "feature",
                        "--no-graph",
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                    Some(&root),
                    true,
                )
                .await
                .unwrap(),
            initial.revision,
            "Commit must not advance a user-owned bookmark"
        );

        let remote = data.path().join("remote.git");
        let initialized = std::process::Command::new("git")
            .args(["init", "--bare", remote.to_string_lossy().as_ref()])
            .status()
            .unwrap();
        assert!(initialized.success());
        repos
            .jj(
                &[
                    "git",
                    "remote",
                    "add",
                    "origin",
                    remote.to_string_lossy().as_ref(),
                ],
                Some(&root),
                false,
            )
            .await
            .unwrap();

        let (_, publication) = repos
            .publication_status(&root, "Update a safely")
            .await
            .unwrap();
        assert!(matches!(
            publication,
            VcsPublicationState::Ready {
                target: VcsPublishTarget {
                    ref ref_name,
                    creates_ref: true,
                    ..
                },
                ..
            } if ref_name == "jolt/update-a-safely"
        ));
        let pushed = repos
            .push_completed(&root, "Update a safely", None, false)
            .await
            .unwrap();
        assert_eq!(pushed.ref_name, "jolt/update-a-safely");
        assert!(pushed.created_ref);
        assert_eq!(pushed.revision, committed.revision);
        assert!(
            repos
                .jj(&["diff", "--summary"], Some(&root), true)
                .await
                .unwrap()
                .contains("b.txt"),
            "Push must leave edits in @ untouched"
        );
        assert_eq!(
            repos
                .jj(
                    &[
                        "log",
                        "-r",
                        "jolt/update-a-safely@origin",
                        "--no-graph",
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                    Some(&root),
                    true,
                )
                .await
                .unwrap(),
            committed.revision
        );
    }
}

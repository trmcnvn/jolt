//! CheckoutDiffSync — checkout-scoped working-tree diff production.
//!
//! Chats do not own working-copy state: a concrete VCS checkout does. This service
//! groups this device's chats by their canonical checkout identity (`chat.cwd` →
//! [`Repos::checkout_identity`]), computes one bounded atomic snapshot per checkout,
//! and publishes it through checkout-specific paged projections:
//!
//! - `WatchCheckoutDiffV2` sends a compact manifest for one chat checkout;
//! - `GetCheckoutDiffPage` fetches immutable SHA-256-addressed patch pages;
//! - a [`DiffSidecar`] JSON `POST {edge}/diff/{chatId}` stores the manifest and
//!   checkout-deduplicated pages for offline review;
//! - `chat.branch` upkeep: each snapshot reconciles mismatched workspace chat rows' `branch` (and
//!   `checkoutId` at reconcile time).
//!
//! Recursive `notify` watchers provide low-latency Git updates. A checkout with
//! Changes subscribers is also refreshed every five seconds; this is the primary
//! refresh path for JJ and repairs dropped native events.
//! Snapshots carry a sha256 checksum; an unchanged checksum publishes nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, mpsc, watch};

use jolt_proto::{
    Chat, CheckoutDiffBootstrap, CheckoutDiffPage, CheckoutDiffWatchFrame, DiffFileSummary, VcsKind,
};

use crate::EngineError;
use crate::diff_projection::DiffProjection;
use crate::doc_host::EdgeConfig;
use crate::pinned_diffs::PinnedDiffStore;
use crate::repos::{CheckoutIdentity, Repos};
use crate::vcs::compose_command_path;
use crate::workspace_host::WorkspaceHost;

/// Hard cap on the unified patch (plus untracked hunks) — "Partial snapshot".
pub const MAX_PATCH_BYTES: usize = 3 * 1024 * 1024;
/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: re-reconcile + re-sync every checkout.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);
/// Live JJ snapshots are intentionally pull-driven: opening the Changes pane
/// subscribes, then refreshes at this cadence until it closes.
const ACTIVE_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// Max subdirectories a checkout may have before we skip its live recursive
/// watch (one OS watch per dir; past this the watcher thread's own bookkeeping
/// costs more than instant diffs are worth). A normal source tree is well
/// under this; a node_modules/vendored tree blows past it. The repair tick
/// still covers skipped checkouts.
const MAX_WATCH_DIRS: usize = 8_000;
/// `git hash-object -t tree /dev/null` — diff base for repos with no commits yet.
const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// Latest-only diff sidecar published to each chat's session DO slot
/// (`POST /diff/{chatId}`; shape: edge/src/session-doc/sidecar.ts).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSidecar {
    pub chat_id: String,
    pub device_id: String,
    pub checkout_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub manifest: jolt_proto::CheckoutDiffManifest,
    pub pages: Vec<CheckoutDiffPage>,
    /// Epoch millis.
    pub published_at: i64,
}

/// One bounded atomic snapshot of a checkout's working tree.
#[derive(Debug, Clone)]
pub struct DiffSnapshot {
    pub vcs: VcsKind,
    pub label: Option<String>,
    pub branch: String,
    pub head_sha: Option<String>,
    pub patch: String,
    pub files: Vec<DiffFileSummary>,
    pub additions: u32,
    pub deletions: u32,
    pub truncated: bool,
    pub checksum: String,
}

/// Immutable VCS object captured before an assistant turn mutates the checkout.
/// The object includes pre-existing working-copy changes, so the finalized diff
/// contains only the net filesystem delta produced during that turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiffBaseline {
    vcs: VcsKind,
    revision: String,
}

struct CheckoutEntry {
    identity: CheckoutIdentity,
    chats: Mutex<Vec<Chat>>,
    /// Last published checksum — unchanged snapshots publish nothing.
    checksum: Mutex<Option<String>>,
    projection: Mutex<Option<Arc<DiffProjection>>>,
    edge_page_ids: Mutex<HashSet<String>>,
    published_chats: Mutex<HashSet<String>>,
    sequence: Mutex<u64>,
    diff_tx: broadcast::Sender<CheckoutDiffWatchFrame>,
    sync_lock: tokio::sync::Mutex<()>,
    edge_publish_lock: tokio::sync::Mutex<()>,
    /// Kick channel into the entry's debounce/sync task.
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the recursive fs watchers alive; dropped on entry close.
    _watchers: Vec<notify::RecommendedWatcher>,
}

struct DiffSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    edge: Option<EdgeConfig>,
    http: reqwest::Client,
    pinned: PinnedDiffStore,
    entries: Mutex<HashMap<String, Arc<CheckoutEntry>>>,
    chat_entries: Mutex<HashMap<String, Arc<CheckoutEntry>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct CheckoutDiffSync {
    inner: Arc<DiffSyncInner>,
}

impl CheckoutDiffSync {
    /// Build and start the sync loop: follows the workspace chat watch and runs the
    /// 2-minute repair tick. Requires a tokio runtime.
    pub fn start(
        repos: Repos,
        workspace: WorkspaceHost,
        device_id: &str,
        edge: Option<EdgeConfig>,
        pinned_root: PathBuf,
    ) -> Self {
        let sync = Self {
            inner: Arc::new(DiffSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                edge,
                http: reqwest::Client::new(),
                pinned: PinnedDiffStore::new(pinned_root),
                entries: Mutex::new(HashMap::new()),
                chat_entries: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(diff_sync_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_chats(),
        ));
        sync
    }

    /// Open one checkout-specific projection by chat id. Bootstrap and
    /// subscription are created while the checkout sync lock is held, so no
    /// manifest update can land between them.
    pub async fn watch_diff(
        &self,
        chat_id: &str,
    ) -> Result<
        (
            CheckoutDiffBootstrap,
            broadcast::Receiver<CheckoutDiffWatchFrame>,
        ),
        EngineError,
    > {
        let entry = self.ensure_entry_for_chat(chat_id).await?;
        // Subscribe before the first capture so a concurrent workspace
        // reconcile cannot retire this newly-created entry. Frames produced by
        // that capture are drained under the sync lock; the returned bootstrap
        // is the atomic opening state.
        let mut receiver = entry.diff_tx.subscribe();
        let _sync = entry.sync_lock.lock().await;
        if lock(&entry.projection).is_none() {
            sync_entry_locked(&self.inner, &entry).await?;
        }
        while receiver.try_recv().is_ok() {}
        let projection = lock(&entry.projection)
            .clone()
            .ok_or_else(|| EngineError::Other("diff projection unavailable".into()))?;
        let sequence = *lock(&entry.sequence);
        Ok((projection.bootstrap(sequence), receiver))
    }

    pub fn current_manifest(&self, chat_id: &str) -> Option<jolt_proto::CheckoutDiffManifest> {
        let entry = self.entry_for_chat(chat_id)?;
        lock(&entry.projection)
            .as_ref()
            .map(|projection| projection.manifest.clone())
    }

    pub fn diff_page(
        &self,
        chat_id: &str,
        catalog_revision: &str,
        page_id: &str,
    ) -> Result<Option<CheckoutDiffPage>, EngineError> {
        let entry = self
            .entry_for_chat(chat_id)
            .ok_or_else(|| EngineError::Other(format!("chat {chat_id} has no local checkout")))?;
        let projection = lock(&entry.projection).clone();
        let Some(projection) = projection else {
            return Ok(None);
        };
        if projection.manifest.catalog_revision != catalog_revision {
            if let Some(page) = self.inner.pinned.page(catalog_revision, page_id)? {
                return Ok(Some(page));
            }
            return Err(EngineError::Other("stale diff catalog revision".into()));
        }
        let page = projection.page(page_id);
        if page.is_none() {
            tracing::warn!(
                requested = %page_id,
                catalog = %catalog_revision,
                descriptors = projection.manifest.pages.len(),
                "diff page is referenced by no current payload"
            );
        }
        Ok(page)
    }

    pub async fn pin_diff(
        &self,
        chat_id: &str,
        catalog_revision: &str,
        review_id: &str,
    ) -> Result<(), EngineError> {
        let entry = self.ensure_entry_for_chat(chat_id).await?;
        let projection = lock(&entry.projection)
            .clone()
            .ok_or_else(|| EngineError::Other("diff projection unavailable".into()))?;
        if projection.manifest.catalog_revision != catalog_revision {
            return Err(EngineError::Other(
                "diff revision is no longer available".into(),
            ));
        }
        self.inner.pinned.pin(&projection, review_id).await
    }

    pub async fn release_diff(
        &self,
        catalog_revision: &str,
        review_id: &str,
    ) -> Result<(), EngineError> {
        self.inner.pinned.release(catalog_revision, review_id).await
    }

    fn entry_for_chat(&self, chat_id: &str) -> Option<Arc<CheckoutEntry>> {
        // Prefer the entry's attached rows: workspace projections can briefly
        // lag a just-created local row, while the checkout subscription is
        // already valid and must stay alive.
        let direct = lock(&self.inner.chat_entries).get(chat_id).cloned();
        if direct.is_some() {
            return direct;
        }
        let chat = self.inner.workspace.chat(chat_id).ok().flatten()?;
        let entries = lock(&self.inner.entries);
        chat.checkout_id
            .as_deref()
            .and_then(|id| entries.get(id).cloned())
            .or_else(|| {
                let cwd = Path::new(chat.cwd.as_deref()?);
                entries
                    .values()
                    .find(|entry| entry.identity.root == cwd)
                    .cloned()
            })
    }

    async fn ensure_entry_for_chat(
        &self,
        chat_id: &str,
    ) -> Result<Arc<CheckoutEntry>, EngineError> {
        if let Some(entry) = self.entry_for_chat(chat_id) {
            return Ok(entry);
        }
        let chat = self
            .inner
            .workspace
            .chat(chat_id)
            .map_err(|error| EngineError::Other(error.to_string()))?
            .ok_or_else(|| EngineError::Other(format!("chat {chat_id} not found")))?;
        if chat.device_id != self.inner.device_id {
            return Err(EngineError::Other(format!(
                "chat {chat_id} is hosted on another device"
            )));
        }
        let cwd = chat
            .cwd
            .as_deref()
            .ok_or_else(|| EngineError::Other(format!("chat {chat_id} has no checkout")))?;
        let identity = self.inner.repos.checkout_identity(Path::new(cwd)).await?;
        if let Some(entry) = lock(&self.inner.entries).get(&identity.id).cloned() {
            let mut chats = lock(&entry.chats);
            if !chats.iter().any(|candidate| candidate.id == chat.id) {
                chats.push(chat.clone());
            }
            drop(chats);
            lock(&self.inner.chat_entries).insert(chat.id.clone(), entry.clone());
            return Ok(entry);
        }
        Ok(add_entry(&self.inner, identity, vec![chat]))
    }

    /// Regroup this device's chats by checkout identity, then (re)build watchers.
    /// Public for tests (the background task calls it on every chat change).
    pub async fn reconcile_now(&self) {
        let chats = self.inner.workspace.watch_chats().borrow().clone();
        reconcile(&self.inner, chats).await;
    }

    /// Kick an immediate sync of every tracked checkout (repair-tick path).
    pub fn sync_all(&self) {
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

// ---------------------------------------------------------------------------
// Reconcile: chats ⇄ checkout entries
// ---------------------------------------------------------------------------

async fn reconcile(inner: &Arc<DiffSyncInner>, chats: Vec<Chat>) {
    // Group this device's cwd-bearing chats by canonical checkout identity.
    let mut groups: HashMap<String, (CheckoutIdentity, Vec<Chat>)> = HashMap::new();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(cwd) = chat.cwd.clone() else {
            continue;
        };
        let identity = match inner.repos.checkout_identity(Path::new(&cwd)).await {
            Ok(identity) => identity,
            Err(err) => {
                tracing::debug!(cwd = %cwd, error = %err, "diff-sync: not a checkout");
                continue;
            }
        };
        // Stamp the row's checkoutId so every device groups this chat correctly.
        if chat.checkout_id.as_deref() != Some(identity.id.as_str())
            && let Err(err) = inner.workspace.set_chat_checkout(&chat.id, &identity.id)
        {
            tracing::debug!(chat = %chat.id, error = %err, "diff-sync: checkoutId write failed");
        }
        groups
            .entry(identity.id.clone())
            .or_insert_with(|| (identity, Vec::new()))
            .1
            .push(chat);
    }

    // Retire entries only when no projection watcher holds a lease. A watcher
    // subscribes before its first capture, so a stale workspace frame cannot
    // tear down an opening stream.
    let removed: Vec<Arc<CheckoutEntry>> = {
        let mut entries = lock(&inner.entries);
        let removed: Vec<_> = entries
            .iter()
            .filter(|(id, entry)| !groups.contains_key(*id) && entry.diff_tx.receiver_count() == 0)
            .map(|(_, entry)| entry.clone())
            .collect();
        entries.retain(|_, entry| {
            !removed
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, entry))
        });
        removed
    };
    if !removed.is_empty() {
        lock(&inner.chat_entries).retain(|_, entry| {
            !removed
                .iter()
                .any(|candidate| Arc::ptr_eq(candidate, entry))
        });
    }

    // Update surviving entries; add new ones (initial sync kicked on add).
    for (checkout_id, (identity, chats)) in groups {
        let existing = lock(&inner.entries).get(&checkout_id).cloned();
        match existing {
            Some(entry) => {
                let has_new = {
                    let mut held = lock(&entry.chats);
                    let previous: HashSet<String> = held.iter().map(|c| c.id.clone()).collect();
                    let has_new = chats.iter().any(|c| !previous.contains(&c.id));
                    *held = chats;
                    has_new
                };
                for chat in lock(&entry.chats).iter() {
                    lock(&inner.chat_entries).insert(chat.id.clone(), entry.clone());
                }
                if has_new {
                    let _ = entry.kick_tx.send(()); // new chat needs a sidecar now
                }
            }
            None => {
                add_entry(inner, identity, chats);
            }
        }
    }
}

/// True if `root`'s directory tree exceeds [`MAX_WATCH_DIRS`] — the signal that
/// a live recursive watch would cost more than it's worth. Bounded BFS: stops
/// the moment the budget is blown (never walks a whole node_modules), skips
/// symlinks (a symlinked dep cycle must not send this into a spin), and treats
/// unreadable dirs as leaves. `.git` internal churn is real diff signal, so it
/// counts toward the budget rather than being skipped.
fn exceeds_watch_budget(root: &Path) -> bool {
    let mut queue = std::collections::VecDeque::from([root.to_path_buf()]);
    let mut seen = 0usize;
    while let Some(dir) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type()` on the dirent does NOT follow symlinks — a symlinked
            // directory reports as a symlink and is skipped, so cyclic deps
            // (pnpm/npm) can't blow up the walk.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                seen += 1;
                if seen > MAX_WATCH_DIRS {
                    return true;
                }
                queue.push_back(entry.path());
            }
        }
    }
    false
}

fn add_entry(
    inner: &Arc<DiffSyncInner>,
    identity: CheckoutIdentity,
    chats: Vec<Chat>,
) -> Arc<CheckoutEntry> {
    let chat_rows = chats.clone();
    let chat_ids: Vec<String> = chats.iter().map(|chat| chat.id.clone()).collect();
    let (kick_tx, kick_rx) = mpsc::unbounded_channel();
    let (diff_tx, _) = broadcast::channel(16);

    // Recursive watchers on the worktree root and (for linked worktrees) the git
    // dir — HEAD/index churn and file edits both land here. Failures are fine:
    // the initial + repair sync still keep the snapshot correct.
    let mut watchers = Vec::new();
    let mut targets: Vec<&PathBuf> = vec![&identity.root];
    if !identity.metadata_dir.starts_with(&identity.root) {
        targets.push(&identity.metadata_dir);
    }
    for target in targets {
        if inner.repos.vcs_kind() != Some(VcsKind::Git) {
            break;
        }
        // A recursive `notify` watch installs one OS watch per subdirectory and
        // has no way to prune subtrees. On a checkout carrying big dependency
        // trees (node_modules, vendored deps) that is tens of thousands of
        // watches: the watcher thread pegs a core just maintaining them — even
        // with the tree completely idle — which starved a real device's whole
        // async runtime (presence heartbeats and IPC stalled; it showed
        // permanently offline). If the tree blows the budget, skip the live
        // watch entirely; the 2-minute repair tick still keeps the diff
        // correct, just not instantly. Bounded so the probe itself stays cheap
        // on a pathological tree.
        if exceeds_watch_budget(target) {
            tracing::info!(path = %target.display(),
                "diff-sync: tree too large to watch live; relying on the repair tick");
            continue;
        }
        let tx = kick_tx.clone();
        let watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                if event.is_ok() {
                    let _ = tx.send(());
                }
            });
        match watcher {
            Ok(mut watcher) => {
                use notify::Watcher as _;
                match watcher.watch(target, notify::RecursiveMode::Recursive) {
                    Ok(()) => watchers.push(watcher),
                    Err(err) => {
                        tracing::debug!(path = %target.display(), error = %err, "diff-sync: watch failed")
                    }
                }
            }
            Err(err) => tracing::debug!(error = %err, "diff-sync: watcher create failed"),
        }
    }

    let entry = Arc::new(CheckoutEntry {
        identity,
        chats: Mutex::new(chats),
        checksum: Mutex::new(None),
        projection: Mutex::new(None),
        edge_page_ids: Mutex::new(HashSet::new()),
        published_chats: Mutex::new(HashSet::new()),
        sequence: Mutex::new(0),
        diff_tx,
        sync_lock: tokio::sync::Mutex::new(()),
        edge_publish_lock: tokio::sync::Mutex::new(()),
        kick_tx: kick_tx.clone(),
        _watchers: watchers,
    });
    let selected = {
        let mut entries = lock(&inner.entries);
        entries
            .entry(entry.identity.id.clone())
            .or_insert_with(|| entry.clone())
            .clone()
    };
    if !Arc::ptr_eq(&selected, &entry) {
        let mut attached = lock(&selected.chats);
        for chat in chat_rows {
            if !attached.iter().any(|candidate| candidate.id == chat.id) {
                attached.push(chat);
            }
        }
        drop(attached);
        for chat_id in chat_ids {
            lock(&inner.chat_entries).insert(chat_id, selected.clone());
        }
        return selected;
    }
    for chat_id in chat_ids {
        lock(&inner.chat_entries).insert(chat_id, entry.clone());
    }
    tokio::spawn(entry_task(
        Arc::downgrade(inner),
        Arc::downgrade(&entry),
        kick_rx,
    ));
    let _ = kick_tx.send(());
    entry
}

/// Per-checkout task: trailing-debounce fs kicks, then compute + publish. Runs
/// syncs sequentially — kicks during a sync accumulate and trigger another pass.
async fn entry_task(
    inner: Weak<DiffSyncInner>,
    entry: Weak<CheckoutEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        // Trailing debounce: wait for the burst to settle.
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        if entry.diff_tx.receiver_count() == 0 && inner.edge.is_none() {
            continue;
        }
        sync_entry(&inner, &entry).await;
    }
}

// ---------------------------------------------------------------------------
// Snapshot + publish
// ---------------------------------------------------------------------------

async fn sync_entry(inner: &Arc<DiffSyncInner>, entry: &Arc<CheckoutEntry>) {
    let _sync = entry.sync_lock.lock().await;
    if let Err(err) = sync_entry_locked(inner, entry).await {
        tracing::debug!(checkout = %entry.identity.root.display(), error = %err,
            "diff-sync: capture failed");
    }
}

async fn sync_entry_locked(
    inner: &Arc<DiffSyncInner>,
    entry: &Arc<CheckoutEntry>,
) -> Result<(), EngineError> {
    let snapshot = capture_diff(&inner.repos, &entry.identity.root).await?;
    let chats = lock(&entry.chats).clone();
    for chat in &chats {
        if chat.branch.as_deref() != Some(snapshot.branch.as_str())
            && let Err(err) = inner.workspace.set_chat_branch(&chat.id, &snapshot.branch)
        {
            tracing::debug!(chat = %chat.id, error = %err, "diff-sync: branch write failed");
        }
    }

    let changed = lock(&entry.checksum).as_deref() != Some(snapshot.checksum.as_str())
        || lock(&entry.projection).is_none();
    let needs_sidecar = inner.edge.is_some()
        && chats
            .iter()
            .any(|chat| !lock(&entry.published_chats).contains(&chat.id));
    if !changed && !needs_sidecar {
        return Ok(());
    }
    let updated_at = chrono::Utc::now();
    let projection = if changed {
        let projection = Arc::new(DiffProjection::build(
            &entry.identity.id,
            &inner.device_id,
            &entry.identity.root.to_string_lossy(),
            &snapshot,
            updated_at,
        ));
        *lock(&entry.checksum) = Some(snapshot.checksum.clone());
        *lock(&entry.projection) = Some(projection.clone());
        let sequence = {
            let mut sequence = lock(&entry.sequence);
            *sequence = sequence.wrapping_add(1);
            *sequence
        };
        let _ = entry.diff_tx.send(CheckoutDiffWatchFrame::Manifest {
            sequence,
            manifest: projection.manifest.clone(),
        });
        projection
    } else {
        lock(&entry.projection)
            .clone()
            .ok_or_else(|| EngineError::Other("diff projection unavailable".into()))?
    };

    if inner.edge.is_some() {
        let inner = Arc::clone(inner);
        let entry = Arc::clone(entry);
        let sequence = *lock(&entry.sequence);
        tokio::spawn(async move {
            publish_edge_sidecars(
                inner, entry, snapshot, projection, chats, changed, sequence, updated_at,
            )
            .await;
        });
    }
    Ok(())
}

/// Upload immutable pages and chat manifests away from the local projection
/// lock. Opening the Changes pane must never wait for authentication, network
/// latency, or R2 writes. Jobs serialize per checkout and stale jobs are
/// discarded before upload so a late task cannot replace a newer manifest.
async fn publish_edge_sidecars(
    inner: Arc<DiffSyncInner>,
    entry: Arc<CheckoutEntry>,
    snapshot: DiffSnapshot,
    projection: Arc<DiffProjection>,
    chats: Vec<Chat>,
    changed: bool,
    sequence: u64,
    updated_at: chrono::DateTime<chrono::Utc>,
) {
    let _publish = entry.edge_publish_lock.lock().await;
    if *lock(&entry.sequence) != sequence {
        return;
    }
    let Some(edge) = &inner.edge else {
        return;
    };
    let current_page_ids: HashSet<_> = projection
        .manifest
        .pages
        .iter()
        .map(|page| page.id.clone())
        .collect();
    let known_page_ids = lock(&entry.edge_page_ids).clone();
    let pages: Vec<_> = projection
        .pages()
        .filter(|page| !known_page_ids.contains(&page.id))
        .cloned()
        .collect();
    let mut pages_uploaded = pages.is_empty();
    for chat in &chats {
        if !changed && lock(&entry.published_chats).contains(&chat.id) {
            continue;
        }
        let sidecar = DiffSidecar {
            chat_id: chat.id.clone(),
            device_id: inner.device_id.clone(),
            checkout_path: entry.identity.root.to_string_lossy().to_string(),
            branch: Some(snapshot.branch.clone()),
            head_sha: snapshot.head_sha.clone(),
            manifest: projection.manifest.clone(),
            pages: if pages_uploaded {
                Vec::new()
            } else {
                pages.clone()
            },
            published_at: updated_at.timestamp_millis(),
        };
        let url = format!("{}/diff/{}", edge.url.trim_end_matches('/'), chat.id);
        let Some(bearer) = edge.bearer().await else {
            tracing::debug!(chat = %chat.id, "diff-sync: sidecar skipped (signed out)");
            continue;
        };
        let result = inner
            .http
            .post(&url)
            .bearer_auth(&bearer)
            .json(&sidecar)
            .timeout(Duration::from_secs(30))
            .send()
            .await;
        match result {
            Ok(response) if !response.status().is_success() => {
                tracing::debug!(chat = %chat.id, status = %response.status(),
                    "diff-sync: sidecar publish rejected");
            }
            Err(err) => {
                tracing::debug!(chat = %chat.id, error = %err, "diff-sync: sidecar publish failed");
            }
            Ok(_) => {
                pages_uploaded = true;
                *lock(&entry.edge_page_ids) = current_page_ids.clone();
                lock(&entry.published_chats).insert(chat.id.clone());
            }
        }
    }
}

/// Chat-watch follower + active-pane refresh tick. Holds only weak handles so dropping the
/// service tears the loop down.
async fn diff_sync_task(inner: Weak<DiffSyncInner>, mut chats_rx: watch::Receiver<Vec<Chat>>) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await;
    let mut active_refresh = tokio::time::interval(ACTIVE_REFRESH_INTERVAL);
    active_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    active_refresh.tick().await;
    loop {
        tokio::select! {
            changed = chats_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow_and_update().clone();
                reconcile(&inner, chats).await;
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let chats = chats_rx.borrow().clone();
                reconcile(&inner, chats).await;
                for entry in lock(&inner.entries).values() {
                    if entry.diff_tx.receiver_count() > 0 || inner.edge.is_some() {
                        let _ = entry.kick_tx.send(());
                    }
                }
            }
            _ = active_refresh.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                for entry in lock(&inner.entries).values() {
                    if entry.diff_tx.receiver_count() > 0 {
                        let _ = entry.kick_tx.send(());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Diff capture (exposed for tests)
// ---------------------------------------------------------------------------

struct Capture {
    stdout: Vec<u8>,
    truncated: bool,
}

/// Run the active VCS capturing stdout under a hard byte ceiling — the child is killed once
/// the cap is hit, so an arbitrarily large repository diff never buffers fully.
async fn capture_command(
    repos: &Repos,
    cwd: &Path,
    args: &[&str],
    max_bytes: usize,
) -> Result<Capture, EngineError> {
    capture_command_with_index(repos, cwd, args, max_bytes, None).await
}

async fn capture_command_with_index(
    repos: &Repos,
    cwd: &Path,
    args: &[&str],
    max_bytes: usize,
    git_index: Option<&Path>,
) -> Result<Capture, EngineError> {
    let backend = repos.vcs_command()?;
    let mut cmd = tokio::process::Command::new(&backend.executable);
    if backend.kind == VcsKind::Git {
        cmd.arg("-C").arg(cwd);
    } else {
        cmd.current_dir(cwd);
    }
    cmd.args(args);
    if let Some(index) = git_index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    compose_command_path(&mut cmd, &backend.executable);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| EngineError::Other(format!("{} spawn failed: {e}", backend.kind.label())))?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        EngineError::Other(format!("{} stdout unavailable", backend.kind.label()))
    })?;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let n = stdout.read(&mut buf).await.map_err(|e| {
            EngineError::Other(format!("{} read failed: {e}", backend.kind.label()))
        })?;
        if n == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(out.len());
        if n > remaining {
            out.extend_from_slice(&buf[..remaining]);
            truncated = true;
            let _ = child.start_kill();
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| EngineError::Other(format!("{} wait failed: {e}", backend.kind.label())))?;
    if !output.status.success() && !truncated {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(EngineError::Other(if message.is_empty() {
            format!("{} exited {}", backend.kind.label(), output.status)
        } else {
            format!("{}: {message}", backend.kind.label())
        }));
    }
    Ok(Capture {
        stdout: out,
        truncated,
    })
}

fn split_z(value: &[u8]) -> Vec<String> {
    value
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

fn parse_name_status(value: &[u8]) -> Vec<DiffFileSummary> {
    let fields = split_z(value);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < fields.len() {
        let raw = fields[i].clone();
        i += 1;
        let code = raw.chars().next().unwrap_or('M');
        let Some(first) = fields.get(i).cloned() else {
            break;
        };
        i += 1;
        let renamed = code == 'R' || code == 'C';
        let second = if renamed {
            let s = fields.get(i).cloned();
            i += 1;
            s
        } else {
            None
        };
        let status = match code {
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'U' => "unmerged",
            _ => "modified",
        };
        out.push(DiffFileSummary {
            path: second.clone().unwrap_or_else(|| first.clone()),
            old_path: second.is_some().then_some(first),
            status: status.to_string(),
            additions: 0,
            deletions: 0,
            binary: false,
        });
    }
    out
}

fn apply_numstat(files: &mut [DiffFileSummary], value: &[u8]) {
    // With -z, a rename record is `adds<TAB>dels<TAB><NUL>old<NUL>new<NUL>`.
    let records: Vec<String> = value
        .split(|b| *b == 0)
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect();
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        if record.is_empty() {
            i += 1;
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let adds = parts.next().unwrap_or_default().to_string();
        let dels = parts.next().unwrap_or_default().to_string();
        let inline_path = parts.next().unwrap_or_default().to_string();
        let path = if inline_path.is_empty() {
            // Rename: the next two records are old, new.
            let new_path = records.get(i + 2).cloned().unwrap_or_default();
            i += 2;
            new_path
        } else {
            inline_path
        };
        i += 1;
        if let Some(file) = files.iter_mut().find(|f| f.path == path) {
            file.additions = adds.parse().unwrap_or(0);
            file.deletions = dels.parse().unwrap_or(0);
            file.binary = adds == "-" || dels == "-";
        }
    }
}

fn quote_patch_path(path: &str) -> String {
    if path
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        serde_json::to_string(path).unwrap_or_else(|_| format!("\"{path}\""))
    } else {
        path.to_string()
    }
}

/// Synthesize a new-file hunk for an untracked file (git diff never shows them).
fn untracked_patch(path: &str, content: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let body: String = lines
        .iter()
        .map(|line| format!("+{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let a = quote_patch_path(&format!("a/{path}"));
    let b = quote_patch_path(&format!("b/{path}"));
    format!(
        "diff --git {a} {b}\nnew file mode 100644\n--- /dev/null\n+++ {b}\n@@ -0,0 +1,{} @@\n{body}\n",
        lines.len()
    )
}

struct TemporaryGitIndex(PathBuf);

impl TemporaryGitIndex {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("jolt-turn-diff-{}.index", uuid::Uuid::new_v4())))
    }
}

impl Drop for TemporaryGitIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Capture the complete non-ignored working tree as an immutable VCS object.
/// Git uses an isolated temporary index, leaving the user's real index intact;
/// JJ snapshots the working copy into its current commit.
pub async fn capture_turn_diff_baseline(
    repos: &Repos,
    root: &Path,
) -> Result<TurnDiffBaseline, EngineError> {
    match repos.vcs_kind() {
        Some(VcsKind::Git) => Ok(TurnDiffBaseline {
            vcs: VcsKind::Git,
            revision: capture_git_worktree_tree(repos, root).await?,
        }),
        Some(VcsKind::Jujutsu) => {
            let revision = capture_command(
                repos,
                root,
                &[
                    "--no-pager",
                    "--color=never",
                    "log",
                    "-r",
                    "@",
                    "--no-graph",
                    "-T",
                    "commit_id ++ \"\\n\"",
                ],
                512,
            )
            .await?;
            let revision = String::from_utf8_lossy(&revision.stdout).trim().to_string();
            if revision.is_empty() {
                return Err(EngineError::Other(
                    "Jujutsu working-copy commit is unavailable".into(),
                ));
            }
            Ok(TurnDiffBaseline {
                vcs: VcsKind::Jujutsu,
                revision,
            })
        }
        None => Err(EngineError::Other(
            "No supported VCS executable found".into(),
        )),
    }
}

/// Finalize the net filesystem diff from a turn baseline to the current tree.
pub async fn capture_turn_diff(
    repos: &Repos,
    root: &Path,
    baseline: &TurnDiffBaseline,
) -> Result<DiffSnapshot, EngineError> {
    capture_scoped_turn_diff(repos, root, baseline, &[]).await
}

/// Capture a turn diff restricted to paths explicitly mutated by that turn.
/// Paths outside the checkout are discarded before invoking the VCS. An empty
/// path list preserves checkout-wide behavior for callers that cannot provide
/// reliable mutation paths.
pub(crate) async fn capture_scoped_turn_diff(
    repos: &Repos,
    root: &Path,
    baseline: &TurnDiffBaseline,
    paths: &[String],
) -> Result<DiffSnapshot, EngineError> {
    if repos.vcs_kind() != Some(baseline.vcs) {
        return Err(EngineError::Other(
            "checkout VCS changed while capturing turn diff".into(),
        ));
    }
    let scoped_paths;
    let paths = if paths.is_empty() {
        paths
    } else {
        let checkout = repos.checkout_identity(root).await?;
        scoped_paths = checkout_scoped_paths(root, &checkout.root, paths);
        if scoped_paths.is_empty() {
            return Ok(turn_snapshot(
                baseline.vcs,
                Some(baseline.revision.clone()),
                String::new(),
                Vec::new(),
                false,
            ));
        }
        &scoped_paths
    };
    match baseline.vcs {
        VcsKind::Git => capture_git_turn_diff(repos, root, &baseline.revision, paths).await,
        VcsKind::Jujutsu => capture_jj_turn_diff(repos, root, &baseline.revision, paths).await,
    }
}

fn checkout_scoped_paths(cwd: &Path, checkout_root: &Path, paths: &[String]) -> Vec<String> {
    let Some(cwd) = canonicalize_with_missing_tail(cwd) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    paths
        .iter()
        .filter_map(|path| {
            let path = Path::new(path);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            let path = normalize_absolute_path(&path)?;
            let path = canonicalize_with_missing_tail(&path)?;
            (path != checkout_root && path.starts_with(checkout_root))
                .then(|| path.to_str().map(str::to_owned))?
        })
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
        }
    }
    Some(normalized)
}

fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(ancestor) {
            for component in tail.iter().rev() {
                resolved.push(component);
            }
            return Some(resolved);
        }
        tail.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
}

async fn capture_git_worktree_tree(repos: &Repos, root: &Path) -> Result<String, EngineError> {
    let index = TemporaryGitIndex::new();
    capture_command_with_index(repos, root, &["read-tree", "--empty"], 256, Some(&index.0)).await?;
    capture_command_with_index(repos, root, &["add", "-A", "--", "."], 256, Some(&index.0)).await?;
    let tree =
        capture_command_with_index(repos, root, &["write-tree"], 256, Some(&index.0)).await?;
    let tree = String::from_utf8_lossy(&tree.stdout).trim().to_string();
    if tree.is_empty() {
        return Err(EngineError::Other(
            "Git working-tree snapshot is unavailable".into(),
        ));
    }
    Ok(tree)
}

fn scoped_diff_args<'a>(mut args: Vec<&'a str>, paths: &'a [String]) -> Vec<&'a str> {
    args.extend(paths.iter().map(String::as_str));
    args
}

async fn capture_git_turn_diff(
    repos: &Repos,
    root: &Path,
    baseline: &str,
    paths: &[String],
) -> Result<DiffSnapshot, EngineError> {
    let target = capture_git_worktree_tree(repos, root).await?;
    let names_args = scoped_diff_args(
        vec![
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            baseline,
            &target,
            "--",
        ],
        paths,
    );
    let names = capture_command(repos, root, &names_args, 2 * 1024 * 1024).await?;
    let nums_args = scoped_diff_args(
        vec![
            "diff",
            "--numstat",
            "-z",
            "--find-renames",
            baseline,
            &target,
            "--",
        ],
        paths,
    );
    let nums = capture_command(repos, root, &nums_args, 2 * 1024 * 1024).await?;
    let patch_args = scoped_diff_args(
        vec![
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            baseline,
            &target,
            "--",
        ],
        paths,
    );
    let captured = capture_command(repos, root, &patch_args, MAX_PATCH_BYTES).await?;
    let mut patch = String::from_utf8_lossy(&captured.stdout).to_string();
    let truncated = captured.truncated || names.truncated || nums.truncated;
    if captured.truncated {
        patch.truncate(patch.rfind('\n').unwrap_or(0));
        patch.push_str("\n# Jolt diff truncated\n");
    }
    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    Ok(turn_snapshot(
        VcsKind::Git,
        Some(target),
        patch,
        files,
        truncated,
    ))
}

async fn capture_jj_turn_diff(
    repos: &Repos,
    root: &Path,
    baseline: &str,
    paths: &[String],
) -> Result<DiffSnapshot, EngineError> {
    // This first command snapshots the current working copy before either diff
    // command reads it.
    let target = capture_command(
        repos,
        root,
        &[
            "--no-pager",
            "--color=never",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "commit_id ++ \"\\n\"",
        ],
        512,
    )
    .await?;
    let target = String::from_utf8_lossy(&target.stdout).trim().to_string();
    let listed_args = scoped_diff_args(
        vec![
            "--no-pager",
            "--color=never",
            "--ignore-working-copy",
            "diff",
            "--from",
            baseline,
            "--to",
            &target,
            "-T",
            "status ++ \"\\t\" ++ source.path() ++ \"\\t\" ++ target.path() ++ \"\\n\"",
            "--",
        ],
        paths,
    );
    let listed = capture_command(repos, root, &listed_args, 2 * 1024 * 1024).await?;
    let patch_args = scoped_diff_args(
        vec![
            "--no-pager",
            "--color=never",
            "--ignore-working-copy",
            "diff",
            "--from",
            baseline,
            "--to",
            &target,
            "--git",
            "--context",
            "3",
            "--",
        ],
        paths,
    );
    let captured = capture_command(repos, root, &patch_args, MAX_PATCH_BYTES).await?;
    let mut patch = String::from_utf8_lossy(&captured.stdout).to_string();
    let truncated = captured.truncated || listed.truncated;
    if captured.truncated {
        patch.truncate(patch.rfind('\n').unwrap_or(0));
        patch.push_str("\n# Jolt diff truncated\n");
    }
    let mut files = parse_jj_files(&listed.stdout);
    apply_patch_stats_by_order(&mut files, &patch);
    Ok(turn_snapshot(
        VcsKind::Jujutsu,
        Some(target),
        patch,
        files,
        truncated,
    ))
}

fn turn_snapshot(
    vcs: VcsKind,
    revision: Option<String>,
    patch: String,
    files: Vec<DiffFileSummary>,
    truncated: bool,
) -> DiffSnapshot {
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    let mut hasher = Sha256::new();
    hasher.update([vcs as u8]);
    hasher.update([0]);
    hasher.update(revision.as_deref().unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(patch.as_bytes());
    hasher.update([0]);
    hasher.update(serde_json::to_vec(&files).unwrap_or_default());
    hasher.update(if truncated { b"1" } else { b"0" });
    DiffSnapshot {
        vcs,
        label: Some("Turn changes".into()),
        branch: String::new(),
        head_sha: revision,
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum: crate::repos::hex(&hasher.finalize()),
    }
}

/// One bounded atomic snapshot: tracked diff vs HEAD (or the empty tree) with
/// renames, plus untracked files (via `git status --porcelain`, index untouched)
/// as synthesized new-file hunks. 3MiB patch cap with a `truncated` flag; sha256
/// checksum over branch ‖ head ‖ patch ‖ files ‖ truncated.
pub async fn capture_diff(repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    match repos.vcs_kind() {
        Some(VcsKind::Git) => capture_git_diff(repos, root).await,
        Some(VcsKind::Jujutsu) => capture_jj_diff(repos, root).await,
        None => Err(EngineError::Other(
            "No supported VCS executable found".into(),
        )),
    }
}

async fn capture_git_diff(repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    let head = capture_command(repos, root, &["rev-parse", "--verify", "HEAD"], 256)
        .await
        .map(|c| String::from_utf8_lossy(&c.stdout).trim().to_string())
        .unwrap_or_default();
    let base: &str = if head.is_empty() {
        EMPTY_TREE_SHA
    } else {
        &head
    };
    let branch = repos
        .current_branch(root)
        .await
        .unwrap_or_else(|_| "HEAD".into());

    let names = capture_command(
        repos,
        root,
        &["diff", "--name-status", "-z", "--find-renames", base, "--"],
        2 * 1024 * 1024,
    )
    .await?;
    let nums = capture_command(
        repos,
        root,
        &["diff", "--numstat", "-z", "--find-renames", base, "--"],
        2 * 1024 * 1024,
    )
    .await?;
    let tracked = capture_command(
        repos,
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--unified=3",
            base,
            "--",
        ],
        MAX_PATCH_BYTES,
    )
    .await?;
    // Untracked listing via porcelain status; `--no-optional-locks` keeps this
    // read-only (a status-triggered index refresh would re-kick our own watcher).
    let status = capture_command(
        repos,
        root,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
        ],
        2 * 1024 * 1024,
    )
    .await?;

    let mut files = parse_name_status(&names.stdout);
    apply_numstat(&mut files, &nums.stdout);
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let mut truncated = tracked.truncated || names.truncated || nums.truncated || status.truncated;

    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Jolt diff truncated\n");
    }

    // `?? path` records; rename records (`R  new\0old`) consume their extra field.
    let mut untracked: Vec<String> = Vec::new();
    let records = split_z(&status.stdout);
    let mut i = 0usize;
    while i < records.len() {
        let record = &records[i];
        i += 1;
        if record.len() < 3 {
            continue;
        }
        let (code, path) = record.split_at(2);
        if code.starts_with('R') || code.starts_with('C') {
            i += 1; // skip the origin-path field
        }
        if code == "??" {
            untracked.push(path.trim_start().to_string());
        }
    }
    untracked.sort();

    for path in untracked {
        let full = root.join(&path);
        let binary;
        let mut additions = 0u32;
        let size = tokio::fs::metadata(&full)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if size > MAX_PATCH_BYTES as u64 {
            binary = true;
            truncated = true;
        } else {
            match tokio::fs::read(&full).await {
                Ok(bytes) => {
                    binary = bytes.contains(&0);
                    if !binary {
                        let text = String::from_utf8_lossy(&bytes).to_string();
                        additions = if text.is_empty() {
                            0
                        } else {
                            (text.split('\n').count() - usize::from(text.ends_with('\n'))) as u32
                        };
                        let addition = untracked_patch(&path, &text);
                        if patch.len() + addition.len() <= MAX_PATCH_BYTES {
                            if !patch.is_empty() && !patch.ends_with('\n') {
                                patch.push('\n');
                            }
                            patch.push_str(&addition);
                        } else {
                            truncated = true;
                        }
                    }
                }
                Err(_) => continue, // vanished between status and read
            }
        }
        files.push(DiffFileSummary {
            path,
            old_path: None,
            status: "added".to_string(),
            additions,
            deletions: 0,
            binary,
        });
    }

    let additions: u32 = files.iter().map(|f| f.additions).sum();
    let deletions: u32 = files.iter().map(|f| f.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|e| EngineError::Other(format!("diff files serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(branch.as_bytes());
    hasher.update([0u8]);
    hasher.update(head.as_bytes());
    hasher.update([0u8]);
    hasher.update(patch.as_bytes());
    hasher.update([0u8]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });
    let checksum = crate::repos::hex(&hasher.finalize());

    Ok(DiffSnapshot {
        vcs: VcsKind::Git,
        label: None,
        branch,
        head_sha: (!head.is_empty()).then_some(head),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum,
    })
}

fn parse_jj_files(value: &[u8]) -> Vec<DiffFileSummary> {
    String::from_utf8_lossy(value)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let (Some(status), Some(source), Some(target)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return None;
            };
            let status = match status {
                "added" => "added",
                "removed" => "deleted",
                "renamed" => "renamed",
                "copied" => "copied",
                _ => "modified",
            };
            Some(DiffFileSummary {
                path: target.to_string(),
                old_path: (source != target).then(|| source.to_string()),
                status: status.into(),
                additions: 0,
                deletions: 0,
                binary: false,
            })
        })
        .collect()
}

fn apply_patch_stats_by_order(files: &mut [DiffFileSummary], patch: &str) {
    let mut index = None;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            let next = index.map_or(0, |index: usize| index + 1);
            index = (next < files.len()).then_some(next);
            continue;
        }
        let Some(file) = index.and_then(|index| files.get_mut(index)) else {
            continue;
        };
        if line.starts_with("Binary files ") || line == "GIT binary patch" {
            file.binary = true;
        } else if line.starts_with('+') && !line.starts_with("+++") {
            file.additions = file.additions.saturating_add(1);
        } else if line.starts_with('-') && !line.starts_with("---") {
            file.deletions = file.deletions.saturating_add(1);
        }
    }
}

async fn capture_jj_diff(repos: &Repos, root: &Path) -> Result<DiffSnapshot, EngineError> {
    let tracked = capture_command(
        repos,
        root,
        &[
            "--no-pager",
            "--color=never",
            "diff",
            "--git",
            "--context",
            "3",
        ],
        MAX_PATCH_BYTES,
    )
    .await?;
    let listed = capture_command(
        repos,
        root,
        &[
            "--no-pager",
            "--color=never",
            "--ignore-working-copy",
            "diff",
            "-T",
            "status ++ \"\\t\" ++ source.path() ++ \"\\t\" ++ target.path() ++ \"\\n\"",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let identity = capture_command(
        repos,
        root,
        &[
            "--no-pager",
            "--color=never",
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            "change_id.shortest(8) ++ \"\\t\" ++ commit_id ++ \"\\n\"",
        ],
        512,
    )
    .await?;

    let identity = String::from_utf8_lossy(&identity.stdout);
    let (change_id, commit_id) = identity.trim().split_once('\t').unwrap_or(("@", ""));
    let label = format!("Working copy · {change_id}");
    let mut patch = String::from_utf8_lossy(&tracked.stdout).to_string();
    let truncated = tracked.truncated || listed.truncated;
    if tracked.truncated {
        let boundary = patch.rfind('\n').unwrap_or(0);
        patch.truncate(boundary);
        patch.push_str("\n# Jolt diff truncated\n");
    }
    let mut files = parse_jj_files(&listed.stdout);
    apply_patch_stats_by_order(&mut files, &patch);
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    let files_json = serde_json::to_string(&files)
        .map_err(|err| EngineError::Other(format!("diff files serialize: {err}")))?;
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(commit_id.as_bytes());
    hasher.update([0]);
    hasher.update(patch.as_bytes());
    hasher.update([0]);
    hasher.update(files_json.as_bytes());
    hasher.update(if truncated { b"1" } else { b"0" });

    Ok(DiffSnapshot {
        vcs: VcsKind::Jujutsu,
        label: Some(label.clone()),
        branch: label,
        head_sha: (!commit_id.is_empty()).then(|| commit_id.to_string()),
        patch,
        files,
        additions,
        deletions,
        truncated,
        checksum: crate::repos::hex(&hasher.finalize()),
    })
}

#[cfg(test)]
mod turn_diff_tests {
    use std::collections::HashSet;
    use std::process::Command;

    use super::{capture_scoped_turn_diff, capture_turn_diff, capture_turn_diff_baseline};
    use crate::repos::Repos;

    fn git(root: &std::path::Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[tokio::test]
    async fn turn_diff_excludes_preexisting_changes_and_preserves_the_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "jolt@example.invalid"]);
        git(&root, &["config", "user.name", "Jolt Test"]);
        std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);

        std::fs::write(root.join("tracked.txt"), "dirty before\n").unwrap();
        std::fs::write(root.join("untouched-before.txt"), "existing\n").unwrap();
        std::fs::write(root.join("staged.txt"), "staged before\n").unwrap();
        git(&root, &["add", "staged.txt"]);
        let status_before = git(&root, &["status", "--porcelain"]);

        let repos =
            Repos::with_worktrees_root(temp.path(), "device", temp.path().join("worktrees"));
        let baseline = capture_turn_diff_baseline(&repos, &root).await.unwrap();
        assert_eq!(git(&root, &["status", "--porcelain"]), status_before);

        std::fs::write(root.join("tracked.txt"), "dirty after\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let snapshot = capture_turn_diff(&repos, &root, &baseline).await.unwrap();
        let paths: HashSet<_> = snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();

        assert_eq!(paths, HashSet::from(["tracked.txt", "new.txt"]));
        assert!(!snapshot.patch.contains("untouched-before.txt"));
        assert!(!snapshot.patch.contains("staged.txt"));
    }

    #[tokio::test]
    async fn scoped_turn_diff_excludes_concurrent_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "jolt@example.invalid"]);
        git(&root, &["config", "user.name", "Jolt Test"]);
        std::fs::write(root.join("session.txt"), "before\n").unwrap();
        std::fs::write(root.join("other.txt"), "before\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "initial"]);

        let repos =
            Repos::with_worktrees_root(temp.path(), "device", temp.path().join("worktrees"));
        let baseline = capture_turn_diff_baseline(&repos, &root).await.unwrap();
        std::fs::write(root.join("session.txt"), "session change\n").unwrap();
        std::fs::write(root.join("other.txt"), "concurrent change\n").unwrap();
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "outside change\n").unwrap();

        let snapshot = capture_scoped_turn_diff(
            &repos,
            &root,
            &baseline,
            &[
                "session.txt".to_string(),
                outside.to_string_lossy().into_owned(),
            ],
        )
        .await
        .unwrap();

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "session.txt");
        assert!(snapshot.patch.contains("session change"));
        assert!(!snapshot.patch.contains("other.txt"));

        let outside_only = capture_scoped_turn_diff(
            &repos,
            &root,
            &baseline,
            &[outside.to_string_lossy().into_owned()],
        )
        .await
        .unwrap();
        assert!(outside_only.files.is_empty());
        assert!(outside_only.patch.is_empty());
    }

    #[tokio::test]
    async fn jujutsu_scoped_turn_diff_ignores_paths_outside_checkout() {
        let temp = tempfile::tempdir().unwrap();
        let repos =
            Repos::with_worktrees_root(temp.path(), "device", temp.path().join("worktrees"));
        if repos.set_vcs(jolt_proto::VcsKind::Jujutsu).is_err() {
            return; // jj 0.43+ is optional on test hosts
        }
        let repo = repos.create("jj-scoped-turn-diff").await.unwrap();
        let root = std::path::PathBuf::from(repo.path);
        std::fs::write(root.join("session.txt"), "before\n").unwrap();
        let baseline = capture_turn_diff_baseline(&repos, &root).await.unwrap();
        std::fs::write(root.join("session.txt"), "after\n").unwrap();
        let outside = temp.path().join("JoltExportOptions.plist");
        std::fs::write(&outside, "outside\n").unwrap();

        let snapshot = capture_scoped_turn_diff(
            &repos,
            &root,
            &baseline,
            &[
                "session.txt".to_string(),
                outside.to_string_lossy().into_owned(),
            ],
        )
        .await
        .unwrap();

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].path, "session.txt");
    }
}

#[cfg(test)]
mod watch_budget_tests {
    use super::{MAX_WATCH_DIRS, exceeds_watch_budget};

    #[test]
    fn small_tree_is_watchable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/a/b")).unwrap();
        std::fs::create_dir_all(root.join("src/c")).unwrap();
        std::fs::write(root.join("src/a/f.txt"), "x").unwrap();
        assert!(!exceeds_watch_budget(root));
    }

    #[test]
    fn budget_is_exceeded_and_probe_stays_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // One flat directory of MAX_WATCH_DIRS + 50 subdirs trips the budget;
        // the BFS must stop right after the threshold, not enumerate the rest.
        for i in 0..(MAX_WATCH_DIRS + 50) {
            std::fs::create_dir(root.join(format!("d{i}"))).unwrap();
        }
        assert!(exceeds_watch_budget(root));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        // A self-referential symlink cycle must not send the walk into a spin.
        std::os::unix::fs::symlink(root.join("real"), root.join("real/inner/loop")).unwrap();
        assert!(!exceeds_watch_budget(root)); // terminates, under budget
    }
}

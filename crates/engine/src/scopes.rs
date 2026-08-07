//! Device-local data scopes and the one-time account-layout migration.
//!
//! Local data always lives in `scopes/local/current`. Account data lives in
//! `scopes/accounts/<org>/<user>`. The old `orgs/<org>/<user>` directory is
//! moved, never adopted in place, so every runtime has one canonical scope
//! root after migration.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension as _;
use serde::{Deserialize, Serialize};

use jolt_doc::{REGISTRY_DOC_ID, RegistryDoc, SessionDoc};

use crate::{EngineError, new_id};

const SCOPES_DIR: &str = "scopes";
const LOCAL_SCOPE_ID: &str = "local-scope-id";
const MIGRATION_MARKER: &str = "scope-layout-v1.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScopeKind {
    Local,
    Account,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeStatus {
    pub active: ScopeKind,
    pub account_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    pub local_has_data: bool,
    pub merge_pending: bool,
}

impl ScopeStatus {
    pub fn local() -> Self {
        Self {
            active: ScopeKind::Local,
            account_available: false,
            account_email: None,
            local_has_data: false,
            merge_pending: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScopeLayout {
    root: PathBuf,
}

impl ScopeLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn local_dir(&self) -> PathBuf {
        self.root.join(SCOPES_DIR).join("local").join("current")
    }

    pub fn account_dir(&self, org_id: &str, user_id: &str) -> PathBuf {
        self.root
            .join(SCOPES_DIR)
            .join("accounts")
            .join(sanitize(org_id))
            .join(sanitize(user_id))
    }

    pub fn has_account_data(&self, org_id: &str, user_id: &str) -> bool {
        self.account_dir(org_id, user_id).exists()
            || self
                .root
                .join("orgs")
                .join(sanitize(org_id))
                .join(sanitize(user_id))
                .exists()
    }

    pub fn ensure_local(&self) -> Result<PathBuf, EngineError> {
        let dir = self.local_dir();
        std::fs::create_dir_all(&dir)?;
        let id_path = dir.join(LOCAL_SCOPE_ID);
        if std::fs::read_to_string(&id_path)
            .ok()
            .is_none_or(|id| id.trim().is_empty())
        {
            std::fs::write(id_path, new_id())?;
        }
        Ok(dir)
    }

    pub fn local_scope_id(&self) -> Result<String, EngineError> {
        let dir = self.ensure_local()?;
        let id = std::fs::read_to_string(dir.join(LOCAL_SCOPE_ID))?;
        Ok(id.trim().to_string())
    }

    /// Move the old account store and installation device id into the canonical
    /// account scope. Every step is idempotent so a crash can resume safely.
    pub fn migrate_account(
        &self,
        org_id: &str,
        user_id: &str,
    ) -> Result<AccountScope, EngineError> {
        let target = self.account_dir(org_id, user_id);
        let legacy = self
            .root
            .join("orgs")
            .join(sanitize(org_id))
            .join(sanitize(user_id));
        let existed = target.exists() || legacy.exists();

        if !target.exists() {
            std::fs::create_dir_all(
                target
                    .parent()
                    .ok_or_else(|| EngineError::Other("account scope has no parent".into()))?,
            )?;
            if legacy.exists() {
                std::fs::rename(&legacy, &target)?;
            } else {
                std::fs::create_dir_all(&target)?;
            }
        }

        let legacy_device = self.root.join("device-id");
        move_if_absent(&legacy_device, &target.join("device-id"))?;
        // A canonical account scope may already own its device id after a
        // partial migration. Never let the leftover installation id become a
        // different account's relay identity.
        if legacy_device.exists() {
            let retired = self.root.join(SCOPES_DIR).join("legacy-device-id");
            std::fs::create_dir_all(
                retired
                    .parent()
                    .ok_or_else(|| EngineError::Other("legacy device id has no parent".into()))?,
            )?;
            if !retired.exists() {
                std::fs::rename(&legacy_device, retired)?;
            } else {
                std::fs::remove_file(&legacy_device)?;
            }
        }
        let legacy_uploads = self.root.join("uploads");
        let movable_uploads = std::fs::symlink_metadata(&legacy_uploads)
            .ok()
            .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
        if movable_uploads && !target.join("uploads").exists() {
            let scoped_uploads = target.join("uploads");
            std::fs::rename(&legacy_uploads, &scoped_uploads)?;
            if let Err(error) = create_upload_compat_link(&legacy_uploads, &scoped_uploads) {
                let _ = std::fs::rename(&scoped_uploads, &legacy_uploads);
                return Err(EngineError::Io(error));
            }
        }

        let marker = MigrationMarker {
            version: 1,
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
        };
        write_json_atomic(&target.join(MIGRATION_MARKER), &marker)?;
        Ok(AccountScope {
            dir: target,
            existed,
        })
    }

    /// Merge Local into an existing account store while both runtimes are
    /// stopped, then replace Local with a blank scope. Registry rows are
    /// re-authored for the account device, Loro documents are merged, and all
    /// device-local ledgers/files are retained.
    pub fn merge_local_into_account(
        &self,
        org_id: &str,
        user_id: &str,
    ) -> Result<AccountScope, EngineError> {
        let local = self.local_dir();
        let target = self.account_dir(org_id, user_id);
        if !target.exists() {
            return self.promote_local(org_id, user_id);
        }
        let source_device = read_id(&local.join("device-id"))?;
        let target_device = read_id(&target.join("device-id"))?;
        merge_docs(&local, &target, &source_device, &target_device)?;
        merge_usage(&local, &target, &target_device)?;
        copy_tree_missing(&local.join("journals"), &target.join("journals"))?;
        copy_tree_missing(&local.join("uploads"), &target.join("uploads"))?;
        std::fs::remove_dir_all(&local)?;
        self.ensure_local()?;
        Ok(AccountScope {
            dir: target,
            existed: true,
        })
    }

    /// Consume the current Local scope into a new account scope, then create a
    /// fresh Local scope immediately. Only valid before that account has a
    /// device-local store; remote state will merge through the normal sync path.
    pub fn promote_local(&self, org_id: &str, user_id: &str) -> Result<AccountScope, EngineError> {
        let local = self.local_dir();
        let target = self.account_dir(org_id, user_id);
        if target.exists() {
            return Err(EngineError::Other(
                "this account already has local data; keep Local separate for now".into(),
            ));
        }
        std::fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| EngineError::Other("account scope has no parent".into()))?,
        )?;
        std::fs::rename(&local, &target)?;
        // Local-only identity is not meaningful once the scope is account-bound.
        let _ = std::fs::remove_file(target.join(LOCAL_SCOPE_ID));
        let marker = MigrationMarker {
            version: 1,
            org_id: org_id.to_string(),
            user_id: user_id.to_string(),
        };
        write_json_atomic(&target.join(MIGRATION_MARKER), &marker)?;
        self.ensure_local()?;
        Ok(AccountScope {
            dir: target,
            existed: false,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AccountScope {
    pub dir: PathBuf,
    /// The account already had device-local data before this startup.
    pub existed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MigrationMarker {
    version: u32,
    org_id: String,
    user_id: String,
}

fn read_id(path: &Path) -> Result<String, EngineError> {
    let id = std::fs::read_to_string(path)?;
    let id = id.trim();
    if id.is_empty() {
        return Err(EngineError::Other(format!(
            "scope identity is empty: {}",
            path.display()
        )));
    }
    Ok(id.to_string())
}

fn merge_docs(
    source_dir: &Path,
    target_dir: &Path,
    source_device: &str,
    target_device: &str,
) -> Result<(), EngineError> {
    let source_path = source_dir.join("docs.sqlite3");
    let target_path = target_dir.join("docs.sqlite3");
    let source = rusqlite::Connection::open(&source_path)
        .map_err(|error| EngineError::Other(format!("open Local documents: {error}")))?;
    let target = rusqlite::Connection::open(&target_path)
        .map_err(|error| EngineError::Other(format!("open Account documents: {error}")))?;

    let snapshots = read_snapshots(&source)?;
    let source_registry = snapshots
        .iter()
        .find(|(id, _)| id == REGISTRY_DOC_ID)
        .map(|(_, bytes)| bytes.as_slice());
    if let Some(source_registry) = source_registry {
        let source_registry = RegistryDoc::from_bytes(source_registry, source_device)?;
        let target_bytes: Option<Vec<u8>> = target
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                [REGISTRY_DOC_ID],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| EngineError::Other(format!("read Account registry: {error}")))?;
        let mut target_registry = match target_bytes {
            Some(bytes) => RegistryDoc::from_bytes(&bytes, target_device)?,
            None => RegistryDoc::new(target_device),
        };
        let state = source_registry.read_all()?;
        for mut space in state.spaces {
            if space.device_id == source_device {
                space.device_id = target_device.to_string();
            }
            target_registry.upsert_space(&space)?;
        }
        for mut chat in state.chats {
            if chat.device_id == source_device {
                chat.device_id = target_device.to_string();
            }
            target_registry.upsert_chat(&chat)?;
        }
        for session in state.sessions {
            target_registry.upsert_session(&session)?;
        }
        save_snapshot(&target, REGISTRY_DOC_ID, &target_registry.to_bytes()?)?;
    }

    for (chat_id, source_bytes) in snapshots
        .iter()
        .filter(|(id, _)| id != REGISTRY_DOC_ID && id != "workspace2")
    {
        let target_bytes: Option<Vec<u8>> = target
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                [chat_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| EngineError::Other(format!("read Account document: {error}")))?;
        let raw = loro::LoroDoc::new();
        if let Some(bytes) = target_bytes {
            raw.import(&bytes)
                .map_err(|error| EngineError::Other(format!("import Account document: {error}")))?;
        }
        raw.import(source_bytes)
            .map_err(|error| EngineError::Other(format!("import Local document: {error}")))?;
        let document = SessionDoc::from_doc(raw);
        save_snapshot(&target, chat_id, &document.export_snapshot()?)?;
    }

    let mut statement = source
        .prepare("SELECT command_id, processed_at FROM processed_commands")
        .map_err(|error| EngineError::Other(format!("read Local command ledger: {error}")))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| EngineError::Other(format!("read Local command ledger: {error}")))?;
    for row in rows {
        let (command_id, processed_at) =
            row.map_err(|error| EngineError::Other(format!("read Local command: {error}")))?;
        target
            .execute(
                "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
                rusqlite::params![command_id, processed_at],
            )
            .map_err(|error| EngineError::Other(format!("merge command ledger: {error}")))?;
    }
    Ok(())
}

fn read_snapshots(
    connection: &rusqlite::Connection,
) -> Result<Vec<(String, Vec<u8>)>, EngineError> {
    let mut statement = connection
        .prepare("SELECT doc_id, bytes FROM snapshots")
        .map_err(|error| EngineError::Other(format!("read snapshots: {error}")))?;
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(|error| EngineError::Other(format!("read snapshots: {error}")))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| EngineError::Other(format!("read snapshot: {error}")))
}

fn save_snapshot(
    connection: &rusqlite::Connection,
    id: &str,
    bytes: &[u8],
) -> Result<(), EngineError> {
    connection
        .execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at",
            rusqlite::params![id, bytes, chrono::Utc::now().timestamp_millis()],
        )
        .map_err(|error| EngineError::Other(format!("save merged snapshot: {error}")))?;
    Ok(())
}

fn merge_usage(
    source_dir: &Path,
    target_dir: &Path,
    target_device: &str,
) -> Result<(), EngineError> {
    let source = source_dir.join("usage.sqlite");
    if !source.exists() {
        return Ok(());
    }
    crate::usage::ensure_schema(&source)
        .map_err(|error| EngineError::Other(format!("migrate Local usage: {error}")))?;
    let target_path = target_dir.join("usage.sqlite");
    crate::usage::ensure_schema(&target_path)
        .map_err(|error| EngineError::Other(format!("migrate Account usage: {error}")))?;
    let target = rusqlite::Connection::open(target_path)
        .map_err(|error| EngineError::Other(format!("open Account usage: {error}")))?;
    target
        .execute(
            "ATTACH DATABASE ?1 AS local_usage",
            [source.to_string_lossy().as_ref()],
        )
        .map_err(|error| EngineError::Other(format!("attach Local usage: {error}")))?;
    target
        .execute(
            "INSERT OR IGNORE INTO usage_events (
                chat_id, journal_seq, device_id, harness, model, cwd, purpose, recorded_at_ms,
                input_tokens, output_tokens, cache_read_input_tokens,
                cache_write_input_tokens, cost_usd, context_tokens, context_window
             ) SELECT chat_id, journal_seq, ?1, harness, model, cwd, purpose, recorded_at_ms,
                input_tokens, output_tokens, cache_read_input_tokens,
                cache_write_input_tokens, cost_usd, context_tokens, context_window
             FROM local_usage.usage_events",
            [target_device],
        )
        .map_err(|error| EngineError::Other(format!("merge Local usage: {error}")))?;
    target
        .execute("DETACH DATABASE local_usage", [])
        .map_err(|error| EngineError::Other(format!("detach Local usage: {error}")))?;
    Ok(())
}

fn copy_tree_missing(source: &Path, target: &Path) -> Result<(), EngineError> {
    if !source.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree_missing(&entry.path(), &destination)?;
        } else if !destination.exists() {
            std::fs::copy(entry.path(), destination)?;
        } else if std::fs::read(entry.path())? != std::fs::read(&destination)? {
            return Err(EngineError::Other(format!(
                "cannot merge different scope files named {}",
                destination.display()
            )));
        }
    }
    Ok(())
}

fn move_if_absent(from: &Path, to: &Path) -> Result<(), EngineError> {
    if from.exists() && !to.exists() {
        std::fs::rename(from, to)?;
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), EngineError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| EngineError::Other(format!("scope manifest: {error}")))?;
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(unix)]
fn create_upload_compat_link(link: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_upload_compat_link(link: &Path, target: &Path) -> std::io::Result<()> {
    match std::os::windows::fs::symlink_dir(target, link) {
        Ok(()) => Ok(()),
        Err(_) => copy_directory(target, link),
    }
}

#[cfg(windows)]
fn copy_directory(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn create_upload_compat_link(_link: &Path, _target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "upload compatibility links are unsupported on this platform",
    ))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_account_data_is_moved_and_local_is_distinct() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("orgs/org/user");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("docs.sqlite3"), b"docs").unwrap();
        std::fs::write(root.path().join("device-id"), b"account-device").unwrap();
        std::fs::create_dir_all(root.path().join("uploads")).unwrap();
        std::fs::write(root.path().join("uploads/image.png"), b"image").unwrap();

        let layout = ScopeLayout::new(root.path());
        let local = layout.ensure_local().unwrap();
        let account = layout.migrate_account("org", "user").unwrap();

        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read(account.dir.join("docs.sqlite3")).unwrap(),
            b"docs"
        );
        assert_eq!(
            std::fs::read_to_string(account.dir.join("device-id")).unwrap(),
            "account-device"
        );
        assert!(local.join(LOCAL_SCOPE_ID).exists());
        assert_ne!(local, account.dir);
        assert_eq!(
            std::fs::read(root.path().join("uploads/image.png")).unwrap(),
            b"image",
            "legacy absolute attachment paths remain readable"
        );
        assert!(account.dir.join("uploads/image.png").exists());
    }

    #[test]
    fn merging_into_existing_account_keeps_documents_and_ledgers() {
        use jolt_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
        use jolt_sync::DocsStore;

        use crate::UsageStore;

        let root = tempfile::tempdir().unwrap();
        let layout = ScopeLayout::new(root.path());
        let local = layout.ensure_local().unwrap();
        std::fs::write(local.join("device-id"), "local-device").unwrap();
        let account = layout.account_dir("org", "user");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join("device-id"), "account-device").unwrap();

        let local_store = DocsStore::open(&local).unwrap();
        let local_doc = SessionDoc::init("chat-local").unwrap();
        local_doc
            .push_message(&SessionMessageEntry {
                id: "message-local".into(),
                role: MessageRole::User,
                parts: vec![MessagePart::Text {
                    id: "text-local".into(),
                    text: "hello".into(),
                }],
                created_at: 1,
                device_id: "local-device".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            })
            .unwrap();
        local_store
            .save_snapshot("chat-local", &local_doc.export_snapshot().unwrap())
            .unwrap();
        local_store.mark_processed("command-local").unwrap();
        drop(local_store);

        let account_store = DocsStore::open(&account).unwrap();
        account_store
            .save_snapshot(
                "chat-account",
                &SessionDoc::init("chat-account")
                    .unwrap()
                    .export_snapshot()
                    .unwrap(),
            )
            .unwrap();
        drop(account_store);
        UsageStore::open(&local.join("usage.sqlite"), "local-device".into()).unwrap();
        UsageStore::open(&account.join("usage.sqlite"), "account-device".into()).unwrap();
        std::fs::create_dir_all(local.join("uploads")).unwrap();
        std::fs::write(local.join("uploads/image.png"), b"image").unwrap();

        layout.merge_local_into_account("org", "user").unwrap();

        let account_store = DocsStore::open(&account).unwrap();
        assert!(account_store.load_snapshot("chat-local").unwrap().is_some());
        assert!(account_store.is_processed("command-local").unwrap());
        assert!(account.join("uploads/image.png").exists());
        assert!(layout.local_dir().join(LOCAL_SCOPE_ID).exists());
        assert!(!layout.local_dir().join("uploads/image.png").exists());
    }

    #[test]
    fn promoting_local_immediately_replaces_it() {
        let root = tempfile::tempdir().unwrap();
        let layout = ScopeLayout::new(root.path());
        let local = layout.ensure_local().unwrap();
        std::fs::write(local.join("local-data"), b"kept").unwrap();

        let account = layout.promote_local("org", "user").unwrap();

        assert_eq!(
            std::fs::read(account.dir.join("local-data")).unwrap(),
            b"kept"
        );
        assert!(layout.local_dir().exists());
        assert!(!layout.local_dir().join("local-data").exists());
        assert!(layout.local_dir().join(LOCAL_SCOPE_ID).exists());
    }
}

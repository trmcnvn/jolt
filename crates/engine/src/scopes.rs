//! Device-local Local and Account data scopes.
//!
//! Local data lives in `scopes/local/current`. Account data lives in
//! `scopes/accounts/<org>/<user>`.

use std::path::{Path, PathBuf};

use rusqlite::OptionalExtension as _;

pub use jolt_api::{ScopeKind, ScopeStatus};
use jolt_registry_model::{REGISTRY_DOC_ID, RegistryDoc};
use jolt_session_doc::SessionDoc;

use crate::{EngineError, new_id};

const SCOPES_DIR: &str = "scopes";
const LOCAL_SCOPE_ID: &str = "local-scope-id";

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

    /// Open the canonical account scope, creating it on first use.
    pub fn ensure_account(&self, org_id: &str, user_id: &str) -> Result<AccountScope, EngineError> {
        let dir = self.account_dir(org_id, user_id);
        let existed = dir.exists();
        std::fs::create_dir_all(&dir)?;
        Ok(AccountScope { dir, existed })
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

    for (chat_id, source_bytes) in snapshots.iter().filter(|(id, _)| id != REGISTRY_DOC_ID) {
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
                cache_write_input_tokens, cost_usd, cost_provenance,
                context_tokens, context_window
             ) SELECT chat_id, journal_seq, ?1, harness, model, cwd, purpose, recorded_at_ms,
                input_tokens, output_tokens, cache_read_input_tokens,
                cache_write_input_tokens, cost_usd, cost_provenance,
                context_tokens, context_window
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
    fn account_and_local_scopes_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let layout = ScopeLayout::new(root.path());
        let local = layout.ensure_local().unwrap();
        let account = layout.ensure_account("org", "user").unwrap();

        assert!(local.join(LOCAL_SCOPE_ID).exists());
        assert_ne!(local, account.dir);
        assert!(!account.existed);
        assert!(layout.ensure_account("org", "user").unwrap().existed);
    }

    #[test]
    fn merging_into_existing_account_keeps_documents_and_ledgers() {
        use jolt_session_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
        use jolt_store::DocsStore;

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

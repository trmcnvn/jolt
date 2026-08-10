//! `DocsStore` — local SQLite persistence for doc snapshots and the
//! processed-command ledger (docs/architecture.md): entries are marked processed
//! BEFORE execution so a crash can never double-execute a command.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// Errors surfaced by [`DocsStore`].
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session document: {0}")]
    SessionDoc(#[from] jolt_session_doc::DocError),
    #[error("session store: {0}")]
    Session(String),
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS snapshots (
    doc_id   TEXT PRIMARY KEY,
    bytes    BLOB NOT NULL,
    saved_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS processed_commands (
    command_id   TEXT PRIMARY KEY,
    processed_at INTEGER NOT NULL
) STRICT;";

/// SQLite-backed store under a data directory (`{data_dir}/docs.sqlite3`).
///
/// Holds the canonical normalized session tables, retained legacy rollback
/// snapshots, registry cache, and the command ledger that provides
/// mark-before-execute idempotence.
pub struct DocsStore {
    conn: Mutex<Connection>,
}

impl DocsStore {
    /// Open (creating directory, database, and schema as needed).
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let data_dir = data_dir.as_ref();
        std::fs::create_dir_all(data_dir)?;
        let mut conn = Connection::open(data_dir.join("docs.sqlite3"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        crate::sessions::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an existing store without running migrations or permitting writes.
    /// Intended for stopped-world verification after a writable migration.
    pub fn open_read_only(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(
            data_dir.as_ref().join("docs.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Latest saved snapshot for `doc_id`, if any.
    pub fn load_snapshot(&self, doc_id: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let bytes = self
            .conn()
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = ?1",
                params![doc_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    /// Save (upsert) the snapshot for `doc_id`.
    pub fn save_snapshot(&self, doc_id: &str, bytes: &[u8]) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO snapshots (doc_id, bytes, saved_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id) DO UPDATE SET bytes = excluded.bytes, saved_at = excluded.saved_at",
            params![doc_id, bytes, now_ms()],
        )?;
        Ok(())
    }

    /// Back up an existing database before opening it through [`DocsStore`].
    /// This is the cutover path: schema migrations must not precede the
    /// rollback artifact. The destination must not already exist.
    pub fn backup_existing_database(
        data_dir: impl AsRef<Path>,
        destination: impl AsRef<Path>,
    ) -> Result<(), StoreError> {
        let destination = destination.as_ref();
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open_with_flags(
            data_dir.as_ref().join("docs.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
        Ok(())
    }

    /// Write a transactionally consistent standalone SQLite backup. The
    /// destination must not already exist.
    pub fn backup_database(&self, destination: impl AsRef<Path>) -> Result<(), StoreError> {
        let destination = destination.as_ref();
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.conn()
            .execute("VACUUM INTO ?1", [destination.to_string_lossy().as_ref()])?;
        Ok(())
    }

    /// Delete the snapshot row for `doc_id`. Missing rows are a no-op.
    pub fn delete_snapshot(&self, doc_id: &str) -> Result<(), StoreError> {
        self.conn()
            .execute("DELETE FROM snapshots WHERE doc_id = ?1", params![doc_id])?;
        Ok(())
    }

    /// Whether `command_id` has already been claimed for execution.
    pub fn is_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let hit = self
            .conn()
            .query_row(
                "SELECT 1 FROM processed_commands WHERE command_id = ?1",
                params![command_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(hit.is_some())
    }

    /// Claim `command_id` for execution — call BEFORE executing (ledger rule:
    /// a crash mid-execution must never re-run the command). Returns `true`
    /// if this call claimed it, `false` if it was already processed.
    pub fn mark_processed(&self, command_id: &str) -> Result<bool, StoreError> {
        let changed = self.conn().execute(
            "INSERT OR IGNORE INTO processed_commands (command_id, processed_at) VALUES (?1, ?2)",
            params![command_id, now_ms()],
        )?;
        Ok(changed > 0)
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-query; the
        // connection itself is still usable.
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert_eq!(store.load_snapshot("chat-1").unwrap(), None);
        store.save_snapshot("chat-1", b"v1").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v1"[..])
        );
        store.save_snapshot("chat-1", b"v2-longer-bytes").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
        // Distinct docs do not collide.
        store.save_snapshot("chat-2", b"other").unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"v2-longer-bytes"[..])
        );
    }

    #[test]
    fn processed_ledger_claims_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocsStore::open(dir.path()).unwrap();

        assert!(!store.is_processed("cmd-1").unwrap());
        assert!(store.mark_processed("cmd-1").unwrap(), "first mark claims");
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(
            !store.mark_processed("cmd-1").unwrap(),
            "second mark must not re-claim"
        );
    }

    #[test]
    fn pre_migration_backup_precedes_session_schema_changes() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("docs.sqlite3");
        let connection = Connection::open(&database).unwrap();
        connection.execute_batch(SCHEMA).unwrap();
        connection
            .execute(
                "INSERT INTO snapshots(doc_id, bytes, saved_at) VALUES ('chat-1', x'01', 1)",
                [],
            )
            .unwrap();
        drop(connection);

        let backup = directory.path().join("backups/pre.sqlite3");
        DocsStore::backup_existing_database(directory.path(), &backup).unwrap();
        DocsStore::open(directory.path()).unwrap();

        let backup = Connection::open(backup).unwrap();
        let session_schema = backup
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_chats'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap();
        assert!(session_schema.is_none());
        let bytes: Vec<u8> = backup
            .query_row(
                "SELECT bytes FROM snapshots WHERE doc_id = 'chat-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bytes, vec![1]);
    }

    #[test]
    fn reopen_preserves_data() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = DocsStore::open(dir.path()).unwrap();
            store.save_snapshot("chat-1", b"persisted").unwrap();
            store.mark_processed("cmd-1").unwrap();
        }
        let store = DocsStore::open(dir.path()).unwrap();
        assert_eq!(
            store.load_snapshot("chat-1").unwrap().as_deref(),
            Some(&b"persisted"[..])
        );
        assert!(store.is_processed("cmd-1").unwrap());
        assert!(!store.mark_processed("cmd-1").unwrap());
    }
}

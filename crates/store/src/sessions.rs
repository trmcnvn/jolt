//! Normalized current-state storage for chat transcripts and durable commands.
//!
//! The assigned chat host is the only transcript writer; other devices submit
//! typed commands and consume paged transcript projections.

use std::sync::Arc;

use jolt_session_doc::{
    MessagePart, MessageRole, MessageStatus, SessionCommandEntry, SessionCommandStatus,
    SessionMessageEntry, TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT, TRANSCRIPT_PAGE_MESSAGE_COUNT,
    TRANSCRIPT_PAGE_TARGET_BYTES, TranscriptBootstrap, TranscriptManifest, TranscriptPage,
    TranscriptPageDescriptor, TranscriptSearchResult, TranscriptTurnDescriptor,
    message_estimated_bytes, transcript_catalog_revision, transcript_entry_preview,
    transcript_page_revision, transcript_search_preview, transcript_searchable_text,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};

use crate::{DocsStore, StoreError};

const SESSION_SCHEMA_MIGRATION: &str = "session-current-state-v1";
const SESSION_PROJECTION_CACHE_MIGRATION: &str = "session-projection-cache-v2";
const SESSION_PUBLICATION_REVISION_MIGRATION: &str = "session-publication-revision-v3";
const SESSION_LEGACY_CLEANUP_MIGRATION: &str = "session-legacy-cleanup-v4";
const TEXT_CHUNK_FOLD_COUNT: i64 = 64;
const TEXT_CHUNK_FOLD_BYTES: i64 = 64 * 1024;

const SESSION_SCHEMA: &str = r#"
CREATE TABLE session_chats (
    chat_id                  TEXT PRIMARY KEY,
    schema_version           INTEGER NOT NULL,
    revision                 INTEGER NOT NULL,
    next_message_ordinal     INTEGER NOT NULL,
    next_command_ordinal     INTEGER NOT NULL,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL
) STRICT;

CREATE TABLE session_pages (
    chat_id                  TEXT NOT NULL,
    page_id                  TEXT NOT NULL,
    page_ordinal             INTEGER NOT NULL,
    first_message_ordinal    INTEGER NOT NULL,
    message_count            INTEGER NOT NULL,
    estimated_bytes          INTEGER NOT NULL,
    revision                 INTEGER NOT NULL,
    sealed                   INTEGER NOT NULL,
    published_hash           TEXT,
    PRIMARY KEY (chat_id, page_id),
    UNIQUE (chat_id, page_ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_messages (
    chat_id                  TEXT NOT NULL,
    message_id               TEXT NOT NULL,
    ordinal                  INTEGER NOT NULL,
    page_id                  TEXT NOT NULL,
    role                     TEXT NOT NULL,
    created_at               INTEGER NOT NULL,
    device_id                TEXT NOT NULL,
    status                   TEXT,
    revision                 INTEGER NOT NULL,
    estimated_bytes          INTEGER NOT NULL,
    PRIMARY KEY (chat_id, message_id),
    UNIQUE (chat_id, ordinal),
    FOREIGN KEY (chat_id, page_id) REFERENCES session_pages(chat_id, page_id)
) STRICT;

CREATE INDEX session_messages_page
    ON session_messages(chat_id, page_id, ordinal);

CREATE TABLE session_parts (
    chat_id                  TEXT NOT NULL,
    message_id               TEXT NOT NULL,
    part_id                  TEXT NOT NULL,
    part_ordinal             INTEGER NOT NULL,
    kind                     TEXT NOT NULL,
    payload_json             TEXT,
    text_base                TEXT,
    revision                 INTEGER NOT NULL,
    PRIMARY KEY (chat_id, message_id, part_id),
    UNIQUE (chat_id, message_id, part_ordinal),
    FOREIGN KEY (chat_id, message_id)
        REFERENCES session_messages(chat_id, message_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_text_chunks (
    chat_id                  TEXT NOT NULL,
    message_id               TEXT NOT NULL,
    part_id                  TEXT NOT NULL,
    chunk_ordinal            INTEGER NOT NULL,
    text                     TEXT NOT NULL,
    PRIMARY KEY (chat_id, message_id, part_id, chunk_ordinal),
    FOREIGN KEY (chat_id, message_id, part_id)
        REFERENCES session_parts(chat_id, message_id, part_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE session_commands (
    chat_id                  TEXT NOT NULL,
    command_id               TEXT NOT NULL,
    command_ordinal          INTEGER NOT NULL,
    edge_seq                 INTEGER,
    payload_json             TEXT NOT NULL,
    issued_by                TEXT NOT NULL,
    issued_at                INTEGER NOT NULL,
    based_on_json            TEXT,
    expires_at               INTEGER,
    status                   TEXT NOT NULL,
    resolution               TEXT,
    delivery_state           TEXT NOT NULL,
    claim_token              TEXT,
    revision                 INTEGER NOT NULL,
    PRIMARY KEY (chat_id, command_id),
    UNIQUE (chat_id, command_ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX session_commands_pending
    ON session_commands(chat_id, status, command_ordinal);

CREATE TABLE session_sync (
    chat_id                          TEXT PRIMARY KEY,
    protocol_generation             INTEGER NOT NULL,
    command_cursor                  INTEGER NOT NULL,
    projection_revision             INTEGER NOT NULL,
    projection_change_revision      INTEGER NOT NULL,
    last_published_local_revision   INTEGER NOT NULL,
    projection_dirty                INTEGER NOT NULL,
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;

"#;

const SESSION_PROJECTION_CACHE_SCHEMA: &str = r#"
ALTER TABLE session_pages ADD COLUMN content_hash TEXT;
ALTER TABLE session_pages ADD COLUMN page_revision TEXT;

CREATE TABLE session_turns (
    chat_id                  TEXT NOT NULL,
    message_id               TEXT NOT NULL,
    ordinal                  INTEGER NOT NULL,
    page_id                  TEXT NOT NULL,
    prompt_preview           TEXT NOT NULL,
    reply_message_id         TEXT,
    reply_preview            TEXT,
    PRIMARY KEY (chat_id, message_id),
    UNIQUE (chat_id, ordinal),
    FOREIGN KEY (chat_id) REFERENCES session_chats(chat_id) ON DELETE CASCADE
) STRICT;
"#;

/// One chat backed by normalized rows in a shared [`DocsStore`].
#[derive(Clone)]
pub struct StoredSession {
    store: Arc<DocsStore>,
    chat_id: Arc<str>,
    change_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Incremental writer for one SQLite-backed streaming assistant message.
pub struct StoredSegmentWriter {
    session: StoredSession,
    entry_id: String,
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS session_store_migrations (
             name TEXT PRIMARY KEY,
             applied_at INTEGER NOT NULL
         ) STRICT;",
    )?;
    let current_state_applied = conn
        .query_row(
            "SELECT 1 FROM session_store_migrations WHERE name = ?1",
            [SESSION_SCHEMA_MIGRATION],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !current_state_applied {
        let transaction = conn.transaction()?;
        transaction.execute_batch(SESSION_SCHEMA)?;
        transaction.execute(
            "INSERT INTO session_store_migrations(name, applied_at) VALUES (?1, ?2)",
            params![SESSION_SCHEMA_MIGRATION, now_ms()],
        )?;
        transaction.commit()?;
    }
    let projection_cache_applied = conn
        .query_row(
            "SELECT 1 FROM session_store_migrations WHERE name = ?1",
            [SESSION_PROJECTION_CACHE_MIGRATION],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if projection_cache_applied {
        migrate_publication_revision(conn)?;
        return migrate_legacy_state(conn);
    }
    let transaction = conn.transaction()?;
    transaction.execute_batch(SESSION_PROJECTION_CACHE_SCHEMA)?;
    let chat_ids = {
        let mut statement = transaction.prepare("SELECT chat_id FROM session_chats")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for chat_id in chat_ids {
        let ids = message_ids(&transaction, &chat_id, None)?;
        for expected_role in ["user", "assistant"] {
            for message_id in &ids {
                let role: String = transaction.query_row(
                    "SELECT role FROM session_messages
                     WHERE chat_id = ?1 AND message_id = ?2",
                    params![chat_id, message_id],
                    |row| row.get(0),
                )?;
                if role == expected_role {
                    refresh_turn_projection(&transaction, &chat_id, message_id)?;
                }
            }
        }
    }
    transaction.execute(
        "INSERT INTO session_store_migrations(name, applied_at) VALUES (?1, ?2)",
        params![SESSION_PROJECTION_CACHE_MIGRATION, now_ms()],
    )?;
    transaction.commit()?;
    migrate_publication_revision(conn)?;
    migrate_legacy_state(conn)
}

fn migrate_publication_revision(conn: &mut Connection) -> Result<(), StoreError> {
    let applied = conn
        .query_row(
            "SELECT 1 FROM session_store_migrations WHERE name = ?1",
            [SESSION_PUBLICATION_REVISION_MIGRATION],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if applied {
        return Ok(());
    }
    let transaction = conn.transaction()?;
    // Fresh v1 schemas already contain the column; older normalized stores need it.
    let has_column = {
        let mut statement = transaction.prepare("PRAGMA table_info(session_sync)")?;
        statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "projection_change_revision")
    };
    if !has_column {
        transaction.execute(
            "ALTER TABLE session_sync
             ADD COLUMN projection_change_revision INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        transaction.execute(
            "UPDATE session_sync
             SET projection_change_revision = (
                 SELECT revision FROM session_chats
                 WHERE session_chats.chat_id = session_sync.chat_id
             )",
            [],
        )?;
        transaction.execute(
            "UPDATE session_sync SET projection_dirty = 1
             WHERE projection_change_revision > last_published_local_revision",
            [],
        )?;
    }
    transaction.execute(
        "INSERT INTO session_store_migrations(name, applied_at) VALUES (?1, ?2)",
        params![SESSION_PUBLICATION_REVISION_MIGRATION, now_ms()],
    )?;
    transaction.commit()?;
    Ok(())
}

fn migrate_legacy_state(conn: &mut Connection) -> Result<(), StoreError> {
    let applied = conn
        .query_row(
            "SELECT 1 FROM session_store_migrations WHERE name = ?1",
            [SESSION_LEGACY_CLEANUP_MIGRATION],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if applied {
        return Ok(());
    }
    let transaction = conn.transaction()?;
    // The snapshots table remains the registry cache. Session snapshots and
    // import reports have no runtime or rollback role after the SessionHub cutover.
    if table_exists(&transaction, "snapshots")? {
        transaction.execute("DELETE FROM snapshots WHERE doc_id != 'registry1'", [])?;
    }
    transaction.execute_batch("DROP TABLE IF EXISTS legacy_session_imports;")?;
    transaction.execute(
        "INSERT INTO session_store_migrations(name, applied_at) VALUES (?1, ?2)",
        params![SESSION_LEGACY_CLEANUP_MIGRATION, now_ms()],
    )?;
    transaction.commit()?;
    // Reclaim pages occupied by removed session snapshots instead of leaving
    // the deleted payload as permanent freelist capacity.
    conn.execute_batch("VACUUM;")?;
    Ok(())
}

impl DocsStore {
    /// Whether normalized state already exists for `chat_id`.
    pub fn unseeded_hub_session_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        if !table_exists(&conn, "session_sync")? {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare(
            "SELECT chat_id FROM session_sync
             WHERE protocol_generation < 2 ORDER BY chat_id",
        )?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn unpublished_hub_session_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        if !table_exists(&conn, "session_sync")? {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare(
            "SELECT chat_id FROM session_sync
             WHERE projection_dirty = 1 ORDER BY chat_id",
        )?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn session_ids(&self) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        if !table_exists(&conn, "session_chats")? {
            return Ok(Vec::new());
        }
        let mut statement = conn.prepare("SELECT chat_id FROM session_chats ORDER BY chat_id")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn session_exists(&self, chat_id: &str) -> Result<bool, StoreError> {
        let conn = self.conn();
        if !table_exists(&conn, "session_chats")? {
            return Ok(false);
        }
        Ok(conn
            .query_row(
                "SELECT 1 FROM session_chats WHERE chat_id = ?1",
                [chat_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Open a normalized session, creating an empty one when necessary.
    pub fn open_session(self: &Arc<Self>, chat_id: &str) -> Result<StoredSession, StoreError> {
        let now = now_ms();
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        ensure_chat(&transaction, chat_id, now)?;
        transaction.commit()?;
        drop(conn);
        Ok(StoredSession {
            store: self.clone(),
            chat_id: Arc::from(chat_id),
            change_hook: None,
        })
    }

    /// Insert already-decoded semantic state under a fresh chat id. This is used
    /// by stopped-world scope migration and deliberately never merges histories.
    pub fn import_session_state(
        self: &Arc<Self>,
        chat_id: &str,
        messages: &[SessionMessageEntry],
        commands: &[SessionCommandEntry],
    ) -> Result<StoredSession, StoreError> {
        if self.session_exists(chat_id)? {
            return Err(StoreError::Session(format!(
                "normalized session {chat_id} already exists"
            )));
        }
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        ensure_chat(&transaction, chat_id, now_ms())?;
        for message in messages {
            insert_message(&transaction, chat_id, message)?;
        }
        for command in commands {
            insert_command(&transaction, chat_id, command)?;
        }
        archive_terminal_command_deliveries(&transaction, chat_id)?;
        transaction.commit()?;
        drop(conn);
        self.open_session(chat_id)
    }

    /// Replace one session's semantic state during a stopped-world scope move.
    pub fn replace_session_state(
        self: &Arc<Self>,
        chat_id: &str,
        messages: &[SessionMessageEntry],
        commands: &[SessionCommandEntry],
    ) -> Result<StoredSession, StoreError> {
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        transaction.execute("DELETE FROM session_chats WHERE chat_id = ?1", [chat_id])?;
        ensure_chat(&transaction, chat_id, now_ms())?;
        for message in messages {
            insert_message(&transaction, chat_id, message)?;
        }
        for command in commands {
            insert_command(&transaction, chat_id, command)?;
        }
        archive_terminal_command_deliveries(&transaction, chat_id)?;
        transaction.commit()?;
        drop(conn);
        self.open_session(chat_id)
    }
}

impl StoredSegmentWriter {
    pub fn begin(
        session: &StoredSession,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<Self, StoreError> {
        session.begin_assistant(entry_id, device_id, created_at)?;
        Ok(Self {
            session: session.clone(),
            entry_id: entry_id.to_string(),
        })
    }

    pub fn sync(&mut self, folded: &[MessagePart]) -> Result<(), StoreError> {
        self.session.sync_assistant(&self.entry_id, folded)
    }

    pub fn finish(self, folded: &[MessagePart], status: MessageStatus) -> Result<(), StoreError> {
        self.session
            .finish_assistant(&self.entry_id, folded, status)
    }
}

impl StoredSession {
    /// Attach one process-local callback invoked after committed mutations.
    pub fn with_change_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.change_hook = Some(hook);
        self
    }

    fn notify_changed(&self) {
        if let Some(hook) = &self.change_hook {
            hook();
        }
    }

    pub fn chat_id(&self) -> &str {
        &self.chat_id
    }

    pub fn revision(&self) -> Result<u64, StoreError> {
        let revision: i64 = self.store.conn().query_row(
            "SELECT revision FROM session_chats WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        u64_from_sql(revision)
    }

    pub fn projection_revision(&self) -> Result<u64, StoreError> {
        let revision: i64 = self.store.conn().query_row(
            "SELECT projection_revision FROM session_sync WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        u64_from_sql(revision)
    }

    pub fn projection_change_revision(&self) -> Result<u64, StoreError> {
        let revision: i64 = self.store.conn().query_row(
            "SELECT projection_change_revision FROM session_sync WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        u64_from_sql(revision)
    }

    pub fn message_count(&self) -> Result<usize, StoreError> {
        let count: i64 = self.store.conn().query_row(
            "SELECT COUNT(*) FROM session_messages WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        Ok(usize_from_sql(count)?)
    }

    pub fn read_entries(&self) -> Result<Vec<SessionMessageEntry>, StoreError> {
        let conn = self.store.conn();
        let ids = message_ids(&conn, self.chat_id(), None)?;
        ids.into_iter()
            .map(|id| read_entry(&conn, self.chat_id(), &id))
            .collect()
    }

    pub fn read_entry_at(&self, index: usize) -> Result<Option<SessionMessageEntry>, StoreError> {
        let conn = self.store.conn();
        let id = conn
            .query_row(
                "SELECT message_id FROM session_messages
                 WHERE chat_id = ?1 ORDER BY ordinal LIMIT 1 OFFSET ?2",
                params![self.chat_id(), sql_usize(index)?],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        id.map(|id| read_entry(&conn, self.chat_id(), &id))
            .transpose()
    }

    pub fn read_page(&self, page_id: &str) -> Result<Vec<SessionMessageEntry>, StoreError> {
        let conn = self.store.conn();
        Ok(transcript_page(&conn, self.chat_id(), page_id)?
            .map_or_else(Vec::new, |page| page.messages))
    }

    pub fn transcript_manifest(&self) -> Result<TranscriptManifest, StoreError> {
        let conn = self.store.conn();
        transcript_manifest(&conn, self.chat_id())
    }

    pub fn transcript_page(&self, page_id: &str) -> Result<Option<TranscriptPage>, StoreError> {
        let conn = self.store.conn();
        transcript_page(&conn, self.chat_id(), page_id)
    }

    /// Whether this immutable page body has already been accepted by edge storage.
    pub fn command_cursor(&self) -> Result<u64, StoreError> {
        let cursor: i64 = self.store.conn().query_row(
            "SELECT command_cursor FROM session_sync WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        u64_from_sql(cursor)
    }

    pub fn set_command_cursor(&self, cursor: u64) -> Result<(), StoreError> {
        self.store.conn().execute(
            "UPDATE session_sync SET command_cursor = MAX(command_cursor, ?2)
             WHERE chat_id = ?1",
            params![self.chat_id(), sql_u64(cursor)?],
        )?;
        Ok(())
    }

    pub fn hub_seeded(&self) -> Result<bool, StoreError> {
        let generation: i64 = self.store.conn().query_row(
            "SELECT protocol_generation FROM session_sync WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        Ok(generation >= 2)
    }

    pub fn hub_projection_dirty(&self) -> Result<bool, StoreError> {
        let dirty: i64 = self.store.conn().query_row(
            "SELECT projection_dirty FROM session_sync WHERE chat_id = ?1",
            [self.chat_id()],
            |row| row.get(0),
        )?;
        Ok(dirty != 0)
    }

    pub fn mark_hub_projection_published(&self, local_revision: u64) -> Result<(), StoreError> {
        self.store.conn().execute(
            "UPDATE session_sync
             SET protocol_generation = 2,
                 last_published_local_revision = MAX(last_published_local_revision, ?2),
                 projection_dirty = CASE
                     WHEN projection_change_revision <= ?2
                     THEN 0 ELSE projection_dirty END
             WHERE chat_id = ?1",
            params![self.chat_id(), sql_u64(local_revision)?],
        )?;
        Ok(())
    }

    pub fn page_is_published(&self, page_id: &str, content_hash: &str) -> Result<bool, StoreError> {
        let published = self
            .store
            .conn()
            .query_row(
                "SELECT published_hash FROM session_pages
                 WHERE chat_id = ?1 AND page_id = ?2 AND sealed = 1",
                params![self.chat_id(), page_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(published.as_deref() == Some(content_hash))
    }

    /// Record a successful immutable page upload without advancing semantic state.
    pub fn mark_page_published(
        &self,
        page_id: &str,
        content_hash: &str,
    ) -> Result<bool, StoreError> {
        let changed = self.store.conn().execute(
            "UPDATE session_pages SET published_hash = ?3
             WHERE chat_id = ?1 AND page_id = ?2 AND sealed = 1
               AND published_hash IS NOT ?3",
            params![self.chat_id(), page_id, content_hash],
        )?;
        Ok(changed > 0)
    }

    pub fn transcript_bootstrap(&self, sequence: u64) -> Result<TranscriptBootstrap, StoreError> {
        let conn = self.store.conn();
        let manifest = transcript_manifest(&conn, self.chat_id())?;
        let mut pages = Vec::new();
        let mut count = 0usize;
        for descriptor in manifest.pages.iter().rev() {
            let page =
                transcript_page(&conn, self.chat_id(), &descriptor.id)?.ok_or_else(|| {
                    StoreError::Session(format!("manifest page {} is missing", descriptor.id))
                })?;
            count += page.messages.len();
            pages.push(page);
            if count >= TRANSCRIPT_BOOTSTRAP_MESSAGE_COUNT {
                break;
            }
        }
        pages.reverse();
        Ok(TranscriptBootstrap {
            sequence,
            manifest,
            pages,
        })
    }

    pub fn search_transcript(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TranscriptSearchResult>, StoreError> {
        let terms = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.store.conn();
        let manifest = transcript_manifest(&conn, self.chat_id())?;
        let mut results = Vec::new();
        for descriptor in manifest.pages.iter().rev() {
            let Some(page) = transcript_page(&conn, self.chat_id(), &descriptor.id)? else {
                continue;
            };
            for (offset, entry) in page.messages.iter().enumerate().rev() {
                let text = transcript_searchable_text(entry);
                let lowercase = text.to_lowercase();
                if !terms.iter().all(|term| lowercase.contains(term)) {
                    continue;
                }
                results.push(TranscriptSearchResult {
                    message_id: entry.id.clone(),
                    page_id: page.id.clone(),
                    ordinal: page.first_ordinal + offset,
                    role: entry.role,
                    preview: transcript_search_preview(&text, &terms, 240),
                    created_at: entry.created_at,
                });
                if results.len() == limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    /// Insert a complete message idempotently. Returns whether a row was added.
    pub fn push_message(&self, entry: &SessionMessageEntry) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let inserted = insert_message(&transaction, self.chat_id(), entry)?;
        transaction.commit()?;
        if inserted {
            self.notify_changed();
        }
        Ok(inserted)
    }

    /// Begin an empty streaming assistant message.
    pub fn begin_assistant(
        &self,
        entry_id: &str,
        device_id: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        let inserted = self.push_message(&SessionMessageEntry {
            id: entry_id.to_string(),
            role: MessageRole::Assistant,
            parts: Vec::new(),
            created_at,
            device_id: device_id.to_string(),
            status: Some(MessageStatus::Streaming),
            continuation_of: None,
        })?;
        if !inserted {
            return Err(StoreError::Session(format!(
                "assistant message {entry_id} already exists"
            )));
        }
        Ok(())
    }

    /// Incrementally synchronize folded parts into one streaming assistant row.
    pub fn sync_assistant(&self, entry_id: &str, folded: &[MessagePart]) -> Result<(), StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let current = read_entry(&transaction, self.chat_id(), entry_id)?;
        if current.role != MessageRole::Assistant {
            return Err(StoreError::Session(format!(
                "message {entry_id} is not an assistant entry"
            )));
        }
        let mut dirty = false;
        for (index, part) in folded.iter().enumerate() {
            match current.parts.get(index) {
                None => {
                    insert_part(&transaction, self.chat_id(), entry_id, index, part, 0)?;
                    dirty = true;
                }
                Some(previous) if previous == part => {}
                Some(MessagePart::Text {
                    id: old_id,
                    text: old,
                }) => {
                    let MessagePart::Text { id, text } = part else {
                        replace_part(&transaction, self.chat_id(), entry_id, index, old_id, part)?;
                        dirty = true;
                        continue;
                    };
                    if old_id == id && text.starts_with(old.as_str()) {
                        let suffix = &text[old.len()..];
                        if !suffix.is_empty() {
                            append_text_chunk(&transaction, self.chat_id(), entry_id, id, suffix)?;
                            fold_text_chunks_if_needed(&transaction, self.chat_id(), entry_id, id)?;
                            dirty = true;
                        }
                    } else {
                        replace_part(&transaction, self.chat_id(), entry_id, index, old_id, part)?;
                        dirty = true;
                    }
                }
                Some(previous) => {
                    replace_part(
                        &transaction,
                        self.chat_id(),
                        entry_id,
                        index,
                        previous_id(previous),
                        part,
                    )?;
                    dirty = true;
                }
            }
        }
        if current.parts.len() > folded.len() {
            for part in &current.parts[folded.len()..] {
                transaction.execute(
                    "DELETE FROM session_parts
                     WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
                    params![self.chat_id(), entry_id, previous_id(part)],
                )?;
            }
            dirty = true;
        }
        if dirty {
            refresh_message_budget(&transaction, self.chat_id(), entry_id)?;
            mark_message_changed(&transaction, self.chat_id(), entry_id)?;
        }
        transaction.commit()?;
        if dirty {
            self.notify_changed();
        }
        Ok(())
    }

    pub fn finish_assistant(
        &self,
        entry_id: &str,
        folded: &[MessagePart],
        status: MessageStatus,
    ) -> Result<(), StoreError> {
        self.sync_assistant(entry_id, folded)?;
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        compact_message_text(&transaction, self.chat_id(), entry_id)?;
        let changed = transaction.execute(
            "UPDATE session_messages SET status = ?3
             WHERE chat_id = ?1 AND message_id = ?2 AND status IS NOT ?3",
            params![self.chat_id(), entry_id, message_status(status)],
        )?;
        if changed > 0 {
            mark_message_changed(&transaction, self.chat_id(), entry_id)?;
            bump_projection_revision(&transaction, self.chat_id())?;
        }
        transaction.commit()?;
        if changed > 0 {
            self.notify_changed();
        }
        Ok(())
    }

    pub fn set_message_status(
        &self,
        message_id: &str,
        status: MessageStatus,
    ) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE session_messages SET status = ?3
             WHERE chat_id = ?1 AND message_id = ?2 AND status IS NOT ?3",
            params![self.chat_id(), message_id, message_status(status)],
        )?;
        if changed > 0 {
            mark_message_changed(&transaction, self.chat_id(), message_id)?;
            bump_projection_revision(&transaction, self.chat_id())?;
        }
        transaction.commit()?;
        if changed > 0 {
            self.notify_changed();
        }
        Ok(changed > 0)
    }

    pub fn update_text_message(
        &self,
        message_id: &str,
        part_id: &str,
        text: &str,
        status: MessageStatus,
    ) -> Result<bool, StoreError> {
        self.replace_text(message_id, part_id, text, Some(status))
    }

    pub fn replace_text_part(
        &self,
        message_id: &str,
        part_id: &str,
        text: &str,
    ) -> Result<bool, StoreError> {
        self.replace_text(message_id, part_id, text, None)
    }

    fn replace_text(
        &self,
        message_id: &str,
        part_id: &str,
        text: &str,
        status: Option<MessageStatus>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE session_parts SET text_base = ?4
             WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3 AND kind = 'text'",
            params![self.chat_id(), message_id, part_id, text],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(false);
        }
        transaction.execute(
            "DELETE FROM session_text_chunks
             WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
            params![self.chat_id(), message_id, part_id],
        )?;
        if let Some(status) = status {
            transaction.execute(
                "UPDATE session_messages SET status = ?3
                 WHERE chat_id = ?1 AND message_id = ?2",
                params![self.chat_id(), message_id, message_status(status)],
            )?;
        }
        refresh_message_budget(&transaction, self.chat_id(), message_id)?;
        mark_message_changed(&transaction, self.chat_id(), message_id)?;
        bump_projection_revision(&transaction, self.chat_id())?;
        transaction.commit()?;
        self.notify_changed();
        Ok(true)
    }

    pub fn append_error_part(
        &self,
        message_id: &str,
        part_id: &str,
        message: &str,
    ) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let exists = transaction
            .query_row(
                "SELECT 1 FROM session_parts
                 WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
                params![self.chat_id(), message_id, part_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            transaction.commit()?;
            return Ok(true);
        }
        let next: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(part_ordinal), -1) + 1 FROM session_parts
             WHERE chat_id = ?1 AND message_id = ?2",
            params![self.chat_id(), message_id],
            |row| row.get(0),
        )?;
        let part = MessagePart::Error {
            id: part_id.to_string(),
            message: message.to_string(),
        };
        insert_part(
            &transaction,
            self.chat_id(),
            message_id,
            usize_from_sql(next)?,
            &part,
            0,
        )?;
        refresh_message_budget(&transaction, self.chat_id(), message_id)?;
        mark_message_changed(&transaction, self.chat_id(), message_id)?;
        transaction.commit()?;
        self.notify_changed();
        Ok(true)
    }

    pub fn resolve_input(&self, request_id: &str) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let row = transaction
            .query_row(
                "SELECT message_id, part_ordinal, payload_json FROM session_parts
                 WHERE chat_id = ?1 AND part_id = ?2 AND kind = 'input'
                 ORDER BY message_id LIMIT 1",
                params![self.chat_id(), request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((message_id, ordinal, payload)) = row else {
            transaction.commit()?;
            return Ok(false);
        };
        let payload = payload.ok_or_else(|| {
            StoreError::Session(format!("input part {request_id} has no payload"))
        })?;
        let mut part: MessagePart = serde_json::from_str(&payload)?;
        let MessagePart::Input { resolved, .. } = &mut part else {
            return Err(StoreError::Session(format!(
                "part {request_id} is not an input payload"
            )));
        };
        if *resolved {
            transaction.commit()?;
            return Ok(true);
        }
        *resolved = true;
        transaction.execute(
            "UPDATE session_parts SET payload_json = ?4
             WHERE chat_id = ?1 AND message_id = ?2 AND part_ordinal = ?3",
            params![
                self.chat_id(),
                message_id,
                ordinal,
                serde_json::to_string(&part)?
            ],
        )?;
        mark_message_changed(&transaction, self.chat_id(), &message_id)?;
        transaction.commit()?;
        self.notify_changed();
        Ok(true)
    }

    pub fn queue_command(&self, entry: &SessionCommandEntry) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let inserted = insert_command(&transaction, self.chat_id(), entry)?;
        transaction.commit()?;
        if inserted {
            self.notify_changed();
        }
        Ok(inserted)
    }

    pub fn read_commands(&self) -> Result<Vec<SessionCommandEntry>, StoreError> {
        read_commands(&self.store.conn(), self.chat_id())
    }

    pub fn commands_pending_hub_submission(&self) -> Result<Vec<SessionCommandEntry>, StoreError> {
        let conn = self.store.conn();
        let mut statement = conn.prepare(
            "SELECT command_id, payload_json, issued_by, issued_at,
                    based_on_json, expires_at, status, resolution
             FROM session_commands
             WHERE chat_id = ?1 AND delivery_state = 'local'
             ORDER BY command_ordinal",
        )?;
        statement
            .query_map([self.chat_id()], decode_command_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn mark_command_hub_submitted(&self, command_id: &str) -> Result<bool, StoreError> {
        self.mark_command_hub_delivery(command_id, "submitted")
    }

    pub fn mark_command_hub_rejected(&self, command_id: &str) -> Result<bool, StoreError> {
        self.mark_command_hub_delivery(command_id, "rejected")
    }

    fn mark_command_hub_delivery(
        &self,
        command_id: &str,
        delivery_state: &str,
    ) -> Result<bool, StoreError> {
        let changed = self.store.conn().execute(
            "UPDATE session_commands SET delivery_state = ?3
             WHERE chat_id = ?1 AND command_id = ?2 AND delivery_state = 'local'",
            params![self.chat_id(), command_id, delivery_state],
        )?;
        Ok(changed > 0)
    }

    pub fn read_command(
        &self,
        command_id: &str,
    ) -> Result<Option<SessionCommandEntry>, StoreError> {
        self.store
            .conn()
            .query_row(
                "SELECT command_id, payload_json, issued_by, issued_at,
                        based_on_json, expires_at, status, resolution
                 FROM session_commands
                 WHERE chat_id = ?1 AND command_id = ?2",
                params![self.chat_id(), command_id],
                decode_command_row,
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn set_command_status(
        &self,
        command_id: &str,
        status: SessionCommandStatus,
        resolution: Option<&str>,
    ) -> Result<bool, StoreError> {
        let mut conn = self.store.conn();
        let transaction = conn.transaction()?;
        let changed = transaction.execute(
            "UPDATE session_commands
             SET status = ?3, resolution = ?4, revision = revision + 1
             WHERE chat_id = ?1 AND command_id = ?2",
            params![
                self.chat_id(),
                command_id,
                command_status(status),
                resolution
            ],
        )?;
        if changed > 0 {
            bump_chat(&transaction, self.chat_id())?;
        }
        transaction.commit()?;
        if changed > 0 {
            self.notify_changed();
        }
        Ok(changed > 0)
    }

    pub fn semantic_hash(&self) -> Result<String, StoreError> {
        semantic_hash(&self.read_entries()?, &self.read_commands()?)
    }

    pub fn delete(self) -> Result<(), StoreError> {
        self.store.conn().execute(
            "DELETE FROM session_chats WHERE chat_id = ?1",
            [self.chat_id()],
        )?;
        Ok(())
    }
}

fn ensure_chat(transaction: &Transaction<'_>, chat_id: &str, now: i64) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO session_chats (
            chat_id, schema_version, revision, next_message_ordinal,
            next_command_ordinal, created_at, updated_at
         ) VALUES (?1, 1, 0, 0, 0, ?2, ?2)",
        params![chat_id, now],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO session_sync (
            chat_id, protocol_generation, command_cursor, projection_revision,
            projection_change_revision, last_published_local_revision, projection_dirty
         ) VALUES (?1, 1, 0, 0, 0, 0, 0)",
        [chat_id],
    )?;
    Ok(())
}

fn insert_message(
    transaction: &Transaction<'_>,
    chat_id: &str,
    entry: &SessionMessageEntry,
) -> Result<bool, StoreError> {
    let exists = transaction
        .query_row(
            "SELECT 1 FROM session_messages WHERE chat_id = ?1 AND message_id = ?2",
            params![chat_id, entry.id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(false);
    }
    let ordinal: i64 = transaction.query_row(
        "SELECT next_message_ordinal FROM session_chats WHERE chat_id = ?1",
        [chat_id],
        |row| row.get(0),
    )?;
    let estimated = sql_usize(message_estimated_bytes(entry))?;
    let page_id = choose_page(transaction, chat_id, &entry.id, ordinal, estimated)?;
    let revision = bump_chat(transaction, chat_id)?;
    transaction.execute(
        "INSERT INTO session_messages (
            chat_id, message_id, ordinal, page_id, role, created_at, device_id,
            status, revision, estimated_bytes
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            chat_id,
            entry.id,
            ordinal,
            page_id,
            message_role(entry.role),
            entry.created_at,
            entry.device_id,
            entry.status.map(message_status),
            revision,
            estimated
        ],
    )?;
    for (index, part) in entry.parts.iter().enumerate() {
        insert_part(transaction, chat_id, &entry.id, index, part, revision)?;
    }
    transaction.execute(
        "UPDATE session_pages
         SET message_count = message_count + 1,
             estimated_bytes = estimated_bytes + ?3,
             revision = ?4,
             content_hash = NULL,
             page_revision = NULL,
             published_hash = NULL
         WHERE chat_id = ?1 AND page_id = ?2",
        params![chat_id, page_id, estimated, revision],
    )?;
    transaction.execute(
        "UPDATE session_chats SET next_message_ordinal = next_message_ordinal + 1
         WHERE chat_id = ?1",
        [chat_id],
    )?;
    refresh_turn_projection(transaction, chat_id, &entry.id)?;
    bump_projection_revision(transaction, chat_id)?;
    Ok(true)
}

fn choose_page(
    transaction: &Transaction<'_>,
    chat_id: &str,
    first_message_id: &str,
    first_message_ordinal: i64,
    message_bytes: i64,
) -> Result<String, StoreError> {
    let current = transaction
        .query_row(
            "SELECT page_id, page_ordinal, message_count, estimated_bytes
             FROM session_pages
             WHERE chat_id = ?1 AND sealed = 0
             ORDER BY page_ordinal DESC LIMIT 1",
            [chat_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((page_id, page_ordinal, count, bytes)) = current {
        let over_count = count >= sql_usize(TRANSCRIPT_PAGE_MESSAGE_COUNT)?;
        let over_bytes = count > 0
            && bytes.saturating_add(message_bytes) > sql_usize(TRANSCRIPT_PAGE_TARGET_BYTES)?;
        if !over_count && !over_bytes {
            return Ok(page_id);
        }
        transaction.execute(
            "UPDATE session_pages SET sealed = 1 WHERE chat_id = ?1 AND page_id = ?2",
            params![chat_id, page_id],
        )?;
        create_page(
            transaction,
            chat_id,
            first_message_id,
            page_ordinal + 1,
            first_message_ordinal,
        )?;
    } else {
        create_page(
            transaction,
            chat_id,
            first_message_id,
            0,
            first_message_ordinal,
        )?;
    }
    Ok(first_message_id.to_string())
}

fn create_page(
    transaction: &Transaction<'_>,
    chat_id: &str,
    page_id: &str,
    page_ordinal: i64,
    first_message_ordinal: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO session_pages (
            chat_id, page_id, page_ordinal, first_message_ordinal,
            message_count, estimated_bytes, revision, sealed, published_hash
         ) VALUES (?1, ?2, ?3, ?4, 0, 0, 0, 0, NULL)",
        params![chat_id, page_id, page_ordinal, first_message_ordinal],
    )?;
    Ok(())
}

fn insert_part(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
    part_ordinal: usize,
    part: &MessagePart,
    revision: i64,
) -> Result<(), StoreError> {
    let (kind, payload, text) = encode_part(part)?;
    transaction.execute(
        "INSERT INTO session_parts (
            chat_id, message_id, part_id, part_ordinal, kind,
            payload_json, text_base, revision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            chat_id,
            message_id,
            part.id(),
            sql_usize(part_ordinal)?,
            kind,
            payload,
            text,
            revision
        ],
    )?;
    Ok(())
}

fn replace_part(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
    part_ordinal: usize,
    previous_part_id: &str,
    part: &MessagePart,
) -> Result<(), StoreError> {
    transaction.execute(
        "DELETE FROM session_parts
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
        params![chat_id, message_id, previous_part_id],
    )?;
    insert_part(transaction, chat_id, message_id, part_ordinal, part, 0)
}

fn encode_part(
    part: &MessagePart,
) -> Result<(&'static str, Option<String>, Option<&str>), StoreError> {
    match part {
        MessagePart::Text { text, .. } => Ok(("text", None, Some(text))),
        MessagePart::TextReveal { .. } => {
            Ok(("textReveal", Some(serde_json::to_string(part)?), None))
        }
        MessagePart::Tool { .. } => Ok(("tool", Some(serde_json::to_string(part)?), None)),
        MessagePart::Input { .. } => Ok(("input", Some(serde_json::to_string(part)?), None)),
        MessagePart::Error { .. } => Ok(("error", Some(serde_json::to_string(part)?), None)),
        MessagePart::HarnessSwitch { .. } => {
            Ok(("harnessSwitch", Some(serde_json::to_string(part)?), None))
        }
        MessagePart::Changes { .. } => Ok(("changes", Some(serde_json::to_string(part)?), None)),
    }
}

fn decode_part(
    kind: &str,
    part_id: String,
    payload: Option<String>,
    text: Option<String>,
) -> Result<MessagePart, StoreError> {
    if kind == "text" {
        return Ok(MessagePart::Text {
            id: part_id,
            text: text.unwrap_or_default(),
        });
    }
    let payload = payload.ok_or_else(|| {
        StoreError::Session(format!("{kind} part {part_id} is missing its payload"))
    })?;
    let part: MessagePart = serde_json::from_str(&payload)?;
    if part.id() != part_id || part_kind(&part) != kind {
        return Err(StoreError::Session(format!(
            "stored part identity mismatch for {part_id}"
        )));
    }
    Ok(part)
}

fn part_kind(part: &MessagePart) -> &'static str {
    match part {
        MessagePart::Text { .. } => "text",
        MessagePart::TextReveal { .. } => "textReveal",
        MessagePart::Tool { .. } => "tool",
        MessagePart::Input { .. } => "input",
        MessagePart::Error { .. } => "error",
        MessagePart::HarnessSwitch { .. } => "harnessSwitch",
        MessagePart::Changes { .. } => "changes",
    }
}

fn previous_id(part: &MessagePart) -> &str {
    part.id()
}

fn append_text_chunk(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
    part_id: &str,
    text: &str,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO session_text_chunks (
            chat_id, message_id, part_id, chunk_ordinal, text
         ) VALUES (
            ?1, ?2, ?3,
            COALESCE((SELECT MAX(chunk_ordinal) + 1 FROM session_text_chunks
                      WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3), 0),
            ?4
         )",
        params![chat_id, message_id, part_id, text],
    )?;
    Ok(())
}

fn fold_text_chunks_if_needed(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
    part_id: &str,
) -> Result<(), StoreError> {
    let (count, bytes): (i64, i64) = transaction.query_row(
        "SELECT COUNT(*), COALESCE(SUM(length(CAST(text AS BLOB))), 0)
         FROM session_text_chunks
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
        params![chat_id, message_id, part_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if count < TEXT_CHUNK_FOLD_COUNT && bytes < TEXT_CHUNK_FOLD_BYTES {
        return Ok(());
    }
    compact_part_text(transaction, chat_id, message_id, part_id)
}

fn compact_message_text(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
        "SELECT part_id FROM session_parts
         WHERE chat_id = ?1 AND message_id = ?2 AND kind = 'text'",
    )?;
    let ids = statement
        .query_map(params![chat_id, message_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for part_id in ids {
        compact_part_text(transaction, chat_id, message_id, &part_id)?;
    }
    Ok(())
}

fn compact_part_text(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
    part_id: &str,
) -> Result<(), StoreError> {
    let base: String = transaction.query_row(
        "SELECT COALESCE(text_base, '') FROM session_parts
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3 AND kind = 'text'",
        params![chat_id, message_id, part_id],
        |row| row.get(0),
    )?;
    let mut statement = transaction.prepare(
        "SELECT text FROM session_text_chunks
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3
         ORDER BY chunk_ordinal",
    )?;
    let chunks = statement
        .query_map(params![chat_id, message_id, part_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if chunks.is_empty() {
        return Ok(());
    }
    let extra = chunks.iter().map(String::len).sum::<usize>();
    let mut text = String::with_capacity(base.len() + extra);
    text.push_str(&base);
    for chunk in chunks {
        text.push_str(&chunk);
    }
    transaction.execute(
        "UPDATE session_parts SET text_base = ?4
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
        params![chat_id, message_id, part_id, text],
    )?;
    transaction.execute(
        "DELETE FROM session_text_chunks
         WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3",
        params![chat_id, message_id, part_id],
    )?;
    Ok(())
}

fn refresh_message_budget(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
) -> Result<(), StoreError> {
    let entry = read_entry(transaction, chat_id, message_id)?;
    let next = sql_usize(message_estimated_bytes(&entry))?;
    let (page_id, previous): (String, i64) = transaction.query_row(
        "SELECT page_id, estimated_bytes FROM session_messages
         WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, message_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    transaction.execute(
        "UPDATE session_messages SET estimated_bytes = ?3
         WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, message_id, next],
    )?;
    transaction.execute(
        "UPDATE session_pages
         SET estimated_bytes = MAX(0, estimated_bytes + ?3 - ?4)
         WHERE chat_id = ?1 AND page_id = ?2",
        params![chat_id, page_id, next, previous],
    )?;
    Ok(())
}

fn mark_message_changed(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
) -> Result<i64, StoreError> {
    let revision = bump_chat(transaction, chat_id)?;
    let sealed: i64 = transaction.query_row(
        "SELECT pages.sealed
         FROM session_messages AS messages
         JOIN session_pages AS pages
           ON pages.chat_id = messages.chat_id AND pages.page_id = messages.page_id
         WHERE messages.chat_id = ?1 AND messages.message_id = ?2",
        params![chat_id, message_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "UPDATE session_messages SET revision = ?3
         WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, message_id, revision],
    )?;
    transaction.execute(
        "UPDATE session_pages
         SET revision = ?3, content_hash = NULL, page_revision = NULL, published_hash = NULL
         WHERE chat_id = ?1 AND page_id = (
            SELECT page_id FROM session_messages
            WHERE chat_id = ?1 AND message_id = ?2
         )",
        params![chat_id, message_id, revision],
    )?;
    refresh_turn_projection(transaction, chat_id, message_id)?;
    transaction.execute(
        "UPDATE session_sync
         SET projection_revision = projection_revision + ?2,
             projection_change_revision = projection_change_revision + 1,
             projection_dirty = 1
         WHERE chat_id = ?1",
        params![chat_id, sealed],
    )?;
    Ok(revision)
}

fn refresh_turn_projection(
    transaction: &Transaction<'_>,
    chat_id: &str,
    message_id: &str,
) -> Result<(), StoreError> {
    let (role, ordinal, page_id): (String, i64, String) = transaction.query_row(
        "SELECT role, ordinal, page_id FROM session_messages
         WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, message_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    match parse_message_role(&role)? {
        MessageRole::User => {
            let entry = read_entry(transaction, chat_id, message_id)?;
            transaction.execute(
                "INSERT INTO session_turns (
                    chat_id, message_id, ordinal, page_id, prompt_preview,
                    reply_message_id, reply_preview
                 ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)
                 ON CONFLICT(chat_id, message_id) DO UPDATE SET
                    ordinal = excluded.ordinal,
                    page_id = excluded.page_id,
                    prompt_preview = excluded.prompt_preview",
                params![
                    chat_id,
                    message_id,
                    ordinal,
                    page_id,
                    transcript_entry_preview(&entry, 160)
                ],
            )?;
        }
        MessageRole::Assistant => {
            let turn = transaction
                .query_row(
                    "SELECT message_id FROM session_turns
                     WHERE chat_id = ?1 AND ordinal < ?2
                     ORDER BY ordinal DESC LIMIT 1",
                    params![chat_id, ordinal],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(turn_id) = turn {
                recompute_turn_reply(transaction, chat_id, &turn_id)?;
            }
        }
        MessageRole::System => {}
    }
    Ok(())
}

fn recompute_turn_reply(
    transaction: &Transaction<'_>,
    chat_id: &str,
    turn_id: &str,
) -> Result<(), StoreError> {
    let turn_ordinal: i64 = transaction.query_row(
        "SELECT ordinal FROM session_turns WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, turn_id],
        |row| row.get(0),
    )?;
    let next_turn: Option<i64> = transaction
        .query_row(
            "SELECT ordinal FROM session_turns
             WHERE chat_id = ?1 AND ordinal > ?2 ORDER BY ordinal LIMIT 1",
            params![chat_id, turn_ordinal],
            |row| row.get(0),
        )
        .optional()?;
    let ids = {
        let mut statement = transaction.prepare(
            "SELECT message_id FROM session_messages
             WHERE chat_id = ?1 AND role = 'assistant' AND ordinal > ?2
               AND (?3 IS NULL OR ordinal < ?3)
             ORDER BY ordinal",
        )?;
        statement
            .query_map(params![chat_id, turn_ordinal, next_turn], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut reply_id = None;
    let mut reply_preview = None;
    for message_id in ids {
        let entry = read_entry(transaction, chat_id, &message_id)?;
        let preview = transcript_entry_preview(&entry, 200);
        if !preview.is_empty() {
            reply_id = Some(message_id);
            reply_preview = Some(preview);
            break;
        }
    }
    transaction.execute(
        "UPDATE session_turns SET reply_message_id = ?3, reply_preview = ?4
         WHERE chat_id = ?1 AND message_id = ?2",
        params![chat_id, turn_id, reply_id, reply_preview],
    )?;
    Ok(())
}

fn bump_projection_revision(
    transaction: &Transaction<'_>,
    chat_id: &str,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE session_sync
         SET projection_revision = projection_revision + 1,
             projection_change_revision = projection_change_revision + 1,
             projection_dirty = 1
         WHERE chat_id = ?1",
        [chat_id],
    )?;
    Ok(())
}

fn bump_chat(transaction: &Transaction<'_>, chat_id: &str) -> Result<i64, StoreError> {
    transaction.execute(
        "UPDATE session_chats
         SET revision = revision + 1, updated_at = ?2
         WHERE chat_id = ?1",
        params![chat_id, now_ms()],
    )?;
    transaction
        .query_row(
            "SELECT revision FROM session_chats WHERE chat_id = ?1",
            [chat_id],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn transcript_manifest(conn: &Connection, chat_id: &str) -> Result<TranscriptManifest, StoreError> {
    let mut statement = conn.prepare(
        "SELECT page_id, first_message_ordinal, message_count, estimated_bytes,
                content_hash, page_revision
         FROM session_pages WHERE chat_id = ?1 ORDER BY page_ordinal",
    )?;
    let raw_pages = statement
        .query_map([chat_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut pages = Vec::with_capacity(raw_pages.len());
    for (index, row) in raw_pages.iter().enumerate() {
        let (page_id, first_ordinal, message_count, estimated_bytes, cached_hash, cached_revision) =
            row;
        let (content_hash, page_revision) = match (cached_hash, cached_revision) {
            (Some(hash), Some(revision)) => (hash.clone(), revision.clone()),
            _ => {
                let page = transcript_page(conn, chat_id, page_id)?.ok_or_else(|| {
                    StoreError::Session(format!("session page {page_id} disappeared while reading"))
                })?;
                let hash = page_content_hash(&page)?;
                conn.execute(
                    "UPDATE session_pages SET content_hash = ?3, page_revision = ?4
                     WHERE chat_id = ?1 AND page_id = ?2",
                    params![chat_id, page_id, hash, page.revision],
                )?;
                (hash, page.revision)
            }
        };
        pages.push(TranscriptPageDescriptor {
            id: page_id.clone(),
            revision: page_revision,
            content_hash: Some(content_hash),
            first_ordinal: usize_from_sql(*first_ordinal)?,
            message_count: usize_from_sql(*message_count)?,
            estimated_bytes: usize_from_sql(*estimated_bytes)?,
            previous_page_id: index
                .checked_sub(1)
                .map(|previous| raw_pages[previous].0.clone()),
            next_page_id: raw_pages.get(index + 1).map(|next| next.0.clone()),
            live: index + 1 == raw_pages.len(),
        });
    }
    let turns = {
        let mut statement = conn.prepare(
            "SELECT message_id, ordinal, page_id, prompt_preview, reply_preview
             FROM session_turns WHERE chat_id = ?1 ORDER BY ordinal",
        )?;
        statement
            .query_map([chat_id], |row| {
                Ok(TranscriptTurnDescriptor {
                    message_id: row.get(0)?,
                    ordinal: usize_from_sql(row.get(1)?)?,
                    page_id: row.get(2)?,
                    prompt_preview: row.get(3)?,
                    reply_preview: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let total_messages = pages
        .last()
        .map_or(0, |page| page.first_ordinal + page.message_count);
    Ok(TranscriptManifest {
        catalog_revision: transcript_catalog_revision(&pages, &turns),
        total_messages,
        pages,
        turns,
    })
}

fn transcript_page(
    conn: &Connection,
    chat_id: &str,
    page_id: &str,
) -> Result<Option<TranscriptPage>, StoreError> {
    let first_ordinal = conn
        .query_row(
            "SELECT first_message_ordinal FROM session_pages
             WHERE chat_id = ?1 AND page_id = ?2",
            params![chat_id, page_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(first_ordinal) = first_ordinal else {
        return Ok(None);
    };
    let ids = message_ids(conn, chat_id, Some(page_id))?;
    let messages = ids
        .into_iter()
        .map(|id| read_entry(conn, chat_id, &id))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(TranscriptPage {
        id: page_id.to_string(),
        revision: transcript_page_revision(&messages),
        first_ordinal: usize_from_sql(first_ordinal)?,
        messages,
    }))
}

fn read_entry(
    conn: &Connection,
    chat_id: &str,
    message_id: &str,
) -> Result<SessionMessageEntry, StoreError> {
    let (role, created_at, device_id, status): (String, i64, String, Option<String>) = conn
        .query_row(
            "SELECT role, created_at, device_id, status
             FROM session_messages WHERE chat_id = ?1 AND message_id = ?2",
            params![chat_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let mut statement = conn.prepare(
        "SELECT part_id, kind, payload_json, text_base
         FROM session_parts
         WHERE chat_id = ?1 AND message_id = ?2 ORDER BY part_ordinal",
    )?;
    let rows = statement
        .query_map(params![chat_id, message_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut parts = Vec::with_capacity(rows.len());
    for (part_id, kind, payload, mut text) in rows {
        if kind == "text" {
            let mut chunks = conn.prepare(
                "SELECT text FROM session_text_chunks
                 WHERE chat_id = ?1 AND message_id = ?2 AND part_id = ?3
                 ORDER BY chunk_ordinal",
            )?;
            for chunk in chunks.query_map(params![chat_id, message_id, part_id], |row| {
                row.get::<_, String>(0)
            })? {
                text.get_or_insert_with(String::new).push_str(&chunk?);
            }
        }
        parts.push(decode_part(&kind, part_id, payload, text)?);
    }
    Ok(SessionMessageEntry {
        id: message_id.to_string(),
        role: parse_message_role(&role)?,
        parts,
        created_at,
        device_id,
        status: status.as_deref().map(parse_message_status).transpose()?,
        continuation_of: None,
    })
}

fn message_ids(
    conn: &Connection,
    chat_id: &str,
    page_id: Option<&str>,
) -> Result<Vec<String>, StoreError> {
    if let Some(page_id) = page_id {
        let mut statement = conn.prepare(
            "SELECT message_id FROM session_messages
             WHERE chat_id = ?1 AND page_id = ?2 ORDER BY ordinal",
        )?;
        return statement
            .query_map(params![chat_id, page_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from);
    }
    let mut statement = conn
        .prepare("SELECT message_id FROM session_messages WHERE chat_id = ?1 ORDER BY ordinal")?;
    statement
        .query_map([chat_id], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn archive_terminal_command_deliveries(
    transaction: &Transaction<'_>,
    chat_id: &str,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE session_commands SET delivery_state = 'archived'
         WHERE chat_id = ?1 AND status != 'pending' AND delivery_state = 'local'",
        [chat_id],
    )?;
    Ok(())
}

fn insert_command(
    transaction: &Transaction<'_>,
    chat_id: &str,
    entry: &SessionCommandEntry,
) -> Result<bool, StoreError> {
    let existing = transaction
        .query_row(
            "SELECT payload_json, issued_by, issued_at, based_on_json,
                    expires_at, status, resolution
             FROM session_commands WHERE chat_id = ?1 AND command_id = ?2",
            params![chat_id, entry.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    let payload = serde_json::to_string(&entry.payload)?;
    let based_on = entry
        .based_on
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    if let Some((
        stored_payload,
        issued_by,
        issued_at,
        stored_based_on,
        expires_at,
        status,
        resolution,
    )) = existing
    {
        let same = stored_payload == payload
            && issued_by == entry.issued_by
            && issued_at == entry.issued_at
            && stored_based_on == based_on
            && expires_at == entry.expires_at
            && status == command_status(entry.status)
            && resolution == entry.resolution;
        if same {
            return Ok(false);
        }
        return Err(StoreError::Session(format!(
            "command {} was reinserted with different contents",
            entry.id
        )));
    }
    let ordinal: i64 = transaction.query_row(
        "SELECT next_command_ordinal FROM session_chats WHERE chat_id = ?1",
        [chat_id],
        |row| row.get(0),
    )?;
    let revision = bump_chat(transaction, chat_id)?;
    transaction.execute(
        "INSERT INTO session_commands (
            chat_id, command_id, command_ordinal, edge_seq, payload_json,
            issued_by, issued_at, based_on_json, expires_at, status, resolution,
            delivery_state, claim_token, revision
         ) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'local', NULL, ?11)",
        params![
            chat_id,
            entry.id,
            ordinal,
            payload,
            entry.issued_by,
            entry.issued_at,
            based_on,
            entry.expires_at,
            command_status(entry.status),
            entry.resolution,
            revision
        ],
    )?;
    transaction.execute(
        "UPDATE session_chats SET next_command_ordinal = next_command_ordinal + 1
         WHERE chat_id = ?1",
        [chat_id],
    )?;
    Ok(true)
}

fn read_commands(conn: &Connection, chat_id: &str) -> Result<Vec<SessionCommandEntry>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT command_id, payload_json, issued_by, issued_at,
                based_on_json, expires_at, status, resolution
         FROM session_commands WHERE chat_id = ?1 ORDER BY command_ordinal",
    )?;
    let rows = statement.query_map([chat_id], decode_command_row)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn decode_command_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionCommandEntry> {
    let command_id: String = row.get(0)?;
    let payload: String = row.get(1)?;
    let issued_by: String = row.get(2)?;
    let issued_at: i64 = row.get(3)?;
    let based_on: Option<String> = row.get(4)?;
    let expires_at: Option<i64> = row.get(5)?;
    let status: String = row.get(6)?;
    let resolution: Option<String> = row.get(7)?;
    let payload = serde_json::from_str(&payload).map_err(json_sql_error)?;
    let based_on = based_on
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(json_sql_error)?;
    let status = parse_command_status(&status).map_err(session_sql_error)?;
    Ok(SessionCommandEntry {
        id: command_id,
        payload,
        issued_by,
        issued_at,
        based_on,
        expires_at,
        status,
        resolution,
    })
}

fn semantic_hash(
    messages: &[SessionMessageEntry],
    commands: &[SessionCommandEntry],
) -> Result<String, StoreError> {
    Ok(digest_hex(&serde_json::to_vec(&(messages, commands))?))
}

fn page_content_hash(page: &TranscriptPage) -> Result<String, StoreError> {
    Ok(digest_hex(&serde_json::to_vec(page)?))
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn message_role(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
    }
}

fn parse_message_role(value: &str) -> Result<MessageRole, StoreError> {
    match value {
        "user" => Ok(MessageRole::User),
        "assistant" => Ok(MessageRole::Assistant),
        "system" => Ok(MessageRole::System),
        other => Err(StoreError::Session(format!("unknown message role {other}"))),
    }
}

fn message_status(status: MessageStatus) -> &'static str {
    match status {
        MessageStatus::Streaming => "streaming",
        MessageStatus::Complete => "complete",
        MessageStatus::Aborted => "aborted",
    }
}

fn parse_message_status(value: &str) -> Result<MessageStatus, StoreError> {
    match value {
        "streaming" => Ok(MessageStatus::Streaming),
        "complete" => Ok(MessageStatus::Complete),
        "aborted" => Ok(MessageStatus::Aborted),
        other => Err(StoreError::Session(format!(
            "unknown message status {other}"
        ))),
    }
}

fn command_status(status: SessionCommandStatus) -> &'static str {
    match status {
        SessionCommandStatus::Pending => "pending",
        SessionCommandStatus::Applied => "applied",
        SessionCommandStatus::Rejected => "rejected",
        SessionCommandStatus::Expired => "expired",
        SessionCommandStatus::Superseded => "superseded",
        SessionCommandStatus::Cancelled => "cancelled",
    }
}

fn parse_command_status(value: &str) -> Result<SessionCommandStatus, StoreError> {
    match value {
        "pending" => Ok(SessionCommandStatus::Pending),
        "applied" => Ok(SessionCommandStatus::Applied),
        "rejected" => Ok(SessionCommandStatus::Rejected),
        "expired" => Ok(SessionCommandStatus::Expired),
        "superseded" => Ok(SessionCommandStatus::Superseded),
        "cancelled" => Ok(SessionCommandStatus::Cancelled),
        other => Err(StoreError::Session(format!(
            "unknown command status {other}"
        ))),
    }
}

fn sql_usize(value: usize) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Session(format!("value {value} exceeds SQLite integer range")))
}

fn usize_from_sql(value: i64) -> Result<usize, rusqlite::Error> {
    usize::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn u64_from_sql(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value)
        .map_err(|_| StoreError::Session(format!("negative SQLite revision {value}")))
}

fn sql_u64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value)
        .map_err(|_| StoreError::Session(format!("revision {value} exceeds SQLite integer range")))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, StoreError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn json_sql_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn session_sql_error(error: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use jolt_session_doc::{
        MessagePart, MessageRole, MessageStatus, SessionCommandPayload, SessionCommandStatus,
        SessionMessageEntry,
    };

    use super::*;

    fn entry(id: &str, role: MessageRole, text: &str) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.to_string(),
            role,
            parts: vec![MessagePart::Text {
                id: "t0".to_string(),
                text: text.to_string(),
            }],
            created_at: 1,
            device_id: "device-a".to_string(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        }
    }

    fn command(id: &str) -> SessionCommandEntry {
        SessionCommandEntry {
            id: id.to_string(),
            payload: SessionCommandPayload::Interrupt {},
            issued_by: "device-b".to_string(),
            issued_at: 2,
            based_on: None,
            expires_at: Some(100),
            status: SessionCommandStatus::Pending,
            resolution: None,
        }
    }

    #[test]
    fn current_state_round_trip_and_streaming_append() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let session = store.open_session("chat-1").unwrap();
        session
            .push_message(&entry("u1", MessageRole::User, "hello"))
            .unwrap();
        session.begin_assistant("a1", "device-a", 2).unwrap();
        session
            .sync_assistant(
                "a1",
                &[MessagePart::Text {
                    id: "t0".into(),
                    text: "hel".into(),
                }],
            )
            .unwrap();
        session
            .sync_assistant(
                "a1",
                &[
                    MessagePart::Text {
                        id: "t0".into(),
                        text: "hello world".into(),
                    },
                    MessagePart::TextReveal { id: "r1".into() },
                ],
            )
            .unwrap();
        session
            .finish_assistant(
                "a1",
                &[
                    MessagePart::Text {
                        id: "t0".into(),
                        text: "hello world".into(),
                    },
                    MessagePart::TextReveal { id: "r1".into() },
                ],
                MessageStatus::Complete,
            )
            .unwrap();

        let entries = session.read_entries().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[1].parts[0],
            MessagePart::Text { text, .. } if text == "hello world"
        ));
        assert_eq!(entries[1].status, Some(MessageStatus::Complete));
    }

    #[test]
    fn projection_acknowledgement_clears_only_the_published_revision() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let session = store.open_session("chat-publish").unwrap();
        session
            .push_message(&entry("message-1", MessageRole::User, "one"))
            .unwrap();
        let published = session.projection_change_revision().unwrap();
        assert_eq!(
            store.unpublished_hub_session_ids().unwrap(),
            vec!["chat-publish"]
        );
        session.mark_hub_projection_published(published).unwrap();
        assert!(session.hub_seeded().unwrap());
        assert!(store.unpublished_hub_session_ids().unwrap().is_empty());
        session.queue_command(&command("command-1")).unwrap();
        assert!(store.unpublished_hub_session_ids().unwrap().is_empty());

        session
            .push_message(&entry("message-2", MessageRole::User, "two"))
            .unwrap();
        session.mark_hub_projection_published(published).unwrap();
        assert_eq!(
            store.unpublished_hub_session_ids().unwrap(),
            vec!["chat-publish"]
        );
    }

    #[test]
    fn sealed_page_publication_marks_persist_and_invalidate_on_edit() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let session = store.open_session("chat-pages").unwrap();
        for index in 0..=TRANSCRIPT_PAGE_MESSAGE_COUNT {
            session
                .push_message(&entry(
                    &format!("message-{index}"),
                    MessageRole::User,
                    "body",
                ))
                .unwrap();
        }
        let manifest = session.transcript_manifest().unwrap();
        let sealed = manifest.pages.iter().find(|page| !page.live).unwrap();
        let hash = sealed.content_hash.as_deref().unwrap();
        assert!(!session.page_is_published(&sealed.id, hash).unwrap());
        assert!(session.mark_page_published(&sealed.id, hash).unwrap());
        assert!(session.page_is_published(&sealed.id, hash).unwrap());

        session
            .replace_text_part("message-0", "t0", "changed")
            .unwrap();
        assert!(!session.page_is_published(&sealed.id, hash).unwrap());
    }

    #[test]
    fn commands_are_idempotent_and_mutable_only_by_status() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let session = store.open_session("chat-1").unwrap();
        let command = command("c1");
        assert!(session.queue_command(&command).unwrap());
        assert!(!session.queue_command(&command).unwrap());
        assert_eq!(
            session.commands_pending_hub_submission().unwrap(),
            vec![command.clone()]
        );
        assert!(
            session
                .set_command_status("c1", SessionCommandStatus::Applied, Some("done"))
                .unwrap()
        );
        let commands = session.read_commands().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            session.read_command("c1").unwrap(),
            Some(commands[0].clone())
        );
        assert_eq!(session.read_command("missing").unwrap(), None);
        assert_eq!(session.command_cursor().unwrap(), 0);
        session.set_command_cursor(7).unwrap();
        session.set_command_cursor(3).unwrap();
        assert_eq!(session.command_cursor().unwrap(), 7);
        assert_eq!(commands[0].status, SessionCommandStatus::Applied);
        assert_eq!(commands[0].resolution.as_deref(), Some("done"));
        assert!(session.mark_command_hub_submitted("c1").unwrap());
        assert!(!session.mark_command_hub_submitted("c1").unwrap());
        assert!(
            session
                .commands_pending_hub_submission()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn imported_terminal_commands_are_historical_not_hub_outbox_entries() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let mut terminal = command("terminal");
        terminal.status = SessionCommandStatus::Applied;
        let session = store
            .import_session_state("chat-import", &[], &[terminal])
            .unwrap();

        assert!(
            session
                .commands_pending_hub_submission()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn projection_cache_migration_backfills_existing_rows() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("docs.sqlite3");
        let expected = {
            let store = Arc::new(DocsStore::open(directory.path()).unwrap());
            let session = store.open_session("chat-v1").unwrap();
            session
                .push_message(&entry("user-1", MessageRole::User, "prompt"))
                .unwrap();
            session
                .push_message(&entry("assistant-1", MessageRole::Assistant, "reply"))
                .unwrap();
            session.transcript_manifest().unwrap()
        };
        let connection = Connection::open(&database).unwrap();
        connection.execute("DROP TABLE session_turns", []).unwrap();
        connection
            .execute("ALTER TABLE session_pages DROP COLUMN content_hash", [])
            .unwrap();
        connection
            .execute("ALTER TABLE session_pages DROP COLUMN page_revision", [])
            .unwrap();
        connection
            .execute(
                "DELETE FROM session_store_migrations WHERE name = ?1",
                [SESSION_PROJECTION_CACHE_MIGRATION],
            )
            .unwrap();
        drop(connection);

        let store = Arc::new(DocsStore::open(directory.path()).unwrap());
        let actual = store
            .open_session("chat-v1")
            .unwrap()
            .transcript_manifest()
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn legacy_cleanup_preserves_only_the_registry_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let store = DocsStore::open(directory.path()).unwrap();
        store.save_snapshot("chat-old", b"session").unwrap();
        store.save_snapshot("registry1", b"registry").unwrap();
        drop(store);

        let connection = Connection::open(directory.path().join("docs.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE legacy_session_imports (
                    chat_id TEXT PRIMARY KEY,
                    semantic_hash TEXT NOT NULL,
                    message_count INTEGER NOT NULL,
                    command_count INTEGER NOT NULL,
                    imported_at INTEGER NOT NULL
                 ) STRICT;
                 DELETE FROM session_store_migrations
                 WHERE name = 'session-legacy-cleanup-v4';",
            )
            .unwrap();
        drop(connection);

        let store = DocsStore::open(directory.path()).unwrap();
        assert_eq!(store.load_snapshot("chat-old").unwrap(), None);
        assert_eq!(
            store.load_snapshot("registry1").unwrap().as_deref(),
            Some(b"registry".as_slice())
        );
        let connection = Connection::open(directory.path().join("docs.sqlite3")).unwrap();
        assert!(!table_exists(&connection, "legacy_session_imports").unwrap());
    }
}

//! Device-local persistence for pending review feedback.
//!
//! Review drafts deliberately live outside Loro and the edge. The reviewing
//! device keeps one JSON payload per logical review subject in SQLite so target
//! adapters can evolve their typed anchor schemas without a table migration.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use jolt_proto::ReviewDraft;
use rusqlite::{Connection, OptionalExtension as _, params};

#[derive(Clone)]
pub struct ReviewStore {
    connection: Arc<Mutex<Connection>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn encode(draft: &ReviewDraft) -> rusqlite::Result<String> {
    serde_json::to_string(draft)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn decode(payload: String) -> rusqlite::Result<ReviewDraft> {
    serde_json::from_str(&payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            payload.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

impl ReviewStore {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS review_drafts (
                 review_key TEXT PRIMARY KEY,
                 review_id TEXT NOT NULL,
                 chat_id TEXT NOT NULL,
                 target_kind TEXT NOT NULL,
                 snapshot_id TEXT NOT NULL,
                 payload_json TEXT NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS review_drafts_chat
                 ON review_drafts(chat_id, updated_at_ms);",
        )?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn get(&self, review_key: &str) -> rusqlite::Result<Option<ReviewDraft>> {
        let payload = lock(&self.connection)
            .query_row(
                "SELECT payload_json FROM review_drafts WHERE review_key = ?1",
                [review_key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        payload.map(decode).transpose()
    }

    pub fn put(&self, draft: &ReviewDraft) -> rusqlite::Result<()> {
        let payload = encode(draft)?;
        let target_kind = match &draft.target {
            jolt_proto::ReviewDraftTarget::Diff { .. } => "diff",
            jolt_proto::ReviewDraftTarget::AssistantMessage { .. } => "assistantMessage",
        };
        lock(&self.connection).execute(
            "INSERT INTO review_drafts (
                 review_key, review_id, chat_id, target_kind, snapshot_id,
                 payload_json, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(review_key) DO UPDATE SET
                 review_id = excluded.review_id,
                 chat_id = excluded.chat_id,
                 target_kind = excluded.target_kind,
                 snapshot_id = excluded.snapshot_id,
                 payload_json = excluded.payload_json,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                draft.review_key,
                draft.review_id,
                draft.destination_chat_id,
                target_kind,
                draft.snapshot_id,
                payload,
                draft.updated_at.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    pub fn delete(&self, review_key: &str) -> rusqlite::Result<()> {
        lock(&self.connection).execute(
            "DELETE FROM review_drafts WHERE review_key = ?1",
            [review_key],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use jolt_proto::{
        AssistantMessageReviewSnapshot, AssistantMessageReviewSubject, REVIEW_DRAFT_SCHEMA_VERSION,
        ReviewDraftTarget,
    };

    use super::*;

    fn draft() -> ReviewDraft {
        let now = Utc::now();
        ReviewDraft {
            schema_version: REVIEW_DRAFT_SCHEMA_VERSION,
            review_id: "review-1".into(),
            review_key: "chat:message".into(),
            destination_chat_id: "chat".into(),
            snapshot_id: "revision".into(),
            target: ReviewDraftTarget::AssistantMessage {
                subject: AssistantMessageReviewSubject {
                    chat_id: "chat".into(),
                    root_message_id: "message".into(),
                },
                snapshot: AssistantMessageReviewSnapshot {
                    revision: "revision".into(),
                    text_parts: Vec::new(),
                },
                comments: Vec::new(),
            },
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn drafts_round_trip_and_delete() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReviewStore::open(&temp.path().join("reviews.sqlite")).unwrap();
        let draft = draft();
        store.put(&draft).unwrap();
        assert_eq!(store.get(&draft.review_key).unwrap(), Some(draft.clone()));
        store.delete(&draft.review_key).unwrap();
        assert_eq!(store.get(&draft.review_key).unwrap(), None);
    }
}

//! jolt-store — local SQLite document snapshots and the processed-command ledger.

mod docs;
mod sessions;

pub use docs::{DocsStore, StoreError};
pub use sessions::{
    LegacyImportReport, LegacySessionMigration, StoredSegmentWriter, StoredSession,
};

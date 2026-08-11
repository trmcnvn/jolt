//! Local SQLite registry, session, and processed-command storage.

mod docs;
mod sessions;

pub use docs::{DocsStore, StoreError};
pub use sessions::{StoredSegmentWriter, StoredSession};

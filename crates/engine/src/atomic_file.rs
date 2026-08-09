//! Small, shared helpers for immutable on-disk artifacts.

use std::path::Path;

use crate::EngineError;

/// Atomically installs `bytes` when `path` is absent.
///
/// Immutable artifact writers are deliberately idempotent: an existing equal
/// payload succeeds, while an existing different payload is treated as store
/// corruption instead of being replaced in-place. Besides preserving the
/// content-addressed contract, this avoids platform-specific overwrite
/// semantics for `rename` (notably on Windows).
pub(crate) async fn write_immutable(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    match tokio::fs::read(path).await {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {
            return Err(EngineError::Other(format!(
                "immutable artifact already exists with different contents: {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let parent = path
        .parent()
        .ok_or_else(|| EngineError::Other("immutable artifact path has no parent".into()))?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, bytes).await?;
    match tokio::fs::rename(&temporary, path).await {
        Ok(()) => Ok(()),
        Err(rename_error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            // Another writer may have won the race. Equal bytes are the same
            // immutable artifact and therefore a successful outcome.
            match tokio::fs::read(path).await {
                Ok(existing) if existing == bytes => Ok(()),
                _ => Err(rename_error.into()),
            }
        }
    }
}

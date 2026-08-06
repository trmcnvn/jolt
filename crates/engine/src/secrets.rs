//! Device-local harness secrets backed by the operating system credential store.
//!
//! Only labels, environment-variable names, and harness scopes are persisted in
//! Jolt's data directory. Values stay in macOS Keychain, Windows Credential
//! Manager, or the freedesktop Secret Service through `keyring`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use jolt_harness::HarnessError;
use jolt_harness::environment::HarnessEnvironmentProvider;
use jolt_proto::{HarnessId, HarnessSecret, HarnessSecretsSnapshot};
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "dev.trmcnvn.jolt.secrets";
const METADATA_FILE: &str = "harness-secrets.json";
const MAX_SECRET_BYTES: usize = 2048;

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("secret metadata: {0}")]
    Metadata(String),
    #[error("secure storage: {0}")]
    Storage(String),
    #[error("invalid secret: {0}")]
    Invalid(String),
}

trait SecretBackend: Send + Sync {
    fn status(&self) -> Result<(), String>;
    fn get(&self, id: &str) -> Result<String, String>;
    fn set(&self, id: &str, value: &str) -> Result<(), String>;
    fn delete(&self, id: &str) -> Result<(), String>;
}

#[derive(Default)]
struct NativeBackend {
    /// Windows Credential Manager does not reliably sequence concurrent
    /// operations against the same entry. Keep all native calls serialized,
    /// including work that outlives a cancelled `spawn_blocking` join.
    operations: std::sync::Mutex<()>,
}

impl NativeBackend {
    fn entry(id: &str) -> Result<keyring::v1::Entry, String> {
        #[cfg(windows)]
        {
            use std::collections::HashMap;

            keyring::v1::Entry::store_status()
                .as_ref()
                .map_err(ToString::to_string)?;
            let inner = keyring_core::Entry::new_with_modifiers(
                KEYRING_SERVICE,
                id,
                &HashMap::from([("persistence", "Local")]),
            )
            .map_err(|error| error.to_string())?;
            Ok(keyring::v1::Entry { inner })
        }
        #[cfg(not(windows))]
        {
            keyring::v1::Entry::new(KEYRING_SERVICE, id).map_err(|error| error.to_string())
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SecretBackend for NativeBackend {
    fn status(&self) -> Result<(), String> {
        let _operation = self.lock();
        keyring::v1::Entry::store_status()
            .as_ref()
            .copied()
            .map_err(ToString::to_string)
    }

    fn get(&self, id: &str) -> Result<String, String> {
        let _operation = self.lock();
        Self::entry(id)?
            .get_password()
            .map_err(|error| error.to_string())
    }

    fn set(&self, id: &str, value: &str) -> Result<(), String> {
        let _operation = self.lock();
        Self::entry(id)?
            .set_password(value)
            .map_err(|error| error.to_string())
    }

    fn delete(&self, id: &str) -> Result<(), String> {
        let _operation = self.lock();
        match Self::entry(id)?.delete_credential() {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default)]
    secrets: Vec<HarnessSecret>,
}

struct Inner {
    path: PathBuf,
    metadata: RwLock<Metadata>,
    backend: Arc<dyn SecretBackend>,
    operations: tokio::sync::Mutex<()>,
}

/// Cloneable device-local secret service used by RPC and harness launchers.
#[derive(Clone)]
pub struct HarnessSecrets {
    inner: Arc<Inner>,
}

impl HarnessSecrets {
    pub fn open(data_dir: &Path) -> Result<Self, SecretsError> {
        Self::open_with_backend(data_dir, Arc::new(NativeBackend::default()))
    }

    fn open_with_backend(
        data_dir: &Path,
        backend: Arc<dyn SecretBackend>,
    ) -> Result<Self, SecretsError> {
        let path = data_dir.join(METADATA_FILE);
        let metadata = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|error| SecretsError::Metadata(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Metadata::default(),
            Err(error) => return Err(SecretsError::Metadata(error.to_string())),
        };
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                metadata: RwLock::new(metadata),
                backend,
                operations: tokio::sync::Mutex::new(()),
            }),
        })
    }

    pub async fn snapshot(&self) -> HarnessSecretsSnapshot {
        let _operation = self.inner.operations.lock().await;
        let backend = self.inner.backend.clone();
        let status = tokio::task::spawn_blocking(move || backend.status()).await;
        let storage_error = match status {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(format!("credential-store task failed: {error}")),
        };
        HarnessSecretsSnapshot {
            secrets: self
                .inner
                .metadata
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .secrets
                .clone(),
            storage_available: storage_error.is_none(),
            storage_error,
        }
    }

    pub async fn upsert(
        &self,
        id: Option<&str>,
        label: &str,
        environment_variable: &str,
        harnesses: Vec<HarnessId>,
        value: Option<&str>,
    ) -> Result<HarnessSecretsSnapshot, SecretsError> {
        let label = validate_label(label)?;
        let environment_variable = validate_environment_variable(environment_variable)?;
        let harnesses = validate_harnesses(harnesses)?;
        if let Some(value) = value {
            validate_value(value)?;
        }

        let _operation = self.inner.operations.lock().await;
        let current = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let secret_id = id.map(str::to_owned).unwrap_or_else(crate::new_id);
        let existing = current.secrets.iter().find(|secret| secret.id == secret_id);
        if id.is_some() && existing.is_none() {
            return Err(SecretsError::Invalid("secret no longer exists".into()));
        }
        if existing.is_none() && value.is_none() {
            return Err(SecretsError::Invalid(
                "a value is required for a new secret".into(),
            ));
        }
        let requested_harnesses: HashSet<_> = harnesses.iter().copied().collect();
        if current.secrets.iter().any(|secret| {
            secret.id != secret_id
                && secret
                    .environment_variable
                    .eq_ignore_ascii_case(&environment_variable)
                && secret
                    .harnesses
                    .iter()
                    .any(|harness| requested_harnesses.contains(harness))
        }) {
            return Err(SecretsError::Invalid(format!(
                "{environment_variable} is already assigned to one of these harnesses"
            )));
        }

        let mut next = current.clone();
        let replacement = HarnessSecret {
            id: secret_id.clone(),
            label,
            environment_variable,
            harnesses,
        };
        if let Some(position) = next
            .secrets
            .iter()
            .position(|secret| secret.id == secret_id)
        {
            next.secrets[position] = replacement;
        } else {
            next.secrets.push(replacement);
        }

        let backend = self.inner.backend.clone();
        let id_for_backend = secret_id.clone();
        let value = value.map(str::to_owned);
        let previous_value = if value.is_some() && existing.is_some() {
            let backend = backend.clone();
            let id = id_for_backend.clone();
            Some(
                tokio::task::spawn_blocking(move || backend.get(&id))
                    .await
                    .map_err(|error| SecretsError::Storage(error.to_string()))?
                    .map_err(SecretsError::Storage)?,
            )
        } else {
            None
        };
        if let Some(value) = &value {
            let backend = backend.clone();
            let id = id_for_backend.clone();
            let value = value.clone();
            tokio::task::spawn_blocking(move || backend.set(&id, &value))
                .await
                .map_err(|error| SecretsError::Storage(error.to_string()))?
                .map_err(SecretsError::Storage)?;
        }

        if let Err(error) = save_metadata(&self.inner.path, &next) {
            if value.is_some() {
                let backend = backend.clone();
                let id = id_for_backend.clone();
                let rollback = previous_value;
                let _ = tokio::task::spawn_blocking(move || match rollback {
                    Some(previous) => backend.set(&id, &previous),
                    None => backend.delete(&id),
                })
                .await;
            }
            return Err(error);
        }
        *self
            .inner
            .metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        drop(_operation);
        Ok(self.snapshot().await)
    }

    pub async fn delete(&self, id: &str) -> Result<HarnessSecretsSnapshot, SecretsError> {
        let _operation = self.inner.operations.lock().await;
        let current = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if !current.secrets.iter().any(|secret| secret.id == id) {
            return Err(SecretsError::Invalid("secret no longer exists".into()));
        }
        let backend = self.inner.backend.clone();
        let id = id.to_owned();
        let previous = {
            let backend = backend.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || backend.get(&id))
                .await
                .map_err(|error| SecretsError::Storage(error.to_string()))?
                .map_err(SecretsError::Storage)?
        };
        {
            let backend = backend.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || backend.delete(&id))
                .await
                .map_err(|error| SecretsError::Storage(error.to_string()))?
                .map_err(SecretsError::Storage)?;
        }
        let mut next = current;
        next.secrets.retain(|secret| secret.id != id);
        if let Err(error) = save_metadata(&self.inner.path, &next) {
            let backend = backend.clone();
            let rollback_id = id.clone();
            let _ = tokio::task::spawn_blocking(move || backend.set(&rollback_id, &previous)).await;
            return Err(error);
        }
        *self
            .inner
            .metadata
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = next;
        drop(_operation);
        Ok(self.snapshot().await)
    }

    async fn environment_for(
        &self,
        harness: HarnessId,
    ) -> Result<Vec<(String, String)>, SecretsError> {
        let _operation = self.inner.operations.lock().await;
        let metadata: Vec<_> = self
            .inner
            .metadata
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .secrets
            .iter()
            .filter(|secret| secret.harnesses.contains(&harness))
            .cloned()
            .collect();
        let backend = self.inner.backend.clone();
        tokio::task::spawn_blocking(move || {
            metadata
                .into_iter()
                .map(|secret| {
                    backend
                        .get(&secret.id)
                        .map(|value| (secret.environment_variable, value))
                        .map_err(|error| {
                            SecretsError::Storage(format!("{}: {error}", secret.label))
                        })
                })
                .collect()
        })
        .await
        .map_err(|error| SecretsError::Storage(error.to_string()))?
    }
}

#[async_trait]
impl HarnessEnvironmentProvider for HarnessSecrets {
    async fn environment(&self, harness: HarnessId) -> Result<Vec<(String, String)>, HarnessError> {
        self.environment_for(harness)
            .await
            .map_err(|error| HarnessError::Environment(error.to_string()))
    }
}

fn validate_label(label: &str) -> Result<String, SecretsError> {
    let label = label.trim();
    if label.is_empty() {
        return Err(SecretsError::Invalid("label must not be empty".into()));
    }
    if label.chars().count() > 100 {
        return Err(SecretsError::Invalid(
            "label must not exceed 100 characters".into(),
        ));
    }
    Ok(label.to_owned())
}

fn validate_environment_variable(value: &str) -> Result<String, SecretsError> {
    let value = value.trim();
    let mut chars = value.chars();
    if !chars
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(SecretsError::Invalid(
            "environment variable must use letters, numbers, and underscores and cannot start with a number"
                .into(),
        ));
    }
    Ok(value.to_owned())
}

fn validate_harnesses(harnesses: Vec<HarnessId>) -> Result<Vec<HarnessId>, SecretsError> {
    let requested: HashSet<_> = harnesses.into_iter().collect();
    let supported = [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Pi];
    if requested.iter().any(|harness| !supported.contains(harness)) {
        return Err(SecretsError::Invalid(
            "selected harness is not supported".into(),
        ));
    }
    let harnesses: Vec<_> = supported
        .into_iter()
        .filter(|harness| requested.contains(harness))
        .collect();
    if harnesses.is_empty() {
        return Err(SecretsError::Invalid("select at least one harness".into()));
    }
    Ok(harnesses)
}

fn validate_value(value: &str) -> Result<(), SecretsError> {
    if value.is_empty() {
        return Err(SecretsError::Invalid("value must not be empty".into()));
    }
    if value.contains('\0') {
        return Err(SecretsError::Invalid("value must not contain NUL".into()));
    }
    if value.len() > MAX_SECRET_BYTES {
        return Err(SecretsError::Invalid(format!(
            "value must not exceed {MAX_SECRET_BYTES} bytes"
        )));
    }
    Ok(())
}

fn save_metadata(path: &Path, metadata: &Metadata) -> Result<(), SecretsError> {
    let bytes = serde_json::to_vec_pretty(metadata)
        .map_err(|error| SecretsError::Metadata(error.to_string()))?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|error| SecretsError::Metadata(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MemoryBackend(Mutex<HashMap<String, String>>);

    impl SecretBackend for MemoryBackend {
        fn status(&self) -> Result<(), String> {
            Ok(())
        }

        fn get(&self, id: &str) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| "missing".into())
        }

        fn set(&self, id: &str, value: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(id.to_owned(), value.to_owned());
            Ok(())
        }

        fn delete(&self, id: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn stores_only_metadata_and_scopes_environment() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(MemoryBackend::default());
        let secrets = HarnessSecrets::open_with_backend(dir.path(), backend.clone()).unwrap();
        let snapshot = secrets
            .upsert(
                None,
                "Executor",
                "EXECUTOR_TOKEN",
                vec![HarnessId::Pi],
                Some("top-secret"),
            )
            .await
            .unwrap();
        assert_eq!(snapshot.secrets.len(), 1);
        assert_eq!(
            secrets.environment_for(HarnessId::Pi).await.unwrap(),
            vec![("EXECUTOR_TOKEN".into(), "top-secret".into())]
        );
        assert!(
            secrets
                .environment_for(HarnessId::ClaudeCode)
                .await
                .unwrap()
                .is_empty()
        );
        let metadata = std::fs::read_to_string(dir.path().join(METADATA_FILE)).unwrap();
        assert!(!metadata.contains("top-secret"));

        let snapshot = secrets.delete(&snapshot.secrets[0].id).await.unwrap();
        assert!(snapshot.secrets.is_empty());
        assert!(backend.0.lock().unwrap().is_empty());
    }

    #[test]
    fn rejects_unsupported_harnesses() {
        let error = validate_harnesses(vec![HarnessId::Mock]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("selected harness is not supported")
        );
    }

    #[tokio::test]
    async fn rejects_overlapping_environment_variables() {
        let dir = tempfile::tempdir().unwrap();
        let secrets =
            HarnessSecrets::open_with_backend(dir.path(), Arc::new(MemoryBackend::default()))
                .unwrap();
        secrets
            .upsert(None, "First", "TOKEN", vec![HarnessId::Pi], Some("one"))
            .await
            .unwrap();
        let error = secrets
            .upsert(
                None,
                "Second",
                "TOKEN",
                vec![HarnessId::Pi, HarnessId::Codex],
                Some("two"),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already assigned"));
    }
}

//! Device-local environment variables injected into harness child processes.
//!
//! The source is an indirection shared by the registry's lazy harnesses. The
//! engine installs its secure-store provider after opening the data directory;
//! already-created harnesses and future lazy instances observe the same source.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use jolt_proto::HarnessId;

use crate::HarnessError;

/// Resolves secret environment variables for one harness launch.
#[async_trait]
pub trait HarnessEnvironmentProvider: Send + Sync {
    async fn environment(&self, harness: HarnessId) -> Result<Vec<(String, String)>, HarnessError>;
}

struct EmptyEnvironment;

#[async_trait]
impl HarnessEnvironmentProvider for EmptyEnvironment {
    async fn environment(
        &self,
        _harness: HarnessId,
    ) -> Result<Vec<(String, String)>, HarnessError> {
        Ok(Vec::new())
    }
}

/// Mutable provider handle shared by all production harness adapters.
#[derive(Clone)]
pub struct HarnessEnvironment {
    provider: Arc<RwLock<Arc<dyn HarnessEnvironmentProvider>>>,
}

impl Default for HarnessEnvironment {
    fn default() -> Self {
        Self {
            provider: Arc::new(RwLock::new(Arc::new(EmptyEnvironment))),
        }
    }
}

impl HarnessEnvironment {
    pub fn set_provider(&self, provider: Arc<dyn HarnessEnvironmentProvider>) {
        *self
            .provider
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = provider;
    }

    pub async fn resolve(&self, harness: HarnessId) -> Result<Vec<(String, String)>, HarnessError> {
        let provider = self
            .provider
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        provider.environment(harness).await
    }
}

#[doc(hidden)]
pub fn apply(command: &mut tokio::process::Command, environment: &[(String, String)]) {
    command.envs(environment.iter().map(|(key, value)| (key, value)));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedEnvironment;

    #[async_trait]
    impl HarnessEnvironmentProvider for FixedEnvironment {
        async fn environment(
            &self,
            harness: HarnessId,
        ) -> Result<Vec<(String, String)>, HarnessError> {
            if harness == HarnessId::Pi {
                Ok(vec![("JOLT_TEST_SECRET".into(), "available".into())])
            } else {
                Ok(Vec::new())
            }
        }
    }

    #[tokio::test]
    async fn provider_can_be_installed_after_source_creation() {
        let environment = HarnessEnvironment::default();
        assert!(environment.resolve(HarnessId::Pi).await.unwrap().is_empty());
        environment.set_provider(Arc::new(FixedEnvironment));
        assert_eq!(
            environment.resolve(HarnessId::Pi).await.unwrap(),
            vec![("JOLT_TEST_SECRET".into(), "available".into())]
        );
        assert!(
            environment
                .resolve(HarnessId::Codex)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolved_values_reach_only_the_child_environment() {
        let environment = HarnessEnvironment::default();
        environment.set_provider(Arc::new(FixedEnvironment));
        let values = environment.resolve(HarnessId::Pi).await.unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "printf %s \"$JOLT_TEST_SECRET\""]);
        apply(&mut command, &values);
        let output = command.output().await.unwrap();
        assert_eq!(output.stdout, b"available");
        assert!(std::env::var_os("JOLT_TEST_SECRET").is_none());
    }
}

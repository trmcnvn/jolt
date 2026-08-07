//! Runtime-selectable command-line VCS backends.
//!
//! Exactly one backend is active on each device. Executables are discovered
//! from the explicit Jolt overrides first, then the process and login-shell
//! PATHs. A missing saved selection falls back in priority order: Jujutsu,
//! then Git.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use jolt_proto::{VcsBackendStatus, VcsKind, VcsSettingsSnapshot};
use serde::{Deserialize, Serialize};

use crate::EngineError;

const SETTINGS_FILE: &str = "vcs-settings.json";
const MIN_JJ_VERSION: (u32, u32) = (0, 43);

#[derive(Debug, Clone)]
pub struct VcsCommand {
    pub kind: VcsKind,
    pub executable: PathBuf,
    pub version: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StoredSettings {
    selected: Option<VcsKind>,
}

struct VcsInner {
    data_dir: PathBuf,
    selected: Mutex<Option<VcsKind>>,
    commands: Mutex<Vec<VcsCommand>>,
}

#[derive(Clone)]
pub struct Vcs {
    inner: std::sync::Arc<VcsInner>,
}

impl Vcs {
    pub fn new(data_dir: &Path) -> Self {
        let selected = std::fs::read_to_string(data_dir.join(SETTINGS_FILE))
            .ok()
            .and_then(|raw| serde_json::from_str::<StoredSettings>(&raw).ok())
            .and_then(|settings| settings.selected);
        let vcs = Self {
            inner: std::sync::Arc::new(VcsInner {
                data_dir: data_dir.to_path_buf(),
                selected: Mutex::new(selected),
                commands: Mutex::new(discovered_commands()),
            }),
        };
        // Heal a missing/unavailable saved choice immediately so headless use
        // gets the same JJ -> Git initial selection as the settings page.
        let _ = vcs.active_command();
        vcs
    }

    fn selected(&self) -> MutexGuard<'_, Option<VcsKind>> {
        self.inner
            .selected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn active_kind(&self) -> Option<VcsKind> {
        self.active_command().map(|command| command.kind)
    }

    pub fn active_command(&self) -> Option<VcsCommand> {
        let mut commands = self
            .inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if commands
            .iter()
            .any(|command| !executable_file(&command.executable))
        {
            *commands = discovered_commands();
        }
        let available: Vec<_> = commands
            .iter()
            .filter(|command| command_supported(command))
            .cloned()
            .collect();
        let mut selected = self.selected();
        let active = selected
            .and_then(|kind| {
                available
                    .iter()
                    .find(|command| command.kind == kind)
                    .cloned()
            })
            .or_else(|| available.first().cloned());
        let healed = active.as_ref().map(|command| command.kind);
        if *selected != healed {
            *selected = healed;
            drop(selected);
            if let Err(err) = self.save(healed) {
                tracing::warn!(error = %err, "failed to persist VCS fallback");
            }
        }
        active
    }

    pub fn snapshot(&self) -> VcsSettingsSnapshot {
        let commands = discovered_commands();
        *self
            .inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = commands.clone();
        let active = self.active_kind();
        VcsSettingsSnapshot {
            selected: active,
            backends: [VcsKind::Jujutsu, VcsKind::Git]
                .into_iter()
                .map(|kind| {
                    let command = commands.iter().find(|command| command.kind == kind);
                    VcsBackendStatus {
                        kind,
                        available: command.is_some_and(command_supported),
                        selected: active == Some(kind),
                        executable: command
                            .map(|command| command.executable.to_string_lossy().into()),
                        version: command.and_then(|command| command.version.clone()),
                    }
                })
                .collect(),
        }
    }

    pub fn set_selected(&self, kind: VcsKind) -> Result<VcsSettingsSnapshot, EngineError> {
        let commands = discovered_commands();
        let available = commands
            .iter()
            .filter(|command| command_supported(command))
            .any(|command| command.kind == kind);
        *self
            .inner
            .commands
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = commands;
        if !available {
            return Err(EngineError::Other(format!(
                "{} executable is unavailable or unsupported",
                kind.label()
            )));
        }
        *self.selected() = Some(kind);
        self.save(Some(kind))?;
        Ok(self.snapshot())
    }

    fn save(&self, selected: Option<VcsKind>) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.inner.data_dir)?;
        let path = self.inner.data_dir.join(SETTINGS_FILE);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&StoredSettings { selected })
            .map_err(|err| EngineError::Other(format!("VCS settings serialize: {err}")))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

fn discovered_commands() -> Vec<VcsCommand> {
    [VcsKind::Jujutsu, VcsKind::Git]
        .into_iter()
        .filter_map(|kind| {
            let executable = resolve_executable(kind)?;
            let version = command_version(kind, &executable);
            Some(VcsCommand {
                kind,
                executable,
                version,
            })
        })
        .collect()
}

fn command_supported(command: &VcsCommand) -> bool {
    match command.kind {
        VcsKind::Git => command.version.is_some(),
        VcsKind::Jujutsu => command
            .version
            .as_deref()
            .and_then(parse_jj_version)
            .is_some_and(|version| version >= MIN_JJ_VERSION),
    }
}

fn command_version(_kind: VcsKind, executable: &Path) -> Option<String> {
    let output = std::process::Command::new(executable)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn parse_jj_version(value: &str) -> Option<(u32, u32)> {
    let version = value
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn resolve_executable(kind: VcsKind) -> Option<PathBuf> {
    let (name, override_name) = match kind {
        VcsKind::Git => ("git", "JOLT_GIT_EXECUTABLE"),
        VcsKind::Jujutsu => ("jj", "JOLT_JJ_EXECUTABLE"),
    };
    resolve_auxiliary_executable(name, override_name)
}

/// Resolve a companion CLI with the same GUI-safe PATH policy as Git/JJ.
pub(crate) fn resolve_auxiliary_executable(name: &str, override_name: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(override_name).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        return executable_file(&path).then_some(path);
    }

    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(path) = jolt_harness::shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(path));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        paths.push(home.join(".local/bin"));
        paths.push(home.join(".cargo/bin"));
        paths.push(home.join(".local/share/mise/shims"));
    }
    paths.push(PathBuf::from("/opt/homebrew/bin"));
    paths.push(PathBuf::from("/usr/local/bin"));
    paths.push(PathBuf::from("/usr/bin"));

    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .map(|path| path.join(name))
        .find(|path| executable_file(path))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn compose_command_path(command: &mut tokio::process::Command, executable: &Path) {
    let mut paths = Vec::new();
    if let Some(parent) = executable
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        paths.push(parent.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Some(path) = jolt_harness::shell_env::login_shell_path() {
        paths.extend(std::env::split_paths(path));
    }
    let mut seen = HashSet::new();
    paths.retain(|path| !path.as_os_str().is_empty() && seen.insert(path.clone()));
    if let Ok(path) = std::env::join_paths(paths) {
        command.env("PATH", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_jj_versions() {
        assert_eq!(parse_jj_version("jj 0.43.0"), Some((0, 43)));
        assert_eq!(parse_jj_version("jj 1.2.3-extra"), Some((1, 2)));
        assert_eq!(parse_jj_version("no version"), None);
    }
}

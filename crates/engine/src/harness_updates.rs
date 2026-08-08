//! Device-local harness release checks and user-approved updates.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, bail};
use jolt_proto::{HarnessId, HarnessUpdateState, HarnessUpdateStatus};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;
use tokio::sync::watch;

use crate::registry::HarnessRegistry;

const INITIAL_DELAY: Duration = Duration::from_secs(30);
const CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const RETIRE_POLL: Duration = Duration::from_millis(100);

pub type HarnessRunCounts = Arc<dyn Fn(HarnessId) -> (usize, usize) + Send + Sync>;
pub type HarnessFence = Arc<dyn Fn(HarnessId, bool) + Send + Sync>;
pub type RetireIdleHarness = Arc<dyn Fn(HarnessId) -> usize + Send + Sync>;
pub type WakeHarnessCommands = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct HarnessUpdater {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<HarnessRegistry>,
    statuses: watch::Sender<Vec<HarnessUpdateStatus>>,
    operations: Mutex<HashSet<HarnessId>>,
    unavailable: Mutex<HashSet<HarnessId>>,
    counts: HarnessRunCounts,
    fence: HarnessFence,
    retire_idle: RetireIdleHarness,
    wake_commands: WakeHarnessCommands,
    checker: Mutex<Option<tokio::task::AbortHandle>>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl HarnessUpdater {
    pub fn spawn(
        registry: Arc<HarnessRegistry>,
        counts: HarnessRunCounts,
        fence: HarnessFence,
        retire_idle: RetireIdleHarness,
        wake_commands: WakeHarnessCommands,
    ) -> Self {
        let (statuses, _) = watch::channel(
            harnesses()
                .into_iter()
                .map(initial_status)
                .collect::<Vec<_>>(),
        );
        let updater = Self {
            inner: Arc::new(Inner {
                registry,
                statuses,
                operations: Mutex::new(HashSet::new()),
                unavailable: Mutex::new(HashSet::new()),
                counts,
                fence,
                retire_idle,
                wake_commands,
                checker: Mutex::new(None),
                tasks: Mutex::new(Vec::new()),
            }),
        };
        let checker = updater.clone();
        let task = tokio::spawn(async move { checker.check_loop().await });
        *lock(&updater.inner.checker) = Some(task.abort_handle());
        updater
    }

    pub fn watch(&self) -> watch::Receiver<Vec<HarnessUpdateStatus>> {
        self.inner.statuses.subscribe()
    }

    pub fn active_maintenance(&self) -> Vec<HarnessId> {
        let mut active = lock(&self.inner.operations).clone();
        active.extend(lock(&self.inner.unavailable).iter().copied());
        active.into_iter().collect()
    }

    pub fn check_now(&self) {
        let updater = self.clone();
        let task = tokio::spawn(async move {
            updater.check_all().await;
        });
        let mut tasks = lock(&self.inner.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
    }

    /// Start a user-approved update. Completion is reported through [`Self::watch`].
    pub fn apply(&self, harness: HarnessId) -> anyhow::Result<()> {
        if !harnesses().contains(&harness) {
            bail!("{harness:?} does not support updates");
        }
        if !lock(&self.inner.operations).insert(harness) {
            bail!("{harness:?} update is already in progress");
        }
        let updater = self.clone();
        let operation_inner = self.inner.clone();
        let task = tokio::spawn(async move {
            let _operation = OperationGuard {
                harness,
                inner: operation_inner,
            };
            updater.apply_inner(harness).await;
        });
        let mut tasks = lock(&self.inner.tasks);
        tasks.retain(|task| !task.is_finished());
        tasks.push(task.abort_handle());
        Ok(())
    }

    pub fn shutdown(&self) {
        if let Some(task) = lock(&self.inner.checker).take() {
            task.abort();
        }
        for task in lock(&self.inner.tasks).drain(..) {
            task.abort();
        }
    }

    async fn check_loop(&self) {
        tokio::time::sleep(INITIAL_DELAY).await;
        loop {
            let healthy = self.check_all().await;
            tokio::time::sleep(if healthy {
                CHECK_INTERVAL
            } else {
                RETRY_INTERVAL
            })
            .await;
        }
    }

    async fn check_all(&self) -> bool {
        let mut healthy = true;
        for harness in harnesses() {
            if lock(&self.inner.operations).contains(&harness) {
                continue;
            }
            if lock(&self.inner.unavailable).contains(&harness)
                && self.executable_usable(harness).await
            {
                lock(&self.inner.unavailable).remove(&harness);
                (self.inner.fence)(harness, false);
                (self.inner.wake_commands)();
            }
            self.modify(harness, |status| {
                status.state = HarnessUpdateState::Checking;
                status.detail = None;
            });
            let result = self.probe(harness).await;
            if lock(&self.inner.operations).contains(&harness) {
                continue;
            }
            match result {
                Ok(status) => self.replace(status),
                Err(error) => {
                    healthy = false;
                    self.modify(harness, |status| {
                        status.state = HarnessUpdateState::Failed;
                        status.checked_at = Some(crate::now_ms());
                        status.detail = Some(format!("{error:#}"));
                    });
                }
            }
        }
        healthy
    }

    async fn apply_inner(&self, harness: HarnessId) {
        let current = self.status(harness);
        if !current.can_apply {
            self.modify(harness, |status| {
                status.state = HarnessUpdateState::Manual;
                status
                    .detail
                    .get_or_insert_with(|| "Update this installation manually".into());
            });
            return;
        }

        (self.inner.fence)(harness, true);
        let mut fence = FenceGuard {
            harness,
            fence: self.inner.fence.clone(),
            wake_commands: self.inner.wake_commands.clone(),
            retained: false,
        };

        loop {
            (self.inner.retire_idle)(harness);
            let (busy, idle) = (self.inner.counts)(harness);
            if busy == 0 && idle == 0 {
                break;
            }
            self.modify(harness, |status| {
                status.state = HarnessUpdateState::WaitingForIdle;
                status.detail = Some(if busy == 0 {
                    format!("Retiring {idle} idle process{}", plural(idle))
                } else {
                    format!(
                        "Waiting for {busy} active process{}; {idle} idle process{} can retire now",
                        plural(busy),
                        plural(idle)
                    )
                });
            });
            tokio::time::sleep(RETIRE_POLL).await;
        }

        self.modify(harness, |status| {
            status.state = HarnessUpdateState::Updating;
            status.detail = Some("Installing the update".into());
        });
        let previous = self.status(harness).current_version;
        tracing::info!(?harness, ?previous, "starting user-approved harness update");
        let result = async {
            let output = self.run_update(harness).await?;
            let status = self.probe(harness).await.with_context(|| {
                format!(
                    "post-update verification failed{}",
                    output.diagnostic_suffix()
                )
            })?;
            if matches!(
                status.state,
                HarnessUpdateState::NotInstalled
                    | HarnessUpdateState::UpdateAvailable
                    | HarnessUpdateState::Manual
            ) {
                bail!(
                    "verification still reports {}{}",
                    status.current_version.as_deref().unwrap_or("no executable"),
                    output.diagnostic_suffix()
                );
            }
            Ok::<_, anyhow::Error>(status)
        }
        .await;
        match result {
            Ok(mut status) => {
                lock(&self.inner.unavailable).remove(&harness);
                status.state = HarnessUpdateState::Updated;
                status.detail = Some(match (&previous, &status.current_version) {
                    (Some(previous), Some(current)) if previous != current => {
                        format!("Updated from {previous} to {current}")
                    }
                    (_, Some(current)) => format!("Harness is up to date ({current})"),
                    _ => "Harness update completed".into(),
                });
                tracing::info!(
                    ?harness,
                    current_version = ?status.current_version,
                    "harness update verified"
                );
                self.replace(status);
            }
            Err(error) => {
                let usable = self.executable_usable(harness).await;
                if usable {
                    lock(&self.inner.unavailable).remove(&harness);
                } else {
                    lock(&self.inner.unavailable).insert(harness);
                    fence.retain();
                }
                self.fail(
                    harness,
                    if usable {
                        format!("{error:#}")
                    } else {
                        format!(
                            "{error:#}. The executable is no longer usable; pending {harness:?} work remains paused until it is repaired"
                        )
                    },
                );
            }
        }
    }

    async fn executable_usable(&self, harness: HarnessId) -> bool {
        let Ok(adapter) = self.inner.registry.resolve(harness) else {
            return false;
        };
        let Ok(executable) = adapter.executable_path() else {
            return false;
        };
        installed_version(&executable).await.is_ok()
    }

    async fn probe(&self, harness_id: HarnessId) -> anyhow::Result<HarnessUpdateStatus> {
        let harness = self
            .inner
            .registry
            .resolve(harness_id)
            .map_err(anyhow::Error::msg)?;
        let executable = match harness.executable_path() {
            Ok(executable) => executable,
            Err(jolt_harness::HarnessError::NotInstalled(_)) => {
                return Ok(HarnessUpdateStatus {
                    harness: harness_id,
                    state: HarnessUpdateState::NotInstalled,
                    current_version: None,
                    latest_version: None,
                    can_apply: false,
                    checked_at: Some(crate::now_ms()),
                    detail: None,
                });
            }
            Err(error) => return Err(anyhow::Error::msg(error.to_string())),
        };
        let current_version = installed_version(&executable).await?;
        let latest_version = latest_version(harness_id, &executable).await?;
        let can_apply = can_apply(harness_id, &executable);
        let newer = jolt_update::version_newer(&latest_version, &current_version);
        Ok(HarnessUpdateStatus {
            harness: harness_id,
            state: if newer {
                if can_apply {
                    HarnessUpdateState::UpdateAvailable
                } else {
                    HarnessUpdateState::Manual
                }
            } else {
                HarnessUpdateState::UpToDate
            },
            current_version: Some(current_version),
            latest_version: Some(latest_version),
            can_apply: newer && can_apply,
            checked_at: Some(crate::now_ms()),
            detail: (newer && !can_apply).then(|| manual_instruction(harness_id, &executable)),
        })
    }

    async fn run_update(&self, harness_id: HarnessId) -> anyhow::Result<UpdateCommandOutput> {
        let harness = self
            .inner
            .registry
            .resolve(harness_id)
            .map_err(anyhow::Error::msg)?;
        let executable = harness
            .executable_path()
            .map_err(|error| anyhow::Error::msg(error.to_string()))?;
        let homebrew_cask = homebrew_cask(&executable);
        let mut output = UpdateCommandOutput::default();
        if homebrew_cask.is_some() {
            // `brew upgrade` can skip auto-update based on its general update
            // interval even when its cask API cache is stale. Refresh metadata
            // explicitly inside the user-approved update operation.
            let (status, refresh_output) = run_updater_command(
                harness_id,
                Path::new("brew"),
                &["update", "--auto-update"],
                &executable,
                true,
            )
            .await
            .context("refreshing Homebrew metadata")?;
            output.append(refresh_output);
            if !status.success() {
                bail!(
                    "Homebrew metadata refresh failed ({status}): {}",
                    output.diagnostic()
                );
            }
        }

        let (program, args): (PathBuf, Vec<&str>) = match (harness_id, homebrew_cask) {
            (HarnessId::ClaudeCode, Some(cask)) => {
                (PathBuf::from("brew"), vec!["upgrade", "--cask", cask])
            }
            (HarnessId::ClaudeCode, None) => (executable.clone(), vec!["update"]),
            (HarnessId::Codex, Some(cask)) => {
                (PathBuf::from("brew"), vec!["upgrade", "--cask", cask])
            }
            (HarnessId::Codex, None) => (executable.clone(), vec!["update"]),
            (HarnessId::Pi, _) => (executable.clone(), vec!["update", "--self"]),
            (HarnessId::Mock, _) => bail!("Mock does not support updates"),
        };
        let (status, command_output) = run_updater_command(
            harness_id,
            &program,
            &args,
            &executable,
            homebrew_cask.is_some(),
        )
        .await
        .with_context(|| format!("running {} updater", harness.display_name()))?;
        output.append(command_output);
        if !status.success() {
            let message = output.diagnostic();
            bail!(
                "{} update failed ({}): {}",
                harness.display_name(),
                status,
                if message.is_empty() {
                    "no error output"
                } else {
                    &message
                }
            );
        }
        Ok(output)
    }

    fn status(&self, harness: HarnessId) -> HarnessUpdateStatus {
        self.inner
            .statuses
            .borrow()
            .iter()
            .find(|status| status.harness == harness)
            .cloned()
            .unwrap_or_else(|| initial_status(harness))
    }

    fn replace(&self, next: HarnessUpdateStatus) {
        self.inner.statuses.send_modify(|statuses| {
            if let Some(status) = statuses
                .iter_mut()
                .find(|status| status.harness == next.harness)
            {
                *status = next;
            }
        });
    }

    fn modify(&self, harness: HarnessId, update: impl FnOnce(&mut HarnessUpdateStatus)) {
        self.inner.statuses.send_modify(|statuses| {
            if let Some(status) = statuses.iter_mut().find(|status| status.harness == harness) {
                update(status);
            }
        });
    }

    fn fail(&self, harness: HarnessId, message: String) {
        tracing::error!(?harness, error = %message, "harness update failed");
        self.modify(harness, |status| {
            status.state = HarnessUpdateState::Failed;
            status.detail = Some(message);
            status.checked_at = Some(crate::now_ms());
        });
    }
}

struct OperationGuard {
    harness: HarnessId,
    inner: Arc<Inner>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        lock(&self.inner.operations).remove(&self.harness);
    }
}

struct FenceGuard {
    harness: HarnessId,
    fence: HarnessFence,
    wake_commands: WakeHarnessCommands,
    retained: bool,
}

impl FenceGuard {
    fn retain(&mut self) {
        self.retained = true;
    }
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        if !self.retained {
            (self.fence)(self.harness, false);
            (self.wake_commands)();
        }
    }
}

fn harnesses() -> [HarnessId; 3] {
    [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Pi]
}

const UPDATE_OUTPUT_LIMIT: usize = 16 * 1024;
const UPDATE_DIAGNOSTIC_LIMIT: usize = 1_000;

#[derive(Default)]
struct UpdateCommandOutput {
    stdout: String,
    stderr: String,
}

impl UpdateCommandOutput {
    fn append(&mut self, output: Self) {
        append_output(&mut self.stdout, output.stdout);
        append_output(&mut self.stderr, output.stderr);
    }

    fn diagnostic(&self) -> String {
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => return String::new(),
            (false, true) => stdout.to_owned(),
            (true, false) => stderr.to_owned(),
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        tail_chars(&combined, UPDATE_DIAGNOSTIC_LIMIT)
    }

    fn diagnostic_suffix(&self) -> String {
        let diagnostic = self.diagnostic();
        if diagnostic.is_empty() {
            String::new()
        } else {
            format!(". Updater output: {diagnostic}")
        }
    }
}

fn append_output(destination: &mut String, output: String) {
    if output.trim().is_empty() {
        return;
    }
    if !destination.is_empty() {
        destination.push('\n');
    }
    destination.push_str(&output);
}

fn tail_chars(value: &str, limit: usize) -> String {
    if limit == 0 {
        return String::new();
    }
    let Some((start, first_retained)) = value.char_indices().rev().nth(limit) else {
        return value.to_owned();
    };
    let start = start + first_retained.len_utf8();
    format!("…{}", &value[start..])
}

async fn run_updater_command(
    harness: HarnessId,
    program: &Path,
    args: &[&str],
    executable: &Path,
    force_homebrew_refresh: bool,
) -> anyhow::Result<(std::process::ExitStatus, UpdateCommandOutput)> {
    tracing::info!(
        ?harness,
        program = %program.display(),
        arguments = ?args,
        "running harness updater command"
    );
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    if force_homebrew_refresh {
        command
            .env("HOMEBREW_FORCE_API_AUTO_UPDATE", "1")
            .env("HOMEBREW_AUTO_UPDATE_SECS", "0")
            .env("HOMEBREW_API_AUTO_UPDATE_SECS", "0");
    }
    jolt_platform::process::compose_child_path(&mut command, executable);
    let (status, stdout, stderr) = bounded_output(command, COMMAND_TIMEOUT).await?;
    tracing::info!(
        ?harness,
        %status,
        stdout = %stdout.trim(),
        stderr = %stderr.trim(),
        "harness updater command exited"
    );
    Ok((status, UpdateCommandOutput { stdout, stderr }))
}

async fn bounded_output(
    mut command: Command,
    timeout: Duration,
) -> anyhow::Result<(std::process::ExitStatus, String, String)> {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("updater stdout unavailable")?;
    let stderr = child.stderr.take().context("updater stderr unavailable")?;
    let (status, stdout, stderr) = tokio::time::timeout(timeout, async {
        tokio::join!(
            child.wait(),
            read_bounded(stdout, UPDATE_OUTPUT_LIMIT),
            read_bounded(stderr, UPDATE_OUTPUT_LIMIT)
        )
    })
    .await
    .context("command timed out")?;
    Ok((
        status?,
        String::from_utf8_lossy(&stdout?).into_owned(),
        String::from_utf8_lossy(&stderr?).into_owned(),
    ))
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> std::io::Result<Vec<u8>> {
    let mut retained = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(retained);
        }
        let remaining = limit.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

fn initial_status(harness: HarnessId) -> HarnessUpdateStatus {
    HarnessUpdateStatus {
        harness,
        state: HarnessUpdateState::Unknown,
        current_version: None,
        latest_version: None,
        can_apply: false,
        checked_at: None,
        detail: None,
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "es" }
}

async fn installed_version(executable: &Path) -> anyhow::Result<String> {
    let mut command = Command::new(executable);
    command.arg("--version").stdin(Stdio::null());
    jolt_platform::process::compose_child_path(&mut command, executable);
    let (status, stdout, stderr) = bounded_output(command, Duration::from_secs(10))
        .await
        .with_context(|| format!("running {} --version", executable.display()))?;
    if !status.success() {
        bail!("{} --version exited with {status}", executable.display());
    }
    let text = format!("{stdout} {stderr}");
    extract_version(&text).context("version command did not report a dotted numeric version")
}

fn extract_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '.'))
        .find(|token| {
            let mut parts = token.split('.');
            parts.clone().count() >= 2 && parts.all(|part| part.parse::<u64>().is_ok())
        })
        .map(str::to_owned)
}

async fn latest_version(harness: HarnessId, executable: &Path) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("jolt/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()?;
    match harness {
        HarnessId::ClaudeCode if let Some(cask) = homebrew_cask(executable) => {
            fetch_json::<VersionResponse>(
                &client,
                &format!("https://formulae.brew.sh/api/cask/{cask}.json"),
            )
            .await
            .map(|response| response.version)
        }
        HarnessId::ClaudeCode => client
            .get(format!(
                "https://downloads.claude.ai/claude-code-releases/{}",
                if homebrew_cask(executable) == Some("claude-code@latest") {
                    "latest"
                } else {
                    claude_release_channel()
                }
            ))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await
            .map(|version| version.trim().to_string())
            .map_err(Into::into),
        HarnessId::Codex if homebrew_cask(executable).is_some() => {
            fetch_json::<VersionResponse>(&client, "https://formulae.brew.sh/api/cask/codex.json")
                .await
                .map(|response| response.version)
        }
        HarnessId::Codex if path_contains(executable, "node_modules") => {
            fetch_json::<VersionResponse>(
                &client,
                "https://registry.npmjs.org/@openai%2Fcodex/latest",
            )
            .await
            .map(|response| response.version)
        }
        HarnessId::Codex => fetch_json::<ReleaseResponse>(
            &client,
            "https://api.github.com/repos/openai/codex/releases/latest",
        )
        .await
        .and_then(|response| {
            response
                .tag_name
                .strip_prefix("rust-v")
                .map(str::to_owned)
                .context("latest Codex release tag is not rust-v<version>")
        }),
        HarnessId::Pi => {
            fetch_json::<VersionResponse>(&client, "https://pi.dev/api/latest-version")
                .await
                .map(|response| response.version)
        }
        HarnessId::Mock => bail!("Mock does not publish updates"),
    }
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<T> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

#[derive(Deserialize)]
struct ClaudeSettings {
    #[serde(default, rename = "autoUpdatesChannel")]
    auto_updates_channel: Option<String>,
}

fn claude_release_channel() -> &'static str {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")));
    let channel = config_dir
        .and_then(|directory| std::fs::read(directory.join("settings.json")).ok())
        .and_then(|contents| serde_json::from_slice::<ClaudeSettings>(&contents).ok())
        .and_then(|settings| settings.auto_updates_channel);
    if channel.as_deref() == Some("stable") {
        "stable"
    } else {
        "latest"
    }
}

#[derive(Deserialize)]
struct VersionResponse {
    version: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
}

fn resolved_path_text(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_ascii_lowercase()
}

fn path_contains(path: &Path, needle: &str) -> bool {
    resolved_path_text(path).contains(&needle.to_ascii_lowercase())
}

fn homebrew_cask(executable: &Path) -> Option<&'static str> {
    let path = resolved_path_text(executable);
    if path.contains("/caskroom/claude-code@latest/") {
        Some("claude-code@latest")
    } else if path.contains("/caskroom/claude-code/") {
        Some("claude-code")
    } else if path.contains("/caskroom/codex/") {
        Some("codex")
    } else {
        None
    }
}

fn can_apply(harness: HarnessId, executable: &Path) -> bool {
    let overridden = match harness {
        HarnessId::ClaudeCode => std::env::var_os("CLAUDE_CODE_EXECUTABLE"),
        HarnessId::Codex => std::env::var_os("CODEX_EXECUTABLE"),
        HarnessId::Pi => std::env::var_os("JOLT_PI_EXECUTABLE"),
        HarnessId::Mock => return false,
    }
    .is_some_and(|value| !value.is_empty());
    if overridden {
        return false;
    }
    match harness {
        HarnessId::ClaudeCode => {
            homebrew_cask(executable).is_some()
                || path_contains(executable, ".local/share/claude")
                || path_contains(executable, "node_modules")
        }
        HarnessId::Codex | HarnessId::Pi => true,
        HarnessId::Mock => false,
    }
}

fn manual_instruction(harness: HarnessId, executable: &Path) -> String {
    format!(
        "Jolt cannot safely replace the configured {harness:?} executable at {}. Update it with the package manager or wrapper that installed it.",
        executable.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_provider_version_formats() {
        assert_eq!(
            extract_version("2.1.226 (Claude Code)"),
            Some("2.1.226".into())
        );
        assert_eq!(extract_version("codex-cli 0.147.0"), Some("0.147.0".into()));
        assert_eq!(extract_version("0.84.1\n"), Some("0.84.1".into()));
        assert_eq!(extract_version("nightly"), None);
    }

    #[test]
    fn update_diagnostics_keep_the_bounded_tail() {
        let output = UpdateCommandOutput {
            stdout: "prefix".into(),
            stderr: "failure detail".into(),
        };
        assert_eq!(output.diagnostic(), "prefix\nfailure detail");
        assert_eq!(tail_chars("abcdef", 3), "…def");
        assert_eq!(tail_chars("abc", 3), "abc");
    }

    #[tokio::test]
    async fn updater_output_is_drained_but_bounded() {
        let (mut writer, reader) = tokio::io::duplex(32 * 1024);
        let write = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt as _;
            writer.write_all(&vec![b'x'; 24 * 1024]).await.unwrap();
        });
        let retained = read_bounded(reader, UPDATE_OUTPUT_LIMIT).await.unwrap();
        write.await.unwrap();
        assert_eq!(retained.len(), UPDATE_OUTPUT_LIMIT);
    }

    #[test]
    fn detects_homebrew_casks() {
        assert_eq!(
            homebrew_cask(Path::new("/opt/homebrew/Caskroom/codex/0.147.0/bin/codex")),
            Some("codex")
        );
        assert_eq!(
            homebrew_cask(Path::new(
                "/opt/homebrew/Caskroom/claude-code@latest/2/bin/claude"
            )),
            Some("claude-code@latest")
        );
    }
}

//! jolt-update — release checking and self-update, shared by the engine (the
//! background checker + `ApplyUpdate`), the CLI (`jolt update`), and the UI
//! (the sidebar update strip + macOS bundle swap).
//!
//! Release layout (see `.github/workflows/release.yml` and `edge/src/install.sh`):
//! artifacts live in the `jolt-releases` R2 bucket, served pre-auth at
//! `{edge}/releases/*`. `manifest.json` carries the latest version and SHA-256
//! digest for every artifact.
//!
//! Install kinds and their update paths:
//! - **Managed** (`~/.jolt/app/<ver>` + `current` symlink — the curl|sh
//!   installer): download the headless tarball into a new versioned dir, flip
//!   the symlink, refresh Linux desktop integration, and restart the service.
//!   Same flow the installer script performs, natively.
//! - **MacApp** (running out of `Jolt.app`): download the app tarball, swap the
//!   bundle directory, relaunch. Driven by the UI.
//! - **Unmanaged** (source builds, hand-copied binaries): report only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::watch;

pub mod background_service;

/// The version compiled into this binary (the workspace version).
pub const fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Background check cadence.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
/// Retry sooner after a failed check (offline boot, transient edge error).
const CHECK_RETRY: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// First check waits out engine boot (room joins, doc re-sync).
const CHECK_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_secs(20);
/// While a user-approved apply waits behind active work, re-probe idleness.
const IDLE_RECHECK: std::time::Duration = std::time::Duration::from_secs(1);

// ---------------------------------------------------------------------------
// Release metadata
// ---------------------------------------------------------------------------

/// `{edge}/releases/manifest.json` — written by the release workflow.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// Artifact file name → verified metadata.
    pub files: BTreeMap<String, FileMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileMeta {
    pub sha256: String,
}

/// Artifact-name platform pair — `uname`-style strings matching the packaging
/// scripts: `linux-x86_64`, `linux-aarch64`, `macos-arm64`.
pub fn platform_key() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = match (os, std::env::consts::ARCH) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    (os, arch)
}

/// `jolt-<ver>-<os>-<arch>.tar.gz` — the headless/CLI tarball (Linux CI builds).
pub fn headless_artifact(version: &str) -> String {
    let (os, arch) = platform_key();
    format!("jolt-{version}-{os}-{arch}.tar.gz")
}

/// `jolt-<ver>-macos-<arch>-app.tar.gz` — the packaged `Jolt.app` bundle.
pub fn mac_app_artifact(version: &str) -> String {
    let (_, arch) = platform_key();
    format!("jolt-{version}-macos-{arch}-app.tar.gz")
}

/// Strictly-newer dotted-numeric compare (`0.1.10` > `0.1.9` > `0.1`).
/// Unparseable versions never count as newer.
pub fn version_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        let nums: Vec<u64> = v
            .trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (!nums.is_empty()).then_some(nums)
    }
    match (parts(latest), parts(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

/// Fetch and validate the newest release manifest.
pub async fn fetch_latest(edge_url: &str) -> anyhow::Result<Manifest> {
    let url = format!("{}/releases/manifest.json", edge_url.trim_end_matches('/'));
    let manifest: Manifest = http_client()?
        .get(&url)
        .send()
        .await
        .context("fetching manifest.json")?
        .error_for_status()
        .context("fetching manifest.json")?
        .json()
        .await
        .context("parsing manifest.json")?;
    if manifest.version.trim().is_empty() {
        bail!("manifest.json has an empty version");
    }
    Ok(manifest)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("jolt/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building http client")
}

// ---------------------------------------------------------------------------
// Install-kind detection
// ---------------------------------------------------------------------------

/// How this binary was installed — decides the update path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `~/.jolt/app/<ver>/jolt` behind the `current` symlink
    /// (curl|sh installer / a previous `jolt update`).
    Managed { app_root: PathBuf },
    /// Running out of a macOS `.app` bundle.
    MacApp { bundle: PathBuf },
    /// Source build or hand-copied binary — updates are report-only.
    Unmanaged,
}

pub fn detect_install() -> InstallKind {
    let Ok(exe) = std::env::current_exe() else {
        return InstallKind::Unmanaged;
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    detect_install_from(&exe, home.as_deref())
}

fn detect_install_from(exe: &Path, home: Option<&Path>) -> InstallKind {
    if let Some(home) = home {
        // `current_exe` resolves the `current` symlink to the versioned dir.
        let app_root = home.join(".jolt").join("app");
        if exe.starts_with(&app_root) {
            return InstallKind::Managed { app_root };
        }
    }
    for ancestor in exe.ancestors() {
        if ancestor.extension().is_some_and(|ext| ext == "app")
            && exe.starts_with(ancestor.join("Contents").join("MacOS"))
        {
            return InstallKind::MacApp {
                bundle: ancestor.to_path_buf(),
            };
        }
    }
    InstallKind::Unmanaged
}

// ---------------------------------------------------------------------------
// Linux desktop integration
// ---------------------------------------------------------------------------

/// Reconcile a managed Linux install's desktop entry and icon from the active
/// version. Running this at process startup makes the first boot after an
/// update repair installs created before desktop integration was supported.
pub fn refresh_linux_desktop_integration() -> anyhow::Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let InstallKind::Managed { app_root } = detect_install() else {
            return Ok(());
        };
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        let (applications_dir, integration_changed) =
            install_linux_desktop_integration(&app_root, &data_home)?;
        if integration_changed {
            match std::process::Command::new("update-desktop-database")
                .arg(&applications_dir)
                .status()
            {
                Ok(status) if !status.success() => tracing::debug!(
                    %status,
                    "update-desktop-database failed after refreshing Jolt launcher"
                ),
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => tracing::debug!(
                    error = %err,
                    "could not refresh desktop database after installing Jolt launcher"
                ),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
fn install_linux_desktop_integration(
    app_root: &Path,
    data_home: &Path,
) -> anyhow::Result<(PathBuf, bool)> {
    let current = app_root.join("current");
    let executable = current.join("jolt-desktop");
    if !executable.is_file() {
        bail!("{} is not a managed Jolt executable", executable.display());
    }
    let template_path = current.join("jolt.desktop");
    let template = std::fs::read_to_string(&template_path)
        .with_context(|| format!("reading {}", template_path.display()))?;
    let desktop = render_desktop_entry(&template, &executable)?;

    let icon_path = current.join("jolt.png");
    let icon_1024 =
        std::fs::read(&icon_path).with_context(|| format!("reading {}", icon_path.display()))?;
    let icon_512_path = current.join("jolt-512.png");
    let icon_512 = match std::fs::read(&icon_512_path) {
        Ok(icon) => icon,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => icon_1024.clone(),
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", icon_512_path.display()));
        }
    };
    let applications_dir = data_home.join("applications");
    let icon_512_dir = data_home.join("icons/hicolor/512x512/apps");
    let icon_1024_dir = data_home.join("icons/hicolor/1024x1024/apps");
    std::fs::create_dir_all(&applications_dir)
        .with_context(|| format!("creating {}", applications_dir.display()))?;
    for icon_dir in [&icon_512_dir, &icon_1024_dir] {
        std::fs::create_dir_all(icon_dir)
            .with_context(|| format!("creating {}", icon_dir.display()))?;
    }
    let icon_512_changed = atomic_write_if_changed(&icon_512_dir.join("jolt.png"), &icon_512)?;
    let icon_1024_changed = atomic_write_if_changed(&icon_1024_dir.join("jolt.png"), &icon_1024)?;
    let desktop_changed =
        atomic_write_if_changed(&applications_dir.join("jolt.desktop"), desktop.as_bytes())?;
    Ok((
        applications_dir,
        icon_512_changed || icon_1024_changed || desktop_changed,
    ))
}

#[cfg(any(target_os = "linux", test))]
fn render_desktop_entry(template: &str, executable: &Path) -> anyhow::Result<String> {
    let executable = executable.to_str().with_context(|| {
        format!(
            "desktop executable path is not UTF-8: {}",
            executable.display()
        )
    })?;
    if executable.contains(['\n', '\r']) {
        bail!("desktop executable path contains a line break");
    }
    let exec = quote_desktop_exec(executable);
    let try_exec = escape_desktop_string(executable);
    let mut saw_exec = false;
    let mut saw_try_exec = false;
    let mut output = String::with_capacity(template.len() + executable.len() * 2);
    for line in template.lines() {
        if line.starts_with("Exec=") {
            output.push_str("Exec=");
            output.push_str(&exec);
            saw_exec = true;
        } else if line.starts_with("TryExec=") {
            output.push_str("TryExec=");
            output.push_str(&try_exec);
            saw_try_exec = true;
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    if !saw_exec || !saw_try_exec {
        bail!("jolt.desktop is missing Exec or TryExec");
    }
    Ok(output)
}

#[cfg(any(target_os = "linux", test))]
fn quote_desktop_exec(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '%' => output.push_str("%%"),
            '\\' | '"' | '`' | '$' => {
                output.push('\\');
                output.push(ch);
            }
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

#[cfg(any(target_os = "linux", test))]
fn escape_desktop_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output
}

#[cfg(any(target_os = "linux", test))]
fn atomic_write_if_changed(path: &Path, contents: &[u8]) -> anyhow::Result<bool> {
    match std::fs::read(path) {
        Ok(existing) if existing == contents => return Ok(false),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    }
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("{} has no file name", path.display()))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}", std::process::id()));
    let result = (|| {
        std::fs::write(&temporary, contents)
            .with_context(|| format!("writing {}", temporary.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))
                .with_context(|| format!("setting permissions on {}", temporary.display()))?;
        }
        std::fs::rename(&temporary, path)
            .with_context(|| format!("installing {}", path.display()))?;
        Ok(true)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

// ---------------------------------------------------------------------------
// Download + verify
// ---------------------------------------------------------------------------

/// Stream `{edge}/releases/<file>` to `dest`, verifying the manifest SHA-256.
/// Writes through a `.partial` sidecar so an interrupted download never
/// leaves a plausible-looking artifact behind.
pub async fn download_release_file(
    edge_url: &str,
    manifest: &Manifest,
    file: &str,
    dest: &Path,
) -> anyhow::Result<()> {
    let url = format!("{}/releases/{file}", edge_url.trim_end_matches('/'));
    let expected = &manifest
        .files
        .get(file)
        .with_context(|| format!("manifest has no metadata for {file}"))?
        .sha256;
    let partial = dest.with_extension("partial");
    let resp = http_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut out = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("creating {}", partial.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        hasher.update(&chunk);
        out.write_all(&chunk).await.context("writing download")?;
    }
    out.flush().await.ok();
    drop(out);
    let actual = hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        tokio::fs::remove_file(&partial).await.ok();
        bail!("checksum mismatch for {file}: expected {expected}, got {actual}");
    }
    tokio::fs::rename(&partial, dest)
        .await
        .with_context(|| format!("moving {} into place", dest.display()))?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Managed (symlink) installs — the daemon/VPS path
// ---------------------------------------------------------------------------

/// Download + unpack the headless tarball into `app_root/<ver>` (idempotent —
/// an already-staged version is reused). Returns the versioned dir.
pub async fn stage_headless(
    edge_url: &str,
    manifest: &Manifest,
    app_root: &Path,
) -> anyhow::Result<PathBuf> {
    let version = &manifest.version;
    let dest = app_root.join(version);
    if dest.join("jolt").exists() {
        return Ok(dest);
    }
    let file = headless_artifact(version);
    let stage = app_root.join(format!(".stage-{version}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).with_context(|| format!("creating {}", stage.display()))?;
    let result = async {
        let tarball = stage.join(&file);
        download_release_file(edge_url, manifest, &file, &tarball).await?;
        let unpacked = stage.join("unpacked");
        std::fs::create_dir_all(&unpacked)?;
        // Tarball root is the versioned stage dir (see scripts/package-linux.sh);
        // strip it exactly as install.sh does.
        run(
            "tar",
            &[
                "-xzf",
                &tarball.to_string_lossy(),
                "-C",
                &unpacked.to_string_lossy(),
                "--strip-components=1",
            ],
        )?;
        if !unpacked.join("jolt").is_file() {
            bail!("tarball {file} did not contain a jolt binary");
        }
        match std::fs::rename(&unpacked, &dest) {
            Ok(()) => {}
            // Lost a race with another stager — the staged copy is equivalent.
            Err(_) if dest.join("jolt").exists() => {}
            Err(err) => {
                return Err(err).with_context(|| format!("moving {} into place", dest.display()));
            }
        }
        Ok(dest.clone())
    }
    .await;
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Atomically repoint `app_root/current` at `app_root/<ver>` (symlink to a temp
/// name, then rename over — never a window with no `current`).
pub fn apply_headless(app_root: &Path, version: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let target = app_root.join(version);
        if !target.join("jolt").exists() {
            bail!("{} is not a staged install", target.display());
        }
        let tmp = app_root.join(format!(".current-{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&target, &tmp).context("creating current symlink")?;
        std::fs::rename(&tmp, app_root.join("current")).context("swapping current symlink")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (app_root, version);
        bail!("managed installs are unix-only");
    }
}

/// Restart the installed engine service (the same units `jolt daemon` and the
/// curl|sh installer manage). Called after a symlink swap so the running daemon
/// picks up the new binary.
pub fn restart_service() -> anyhow::Result<()> {
    if cfg!(target_os = "macos") {
        let output = std::process::Command::new("id").arg("-u").output()?;
        let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
        run(
            "launchctl",
            &["kickstart", "-k", &format!("gui/{uid}/dev.trmcnvn.jolt")],
        )
    } else {
        run("systemctl", &["--user", "restart", "jolt.service"])
    }
}

// ---------------------------------------------------------------------------
// macOS app-bundle installs — the desktop path
// ---------------------------------------------------------------------------

/// Download + unpack the app tarball into `{data_dir}/updates/<ver>/Jolt.app`
/// (idempotent). Returns the staged bundle path.
pub async fn stage_mac_app(
    edge_url: &str,
    manifest: &Manifest,
    data_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let version = &manifest.version;
    let dir = data_dir.join("updates").join(version);
    let staged = dir.join("Jolt.app");
    if staged.join("Contents/MacOS/Jolt").exists() {
        return Ok(staged);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = mac_app_artifact(version);
    let tarball = dir.join(&file);
    download_release_file(edge_url, manifest, &file, &tarball).await?;
    run(
        "tar",
        &[
            "-xzf",
            &tarball.to_string_lossy(),
            "-C",
            &dir.to_string_lossy(),
        ],
    )?;
    std::fs::remove_file(&tarball).ok();
    if !staged.join("Contents/MacOS/Jolt").exists() {
        bail!("app tarball {file} did not contain Jolt.app");
    }
    Ok(staged)
}

/// Swap the installed bundle for the staged one: `ditto` the staged copy next to
/// the target (metadata-preserving, cross-volume safe), then two renames — the
/// old bundle is restored if the second rename fails.
pub fn apply_mac_app(staged: &Path, bundle: &Path) -> anyhow::Result<()> {
    let parent = bundle
        .parent()
        .context("app bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .context("app bundle has no name")?
        .to_string_lossy();
    let pid = std::process::id();
    let fresh = parent.join(format!(".{name}.new-{pid}"));
    let old = parent.join(format!(".{name}.old-{pid}"));
    let _ = std::fs::remove_dir_all(&fresh);
    run(
        "ditto",
        &[&staged.to_string_lossy(), &fresh.to_string_lossy()],
    )?;
    std::fs::rename(bundle, &old).context("moving the current app aside")?;
    if let Err(err) = std::fs::rename(&fresh, bundle) {
        let _ = std::fs::rename(&old, bundle);
        let _ = std::fs::remove_dir_all(&fresh);
        return Err(err).context("installing the new app bundle");
    }
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

/// Detached relauncher: waits for THIS process to exit, then `open`s the bundle.
/// (Opening before exit would race the single-instance engine lock and the IPC
/// port.) The caller quits the app after this returns.
pub fn relaunch_app_after_exit(bundle: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let pid = std::process::id();
        let script = format!(
            "while /bin/kill -0 {pid} 2>/dev/null; do sleep 0.2; done; /usr/bin/open \"{}\"",
            bundle.display()
        );
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        if let Err(err) = command.spawn() {
            tracing::error!(error = %err, "failed to spawn the relauncher");
        }
    }
    #[cfg(not(unix))]
    let _ = bundle;
}

// ---------------------------------------------------------------------------
// Engine-side checker
// ---------------------------------------------------------------------------

/// What the engine reports over the `UpdateStatus` stream. Version facts only —
/// download/apply progress is owned by whoever drives the update (UI or CLI).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    /// Whether this engine can apply the release through its managed install.
    #[serde(default)]
    pub can_apply: bool,
    /// Epoch ms of the last successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: current_version().to_string(),
            latest_version: None,
            update_available: false,
            can_apply: matches!(detect_install(), InstallKind::Managed { .. }),
            checked_at: None,
            error: None,
        }
    }
}

/// "Nothing would be interrupted by a restart right now" — wired by the engine
/// to its live-run and open-terminal registries. `None` = no gate.
pub type QuiescentCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// Background release checker: polls `{edge}/releases` on a 2h cadence and
/// publishes [`UpdateStatus`] over a watch channel. Checks only report release
/// availability; installation always requires an explicit `ApplyUpdate` call.
#[derive(Clone)]
pub struct Updater {
    edge_url: String,
    status_tx: Arc<watch::Sender<UpdateStatus>>,
    quiescent: Option<QuiescentCheck>,
    task: Arc<std::sync::Mutex<Option<tokio::task::AbortHandle>>>,
}

impl Updater {
    /// Spawn the check loop (must run on a tokio runtime).
    pub fn spawn(edge_url: String, quiescent: Option<QuiescentCheck>) -> Self {
        let (status_tx, _) = watch::channel(UpdateStatus::initial());
        let updater = Self {
            edge_url,
            status_tx: Arc::new(status_tx),
            quiescent,
            task: Arc::new(std::sync::Mutex::new(None)),
        };
        let for_loop = updater.clone();
        let task = tokio::spawn(async move { for_loop.check_loop().await });
        *updater
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task.abort_handle());
        updater
    }

    /// Stop this runtime's release checker. Clones share the same task handle,
    /// so workspace reload teardown cancels exactly the checker it created.
    pub fn shutdown(&self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status_tx.subscribe()
    }

    fn quiescent_now(&self) -> bool {
        self.quiescent.as_ref().is_none_or(|check| check())
    }

    async fn check_loop(&self) {
        tokio::time::sleep(CHECK_INITIAL_DELAY).await;
        loop {
            let ok = self.check_once().await;
            tokio::time::sleep(if ok { CHECK_INTERVAL } else { CHECK_RETRY }).await;
        }
    }

    /// One check; returns false on fetch failure (retry sooner).
    async fn check_once(&self) -> bool {
        match fetch_latest(&self.edge_url).await {
            Ok(manifest) => {
                let status = UpdateStatus {
                    current_version: current_version().to_string(),
                    update_available: version_newer(&manifest.version, current_version()),
                    can_apply: matches!(detect_install(), InstallKind::Managed { .. }),
                    latest_version: Some(manifest.version),
                    checked_at: Some(now_ms()),
                    error: None,
                };
                if status.update_available {
                    tracing::info!(
                        latest = status.latest_version.as_deref().unwrap_or(""),
                        current = %status.current_version,
                        "update available"
                    );
                }
                self.status_tx.send_replace(status);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "update check failed");
                self.status_tx
                    .send_modify(|s| s.error = Some(format!("{err:#}")));
                false
            }
        }
    }

    /// Stage + apply the newest release on THIS device (managed installs only),
    /// then restart the service after a short delay so the caller's RPC reply
    /// flushes before systemd/launchd kills this process.
    pub async fn apply(&self) -> anyhow::Result<String> {
        let InstallKind::Managed { app_root } = detect_install() else {
            bail!(
                "this install is not update-managed — the desktop app updates from its UI; \
                 source builds update via git"
            );
        };
        let manifest = fetch_latest(&self.edge_url).await?;
        if !version_newer(&manifest.version, current_version()) {
            bail!("already up to date ({})", current_version());
        }
        stage_headless(&self.edge_url, &manifest, &app_root).await?;
        let mut deferred = false;
        while !self.quiescent_now() {
            if !deferred {
                deferred = true;
                tracing::info!("user-approved update waiting for sessions and terminals to finish");
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
        apply_headless(&app_root, &manifest.version)?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            if let Err(err) = restart_service() {
                tracing::warn!(error = %err, "service restart failed — restart the engine to finish the update");
            }
        });
        Ok(manifest.version)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_aborts_release_checker() {
        let updater = Updater::spawn("http://127.0.0.1:1".into(), None);
        let task = updater
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .expect("release checker task")
            .clone();
        updater.shutdown();
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }

    #[test]
    fn version_compare() {
        assert!(version_newer("0.1.1", "0.1.0"));
        assert!(version_newer("0.2.0", "0.1.9"));
        assert!(version_newer("0.1.10", "0.1.9"));
        assert!(version_newer("v0.1.1", "0.1.0"));
        assert!(version_newer("0.1.0.1", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.0"));
        assert!(!version_newer("0.1.0", "0.1.1"));
        // Garbage never counts as newer.
        assert!(!version_newer("", "0.1.0"));
        assert!(!version_newer("nightly", "0.1.0"));
    }

    #[test]
    fn install_kind_detection() {
        assert_eq!(
            detect_install_from(
                Path::new("/home/u/.jolt/app/0.1.1/jolt"),
                Some(Path::new("/home/u")),
            ),
            InstallKind::Managed {
                app_root: PathBuf::from("/home/u/.jolt/app")
            }
        );
        assert_eq!(
            detect_install_from(
                Path::new("/Applications/Jolt.app/Contents/MacOS/Jolt"),
                Some(Path::new("/Users/u")),
            ),
            InstallKind::MacApp {
                bundle: PathBuf::from("/Applications/Jolt.app")
            }
        );
        // A path merely containing `.app` without the bundle layout is not a bundle.
        assert_eq!(
            detect_install_from(Path::new("/tmp/foo.app/jolt"), None),
            InstallKind::Unmanaged
        );
        assert_eq!(
            detect_install_from(
                Path::new("/src/target/release/Jolt"),
                Some(Path::new("/home/u"))
            ),
            InstallKind::Unmanaged
        );
    }

    #[test]
    fn artifact_names_match_packaging() {
        let (os, arch) = platform_key();
        assert!(headless_artifact("0.2.0").starts_with("jolt-0.2.0-"));
        assert_eq!(
            headless_artifact("0.2.0"),
            format!("jolt-0.2.0-{os}-{arch}.tar.gz")
        );
        assert_eq!(
            mac_app_artifact("0.2.0"),
            format!("jolt-0.2.0-macos-{arch}-app.tar.gz")
        );
    }

    #[test]
    fn manifest_requires_artifact_metadata() {
        let full: Manifest = serde_json::from_str(
            r#"{"version":"0.1.1","files":{"jolt-0.1.1-linux-x86_64.tar.gz":{"sha256":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(full.version, "0.1.1");
        assert_eq!(full.files["jolt-0.1.1-linux-x86_64.tar.gz"].sha256, "abc");
        assert!(serde_json::from_str::<Manifest>(r#"{"version":"0.1.1"}"#).is_err());
    }

    #[test]
    fn desktop_entry_uses_the_managed_executable() {
        let executable = Path::new("/home/a b/$cash/%bin/`jolt`\\x");
        let rendered = render_desktop_entry(
            "[Desktop Entry]\nExec=jolt\nTryExec=jolt\nIcon=jolt\n",
            executable,
        )
        .unwrap();
        assert!(
            rendered.contains(r#"Exec="/home/a b/\$cash/%%bin/\`jolt\`\\x""#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"TryExec=/home/a b/$cash/%bin/`jolt`\\x"#),
            "{rendered}"
        );
    }

    #[test]
    fn managed_desktop_assets_install_under_xdg_data_home() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        let current = app_root.join("current");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("jolt-desktop"), b"binary").unwrap();
        std::fs::write(
            current.join("jolt.desktop"),
            b"[Desktop Entry]\nExec=jolt\nTryExec=jolt\nIcon=jolt\n",
        )
        .unwrap();
        std::fs::write(current.join("jolt.png"), b"icon").unwrap();
        let data_home = tmp.path().join("xdg-data");

        let (applications, changed) =
            install_linux_desktop_integration(&app_root, &data_home).unwrap();

        assert!(changed);
        assert_eq!(applications, data_home.join("applications"));
        let desktop = std::fs::read_to_string(applications.join("jolt.desktop")).unwrap();
        assert!(desktop.contains(&format!(
            "Exec=\"{}\"",
            current.join("jolt-desktop").display()
        )));
        assert_eq!(
            std::fs::read(data_home.join("icons/hicolor/512x512/apps/jolt.png")).unwrap(),
            b"icon"
        );
        assert_eq!(
            std::fs::read(data_home.join("icons/hicolor/1024x1024/apps/jolt.png")).unwrap(),
            b"icon"
        );
        assert!(
            !install_linux_desktop_integration(&app_root, &data_home)
                .unwrap()
                .1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(applications.join("jolt.desktop"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o644
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn headless_symlink_swap() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path().join("app");
        for ver in ["0.1.0", "0.1.1"] {
            std::fs::create_dir_all(app_root.join(ver)).unwrap();
            std::fs::write(app_root.join(ver).join("jolt"), ver).unwrap();
        }
        apply_headless(&app_root, "0.1.0").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.0")
        );
        // Swap over an existing symlink.
        apply_headless(&app_root, "0.1.1").unwrap();
        assert_eq!(
            std::fs::read_link(app_root.join("current")).unwrap(),
            app_root.join("0.1.1")
        );
        // Unstaged version refuses.
        assert!(apply_headless(&app_root, "0.2.0").is_err());
    }
}

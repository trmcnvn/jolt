# Quickstart

This page gets a computer running Jolt and starts a first agent thread.

## 1. Install Jolt

### Linux daemon

The installer places a managed binary under `$XDG_DATA_HOME/jolt/app` (normally `~/.local/share/jolt/app`), adds a desktop launcher, installs a systemd user service, and exposes `jolt` on your path:

```bash
curl -fsSL https://jolt.trmcnvn.dev/install.sh | sh
systemctl --user start jolt
```

For an always-on machine, allow the user service to start without an interactive login:

```bash
loginctl enable-linger "$USER"
```

Use Jolt's service wrapper for normal management:

```bash
jolt daemon start
jolt daemon stop
jolt daemon restart
jolt daemon status
```

### Desktop from source

Build the desktop app from the Rust workspace with stable Rust and Zig 0.16.0:

```bash
cargo build -p jolt
./target/debug/Jolt
```

Running `Jolt` with no subcommand opens the desktop app. It connects to an engine already listening on the local IPC port, or embeds an engine in the app when no daemon is present. On macOS and Linux, **Settings → Devices → Keep this device available** installs the per-user background service so agents continue after you quit the app.

See [Development](development.md) for platform dependencies and packaged builds.

## 2. Install an agent CLI

Jolt launches agent CLIs on the device that owns the thread. Install and authenticate at least one:

- **Claude Code:** install `claude` and complete its normal login.
- **Codex:** install `codex` and complete its normal login.
- **Pi:** install `pi`, run it once, and use `/login` for the providers you want.

Jolt discovers models from the installed CLI, and each engine uses the credentials available on its device.

See [Agent harnesses](harnesses.md) for details.

## 3. Choose Local or sign in

The desktop opens without an account in a Local scope stored only on that device. Sign in from the user menu to add synchronized spaces, remote device control, and iOS access. If Local already contains threads, Jolt asks before moving them into the account and explains which data remains device-local.

A headless engine also starts in Local without an account or network connection. To make its spaces remotely available, stop the daemon, run `jolt login`, open the printed URL and paste the browser code, then restart the daemon. The session is saved under Jolt's data directory and reused by the daemon. A headed viewport can instead sign in without stopping its embedded engine.

Organization setup is automatic: Jolt adopts the sole existing membership or creates a private organization named `Personal`. **Switch to Local** keeps the account runtime running in the background; **Sign out** disconnects it and returns to Local.

## 4. Add a space

A **space** is a folder on a specific device. It determines where new threads run.

1. Open Jolt on a desktop.
2. Choose **Add space** or press `Cmd+K` on macOS / `Ctrl+K` elsewhere.
3. Pick a device.
4. Browse to a folder and add it.

The device may be local or remote. Folder browsing is executed by that device's engine through the relay.

## 5. Start a thread

1. Select a space.
2. Choose **New thread**.
3. Pick Claude Code, Codex, or Pi; then choose a model and reasoning level.
4. For a Git or Jujutsu space, choose the current checkout, an existing working copy, or a new isolated worktree/workspace.
5. Enter a prompt and send it.

The thread belongs to the space's device. Use **Close** to move it into the **Closed** section after its active run stops; closing does not migrate or stop the host engine.

## 6. Add another device

Install Jolt and sign in with the same account on another computer. Its engine registers automatically and the workspace index appears on every connected client. Add folders from that device as spaces, then start or control its threads from any viewport.

For a server, leave `jolt headless` managed by systemd or launchd. For a phone, see [Jolt for iOS](ios.md).

## Next steps

- [Using Jolt](using-jolt.md)
- [CLI and headless engines](cli.md)
- [Settings](settings.md)
- [Security and data](security.md)

# Quickstart

This page gets a computer running Jolt and starts a first agent session.

## 1. Install Jolt

### Linux daemon

The installer places a managed binary under `~/.jolt/app`, installs a systemd user service, and exposes `jolt` on your path:

```bash
curl -fsSL https://jolt.trmcnvn.dev/install.sh | sh
jolt login
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

Build the desktop app from the Rust workspace with stable Rust and Zig 0.15.2:

```bash
cargo build -p jolt
./target/debug/Jolt
```

Running `Jolt` with no subcommand opens the desktop app. It connects to an engine already listening on the local IPC port, or embeds an engine in the app when no daemon is present.

See [Development](development.md) for platform dependencies and packaged builds.

## 2. Install an agent CLI

Jolt launches agent CLIs on the device that owns the session. Install and authenticate at least one:

- **Claude Code:** install `claude` and complete its normal login.
- **Codex:** install `codex` and complete its normal login.
- **Pi:** install `pi`, run it once, and use `/login` for the providers you want.

Jolt discovers models from the installed CLI, and each engine uses the credentials available on its device.

See [Agent harnesses](harnesses.md) for details.

## 3. Sign in to Jolt

For a headless device:

```bash
jolt login
```

Open the printed URL, then paste the browser code into the terminal. The session is saved under Jolt's data directory and reused by the daemon.

The desktop app presents the same sign-in flow in its gate. Organization setup is automatic: Jolt adopts the sole existing membership or creates a private organization named `Personal`.

## 4. Add a space

A **space** is a folder on a specific device. It determines where new sessions run.

1. Open Jolt on a desktop.
2. Choose **Add space** or press `Cmd+K` on macOS / `Ctrl+K` elsewhere.
3. Pick a device.
4. Browse to a folder and add it.

The device may be local or remote. Folder browsing is executed by that device's engine through the relay.

## 5. Start a session

1. Select a space.
2. Choose **New session**.
3. Pick Claude Code, Codex, or Pi; then choose a model and reasoning level.
4. For a Git or Jujutsu space, choose the current checkout, an existing working copy, or a new isolated worktree/workspace.
5. Enter a prompt and send it.

The session belongs to the space's device. Closing its tab archives it; it does not stop or migrate the host engine.

## 6. Add another device

Install Jolt and sign in with the same account on another computer. Its engine registers automatically and the workspace index appears on every connected client. Add folders from that device as spaces, then start or control its sessions from any viewport.

For a server, leave `jolt headless` managed by systemd or launchd. For a phone, see [Jolt for iOS](ios.md).

## Next steps

- [Using Jolt](using-jolt.md)
- [CLI and headless engines](cli.md)
- [Settings](settings.md)
- [Security and data](security.md)

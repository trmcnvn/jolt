# Jolt

Jolt is a multi-device ADE for Claude Code, Codex, and Pi. Use it locally without an account, or sign in to control your agents from any of your devices.

The desktop always provides a Local scope that never syncs. When signed in, every device runs a small account engine that keeps your sessions in sync: start an
agent on one machine, follow and drive it from another. Install the engine as
a daemon on an always-on machine (a VPS, a spare box) and your agents keep
working after you close your laptop.

## Install the daemon (Linux)

```bash
curl -fsSL https://jolt.trmcnvn.dev/install.sh | sh
jolt login                          # sign in (paste a code, done)
systemctl --user start jolt
```

No configuration needed. Day-to-day:

```bash
jolt status      # signed in? engine running?
jolt update      # update to the latest release
jolt daemon start|stop|restart|status
```

On macOS: build `jolt` from source, then `jolt daemon install` (launchd).

## Agent CLIs

Jolt uses locally installed agent CLIs. For Pi, install and authenticate it first:

```bash
pi              # then use /login for the providers you want
```

Pi models are discovered dynamically. Jolt asks before loading project-local Pi
settings, extensions, skills, or prompts when Pi has no saved trust decision; the
prompt can save the folder decision to Pi's trust store.
`JOLT_PI_EXECUTABLE` can point Jolt at a non-standard Pi installation.

Harness environment secrets can be added under **Settings → Secrets**. Values
stay in the device's native credential store (macOS Keychain, Windows Credential
Manager, or Secret Service on Linux) and are injected only into the selected
harness processes. Secrets are device-local and are never synced.

## Building from source

Building Jolt requires stable Rust and Zig 0.16.0:

```bash
cargo build -p jolt
```

## Version control

Each device uses one command-line VCS backend. Jolt selects Jujutsu 0.43+
when available, then falls back to Git; the active backend can be changed per
device under **Settings → Version control**. `JOLT_JJ_EXECUTABLE` and
`JOLT_GIT_EXECUTABLE` override executable discovery. Jolt-created JJ workspaces
live under `~/.jolt/workspaces` (`JOLT_WORKSPACES_DIR` overrides the root).

## Comet

Jolt is a fork of [Comet](https://github.com/zeronsh/comet). A huge shoutout
to the Comet project and its contributors for the foundation they created.

---

Start with the [Jolt documentation](docs/README.md) for usage, harnesses, headless deployment, architecture, sync, RPC, and development.

Licensed under the [MIT License](LICENSE).

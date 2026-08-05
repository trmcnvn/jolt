# Jolt

Control your coding agents (Claude Code, Codex, Pi) from any of your devices.

![Jolt running a Claude Code session](docs/screenshot.png)

Every device runs a small engine that keeps your sessions in sync: start an
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

## Building from source

Building Jolt requires stable Rust and Zig 0.15.2:

```bash
cargo build -p jolt
```

## Version control

Each device uses one command-line VCS backend. Jolt selects Jujutsu 0.43+
when available, then falls back to Git; the active backend can be changed per
device under **Settings → Version control**. `JOLT_JJ_EXECUTABLE` and
`JOLT_GIT_EXECUTABLE` override executable discovery. Jolt-created JJ workspaces
live under `~/.jolt/workspaces` (`JOLT_WORKSPACES_DIR` overrides the root).

---

Developing or curious how it works? See [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).

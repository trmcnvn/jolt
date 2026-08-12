# Jolt

Jolt is a local-first ADE for Claude Code, Codex, and Pi. The desktop and
headless engine work without an account or cloud service; your Local scope
stays on that device.

Remote control and multi-device sync are optional self-hosted capabilities.
They require a Jolt edge deployment backed by Cloudflare Workers, Durable
Objects, R2, and WorkOS AuthKit. The public Jolt service does not provide open
account registration.

## Install the daemon (Linux)

```bash
curl -fsSL https://jolt.trmcnvn.dev/install.sh | sh
# Optional before starting: jolt login
systemctl --user start jolt         # Local works without an account
```

No configuration needed. Day-to-day:

```bash
jolt status      # Local/Account auth and engine status
jolt update      # update to the latest release
jolt daemon start|stop|restart|status
```

On macOS and Linux desktops, enable **Settings → Devices → Keep this device available** to run the engine in the background. The CLI equivalent is `jolt daemon install`.

## Remote devices and sync

Local mode needs no setup. To use Account mode for remote devices, synchronized
spaces, and mobile access, deploy your own backend:

1. Create and configure a WorkOS AuthKit environment.
2. Deploy `edge/` to Cloudflare with its Durable Object classes and R2 buckets.
3. Configure the Worker’s `WORKOS_CLIENT_ID`, set `WORKOS_API_KEY` as a Wrangler
   secret, and use `AUTH_MODE=workos` for public deployments.
4. Point desktop and headless clients at it with `JOLT_EDGE_URL` and
   `JOLT_WORKOS_CLIENT_ID`. Native mobile builds must use the same endpoints and
   WorkOS client ID.

The checked-in deployment configuration contains Jolt’s production resource
names and must be adapted for your own Cloudflare account and domains. See
[environment variables](docs/environment-variables.md),
[development](docs/development.md), and [security](docs/security.md) before
exposing an edge deployment publicly.

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
live under the platform data directory's `workspaces` folder
(`JOLT_WORKSPACES_DIR` overrides the root).

## Comet

Jolt is a fork of [Comet](https://github.com/zeronsh/comet). A huge shoutout
to the Comet project and its contributors for the foundation they created.

---

Start with the [Jolt documentation](docs/README.md) for usage, harnesses, headless deployment, architecture, sync, RPC, and development.

Licensed under the [MIT License](LICENSE).

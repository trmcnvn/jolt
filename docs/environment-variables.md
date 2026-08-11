# Environment variables

Jolt works without environment configuration in production. Variables are primarily for alternate data roots, self-hosted/development edges, executable discovery, and diagnostics.

## Runtime configuration

| Variable | Default | Description |
| --- | --- | --- |
| `JOLT_DATA_DIR` | platform data directory | Engine stores, auth session, UI settings, logs, uploads, and managed update staging |
| `JOLT_EDGE_URL` | `https://edge.jolt.trmcnvn.dev` | Worker base URL for auth, sync, relay, attachments, and releases |
| `JOLT_IPC_PORT` | `27654` | Localhost engine RPC port probed by the desktop and served by engines |
| `JOLT_CALLBACK_PORT` | `27641` | Preferred headed WorkOS loopback callback port |
| `JOLT_DEVICE_NAME` | hostname | Display name stamped into this engine's device row |
| `JOLT_HARNESS` | `claude-code` | Fallback harness for chats without config; supported production values are `claude-code`, `codex`, and `pi` (`mock` is for tests/demo) |
| `RUST_LOG` | mode-specific | `tracing_subscriber` filter; long-running modes default to info |

The default data directory is `$XDG_DATA_HOME/jolt` on Linux (normally `~/.local/share/jolt`) and `~/Library/Application Support/Jolt` on macOS. On first lock-free startup, Jolt moves an existing default `~/.jolt` directory there, using a staged copy when the roots span filesystems, and leaves a compatibility symlink. An explicit `JOLT_DATA_DIR` is used as-is and never migrated.

Changing `JOLT_DATA_DIR` creates a distinct device identity, auth session, repository registry, and local usage store unless data is copied intentionally.

## Authentication and edge development

| Variable | Description |
| --- | --- |
| `JOLT_WORKOS_CLIENT_ID` | Override the baked public WorkOS client ID. Set to an empty string to disable WorkOS mode. |
| `JOLT_WORKOS_API_BASE` | Override the WorkOS API base used by the auth client; intended for tests/development. |
| `JOLT_EDGE_TOKEN` | Enable development-edge auth and room sync. A dev edge commonly uses `userId@orgId`. |
| `JOLT_ORG_ID` | Development organization fallback. A WorkOS session's scoped organization wins. |

Example against local Wrangler:

```bash
npm -C edge run dev

JOLT_EDGE_URL=http://localhost:27640 \
JOLT_EDGE_TOKEN=alice@org1 \
JOLT_ORG_ID=org1 \
JOLT_WORKOS_CLIENT_ID= \
cargo run -p jolt --bin Jolt
```

With WorkOS disabled and no edge token, the engine opens the offline Local scope. Development edge mode is explicit: setting `JOLT_EDGE_TOKEN` selects the Account runtime for that bearer without probing `/health`.

## Executables and working-copy roots

| Variable | Description |
| --- | --- |
| `JOLT_PI_EXECUTABLE` | Absolute path to a non-standard Pi executable |
| `JOLT_JJ_EXECUTABLE` | Override Jujutsu executable discovery |
| `JOLT_GIT_EXECUTABLE` | Override Git executable discovery |
| `JOLT_WORKTREES_DIR` | Root for Jolt-created Git worktrees; default `{data_dir}/worktrees` |
| `JOLT_WORKSPACES_DIR` | Root for Jolt-created Jujutsu workspaces; default `{data_dir}/workspaces` |
| `JOLT_NO_LOGIN_SHELL` | Any non-empty value disables the login-shell PATH snapshot |

Harnesses also inspect the process PATH, a login-shell PATH snapshot, and common fnm/nvm/Volta/Bun/pnpm locations. `jolt daemon install` captures the current PATH, `XDG_DATA_HOME`, and selected Jolt variables into the service definition.

For arbitrary API keys or service credentials that should reach only selected agent CLIs, use **Settings → Secrets** instead of Jolt process variables.

## Diagnostics

| Variable | Description |
| --- | --- |
| `JOLT_GPU_STATS` | Enable the pinned gpui fork's GPU memory/statistics diagnostics |
| `RUST_LOG` | Select structured log levels and modules |

Debug builds contain additional UI/render/test knobs (`JOLT_FRAME_STATS`, `JOLT_PERFORMANCE_HUD`, mock-harness controls, screenshot routes, and others). They are development instrumentation rather than a stable runtime interface and are intentionally not listed as user configuration.

## Service environments

`jolt daemon install` captures these when present:

```text
PATH
XDG_DATA_HOME
JOLT_DATA_DIR
JOLT_EDGE_URL
JOLT_EDGE_TOKEN
JOLT_ORG_ID
JOLT_WORKOS_CLIENT_ID
JOLT_WORKOS_API_BASE
JOLT_IPC_PORT
JOLT_CALLBACK_PORT
JOLT_HARNESS
JOLT_PI_EXECUTABLE
JOLT_JJ_EXECUTABLE
JOLT_GIT_EXECUTABLE
JOLT_WORKTREES_DIR
JOLT_WORKSPACES_DIR
JOLT_DEVICE_NAME
RUST_LOG
```

On Linux, the generated unit also reads `{data_dir}/env` if it exists. After changing captured values, reinstall the daemon unit or update its environment file and restart it.

## Edge variables and secrets

The Cloudflare Worker uses its own configuration, not the engine's `JOLT_*` variables:

- `AUTH_MODE`: `workos` in production or `dev` locally;
- `WORKOS_CLIENT_ID`: JWT/JWKS audience configuration;
- `WORKOS_API_KEY`: Wrangler secret used by `/auth/*` routes.

See `edge/wrangler.jsonc` for Durable Object and R2 bindings.

# CLI and headless engines

The `Jolt` binary is headed by default. The installed command is normally exposed as `jolt`.

```text
jolt [COMMAND]
```

## Commands

| Command | Description |
| --- | --- |
| `jolt` | Open the desktop viewport. Connect to a local daemon or embed an engine. |
| `jolt headless` | Run the engine and localhost RPC server without a window. |
| `jolt login` | Complete paste-code sign-in, provision the hidden Personal organization, save the session, and exit. |
| `jolt logout` | Remove the saved Jolt session. Refuses while an engine owns the data directory. |
| `jolt status` | Show data directory, edge, Local/Account auth state, engine PID, and IPC reachability. Signed-out Local mode is healthy. |
| `jolt sync` | Query the running engine for registry and SessionHub state/counters. |
| `jolt recover-chat SOURCE NEW SPACE` | Create a fresh-host chat from a permanently lost host's verified published transcript. |
| `jolt update` | Check, download, verify, apply, and restart a managed install. |
| `jolt update --check` | Report whether an update exists. Exits `1` when one is available. |
| `jolt daemon …` | Install or manage a system service. |

## Headed versus headless

The headed app probes `ws://127.0.0.1:27654` by default:

1. If a Jolt engine answers, the UI uses it and leaves it running when the window exits.
2. Otherwise the app embeds an engine and communicates through the same serialized RPC envelopes over an in-memory transport.
3. The embedded engine also binds the localhost port when possible, allowing another viewport to attach.
4. If the port is occupied by a non-Jolt process, the app still embeds an engine; only external viewports lose access.

`jolt headless` always owns the engine and treats an IPC bind failure as fatal. It serves Local immediately when signed out; a saved account also enables the Account runtime and relay.

One data directory can have only one engine. The process holds `{data_dir}/engine.lock` for its lifetime to prevent concurrent SQLite and journal writers.

## Authentication

A background service starts Local without authentication. To enable Account sync from the CLI, stop it before changing the saved session:

```bash
jolt daemon stop
jolt login
jolt daemon start
```

`login` and `logout` take the same data-directory lock as the engine. If the desktop app or daemon is running, use its UI or stop it first.

The saved WorkOS session is `{data_dir}/session.json` and is written with owner-only permissions. Organization setup is automatic and requires exactly zero or one active membership.

## Daemon management

The desktop exposes the same macOS/Linux service through **Settings → Devices → Keep this device available**. Changing it restarts the app so engine ownership transfers safely.

```bash
jolt daemon install
jolt daemon uninstall
jolt daemon start
jolt daemon stop
jolt daemon restart
jolt daemon status
```

- **Linux:** installs `~/.config/systemd/user/jolt.service`, enables it, and starts it.
- **macOS:** installs `~/Library/LaunchAgents/dev.trmcnvn.jolt.plist`, bootstraps it, and starts it.

Installation captures `PATH`, `XDG_DATA_HOME`, supported `JOLT_*` variables, and `RUST_LOG` from the current shell. Linux also reads optional overrides from `{data_dir}/env` through the systemd unit.

For Linux logs:

```bash
journalctl --user -u jolt.service -f
```

For the macOS LaunchAgent, the default service log is `{data_dir}/daemon.log`.

## Updates

Jolt polls the edge release manifest every six hours after startup. The UI reports available versions and can update a packaged macOS app or a managed remote device. Restarting after a macOS app update also restarts an installed background engine so both processes load the same release.

Managed Linux installs use versioned directories under `{data_dir}/app/<version>` and an atomic `current` symlink. Downloads are verified against the release manifest's SHA-256 when present. On startup, the active version refreshes its desktop launcher and icon, including for installations created before desktop integration was available.

Managed headless daemons report available releases to signed-in desktops. Updates run only after an explicit **Update** action on **Settings → Devices**, and wait for active runs and terminals to finish before restarting the service.

Source builds and hand-copied binaries are report-only; update them through their source or package workflow.

## Permanent host recovery

Run the target desktop/daemon, create or choose a space owned by that machine,
then invoke `jolt recover-chat SOURCE_CHAT_ID NEW_CHAT_ID SPACE_ID`. The new ID
must be fresh. Recovery verifies the source SessionHub projection and sealed
page hashes, imports transcript history, and adds a provenance marker. It does
not reassign or delete the source chat, and it cannot transfer pending commands,
goals, native harness sessions, or machine-local checkout state. Recover every
needed thread before removing the lost device, because device removal retires its
source Hubs and their published recovery projections.

## Sync diagnostics

```bash
jolt sync
```

The command reports:

- connection state;
- registry frame/ack ages and liveness counters;
- SessionHub writer lease, projection sequence, command revision, and reconnects;
- persistent hosted-chat totals, normalized rows, unseeded Hubs, and unpublished local projections;
- the workspace registry and every currently open chat.

It requires a running local engine. Use it before restarting a device when diagnosing stale workspace rows or transcripts.

Long-running headed and headless modes also write launch logs under `{data_dir}/logs/` by default. The current and previous launch are retained separately; concurrent processes use PID-suffixed overflow logs rather than rotating a live file.

## Configuration

See [Environment variables](environment-variables.md) for ports, endpoints, executable overrides, data directories, and development auth.

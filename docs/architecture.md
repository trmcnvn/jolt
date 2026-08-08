# Architecture

Jolt is a multi-device ADE for local coding-agent CLIs. The engine is the authority for machine-local capabilities; viewports render and control engines through one RPC contract. Cloudflare edge services synchronize durable shared state and relay device-to-device calls.

## Topology

```text
                         Cloudflare edge
                 ┌─────────────────────────────┐
                 │ Worker: auth and routing    │
                 │ RegistryRoom: workspace rows│
                 │ SessionRoom: Loro per chat  │
                 │ DeviceRoom: engine relay    │
                 │ R2: attachments, backups,   │
                 │     and release artifacts   │
                 └───────────┬─────────────────┘
                             │ TLS / WebSocket
              ┌──────────────┴──────────────┐
              │                             │
     ┌────────▼────────┐           ┌────────▼────────┐
     │ Engine: device A│           │ Engine: device B│
     │ agents/files/PTY│           │ agents/files/PTY│
     └────────┬────────┘           └────────┬────────┘
              │ typed RPC                   │ typed RPC
       ┌──────▼──────┐               ┌──────▼──────┐
       │ desktop gpui│               │ desktop gpui│
       └─────────────┘               └─────────────┘

             iOS joins registry/session rooms directly and
             dials engines through DeviceRoom for live RPC.
```

## Processes

### Engine

The Rust engine:

- launches Claude Code, Codex, Pi, and test harness subprocesses;
- hosts an authenticated loopback MCP endpoint injected into supported live harness processes;
- owns local authentication, repositories, version-control commands, worktrees/workspaces, terminals, uploads, diffs, and usage;
- hosts session documents and executes commands only for chats assigned to its device;
- persists snapshots, command claims, run journals, settings, and identity-scoped telemetry;
- exposes the RPC service on localhost and through its device relay room.

A data directory has one engine owner, enforced by an OS-level lock.

### Desktop viewport

The gpui desktop app is a viewport over the RPC service. On startup it probes the configured localhost port:

- a responding engine becomes a remote local backend over WebSocket;
- otherwise the app creates an `EngineSupervisor`, uses the same JSON envelopes over an in-memory channel, and best-effort serves that engine on the localhost port.

The supervisor resolves the preferred scope behind the splash. It always owns a fully offline Local runtime and, while authenticated, a separate Account runtime. Switching the viewport to Local leaves Account runs, synchronization, and relay hosting alive in the background. Only Account is exposed through the device relay.

On macOS and Linux, the Devices setting can install the same headless engine as a per-user launchd/systemd service. Changing that setting gracefully shuts down the current engine, applies the service configuration, and relaunches the viewport so one process always owns the data directory.

### iOS viewport

The SwiftUI app maintains a local workspace-registry replica and a byte-bounded transcript page cache. It consumes edge manifest/tail streams, submits commands through a durable device-local outbox, and uses relay RPC when an engine must touch a filesystem or CLI. It does not retain complete session Loro documents.

### Edge

`edge/` is a TypeScript Cloudflare Worker with three Durable Object classes:

- **RegistryRoom:** current-state workspace rows and per-field last-write-wins merge.
- **SessionRoom:** per-chat canonical Loro synchronization, transcript manifest/page/live projections, durable command submission, diff sidecars, compaction, and backups.
- **DeviceRoom:** one host socket per engine, client byte relay, durable nudges, and small latest-value sidecars.

The Worker verifies WorkOS JWTs or development bearers before stamping identity into Durable Object requests. It also performs WorkOS code exchange/refresh and serves content-addressed attachments and signed release metadata.

## Data model

### Workspace registry

The sidebar and session index use a replicated table with four row kinds:

- `devices`
- `spaces`
- `chats`
- `sessions`

A per-user RegistryRoom is authoritative for current rows. Clients retain an authoritative snapshot plus pending local operations, which are overlaid for optimistic reads and replayed after reconnect. Per-field hybrid logical clocks provide deterministic last-write-wins behavior.

This data is small scalar index state. Transcript content never enters the registry. Chat tombstones also enter a durable artifact-purge queue: the registry retires the chat room and deletes its R2 backup and chat-scoped attachment prefix.

### Session documents

Every chat has one Loro document with three roots:

```text
meta      { chatId, schemaVersion }
messages  [ { id, role, parts, createdAt, deviceId, status?, continuationOf? } ]
commands  [ { id, payload, issuedBy, issuedAt, status, ... } ]
```

Text bodies use `LoroText`, allowing streamed appends to merge efficiently. `textReveal` part markers expose stable prose before tool, provider-message, input, and terminal boundaries while later text remains durably synced but unpainted; terminal recovery reveals preserved partial output. Large entries split into continuation records at part/code-point boundaries and join during projection.

The host engine writes transcript entries and command outcomes. Authorized viewers submit their own idempotent command entries through the edge. Synced tool projections deliberately omit sensitive or bulky inputs that are not needed for rendering.

Viewport transcript state is a derived projection: a compact whole-session manifest, byte-bounded historical pages, and a mutable live tail. Desktop builds the projection beside its local canonical document; iOS and remote viewers consume the edge projection. Unloaded pages remain estimated-height placeholders, so navigation and scrollbar range cover the complete conversation without decoding it.

The desktop Changes pane follows the same bounded-projection principle. The host captures one checkout snapshot, builds a compact complete file manifest, and splits retained unified patch text into immutable content-addressed pages. The pane renders collapsed headers without parsing bodies, fetches pages only for expanded viewport ranges, and virtualizes file headers, hunk headers, notices, lines, pending review cards, and unloaded placeholders in one list. Its expanded view offers explicit unified and split layouts; split rows pair deletion/addition blocks while side-specific review ranges and cards remain anchored to old or new coordinates. Assistant turns additionally capture the complete non-ignored VCS tree before and after execution, preserving pre-existing working-copy changes while deriving an immutable net turn delta. Its compact manifest travels with the transcript entry; content-addressed pages remain in the host's turn-diff store and load through the same desktop viewer. Edge manifests are chat-authorized while page bodies are deduplicated per checkout. iOS renders only the inline turn manifest as a collapsed changed-files card; it has no diff pane and never requests patch pages.

Review is a target-neutral, device-local lifecycle layered over reviewable surfaces rather than a diff transcript schema. A typed review draft owns a retained snapshot plus target-specific anchors; the initial diff adapter records old/new line coordinates and excerpts, while the model also reserves assistant-message text selectors. Draft bodies auto-save to local SQLite and never enter Loro or edge storage. Beginning a working-copy annotation leases that immutable diff revision, so later head updates produce a “newer changes available” notice instead of moving anchors. Sending formats all pending annotations and uses the composer's ordinary Run/Steer command path without replacing its visible draft or staged attachments; the local review clears only after command submission succeeds.

### Durable command plane

Run, shell, steer, interrupt, and input-answer operations are session-document entries. The chat's host device:

1. evaluates expiry and supersession rules;
2. claims the command in its local processed-command ledger **before** execution;
3. executes it at most once from that host store;
4. writes the outcome and transcript changes back to the document.

A device-room nudge tells a cold host to open the chat. Delivery does not depend on the nudge: the command itself is durable and remains pending while the host is offline.

### Machine-local state

The following stays on its owning device:

- agent/provider credential values;
- harness secret values;
- repository path registry and VCS selection;
- PTY processes and replay buffers;
- full run journals and stripped tool inputs;
- detailed token/cost usage;
- viewport layout, fonts, notifications, and keybindings.

Some local state is queried from a reachable device through RPC for display.

## Main flows

### Start a remote session

```text
viewport
  → Mutate createChat in a synced space
  → persist Run command in the local outbox
  → edge idempotently appends it to the chat Loro document
  → POST durable nudge to the host's DeviceRoom
  → host opens/syncs the document
  → host claims command and launches the selected harness
  → normalized events fold into transcript parts every ~120 ms
  → SessionRoom refreshes the mutable tail projection
  → viewports receive bounded live transcript frames
```

### Live remote RPC

Filesystem browsing, model/ref discovery, terminals, account management, diffs, attachment reads, and updates can carry `targetDeviceId`. The local engine dials that device's DeviceRoom, wraps the normal RPC stream in a virtual socket, and forwards the unchanged method.

Secret methods are rejected by the host relay. Local identity, auth, and workspace watch/mutation methods do not honor `targetDeviceId` forwarding and are normally called on the directly connected engine; an iOS client can dial a host directly for the limited workspace mutation it needs.

## Storage layout

Default root: `~/.jolt`.

```text
~/.jolt/
  engine.lock
  session.json
  ui-settings.json
  themes/*.json              # installation-level paired custom palettes
  composer-defaults.json
  vcs-settings.json
  harness-secrets.json        # metadata only; values are in OS credentials
  repos.json
  repos/
  agent-accounts/
  logs/
  updates/
  scopes/local/current/
    local-scope-id
    device-id
    docs.sqlite3
    usage.sqlite
    journals/*.jsonl
    uploads/
  scopes/accounts/<org>/<user>/
    scope-layout-v1.json
    device-id
    docs.sqlite3              # doc/registry snapshots + processed commands
    usage.sqlite
    journals/*.jsonl
    uploads/
```

Git worktrees default to `~/.jolt/worktrees`; Jujutsu workspaces default to `~/.jolt/workspaces`.

Scope-isolated stores prevent Local data from entering edge synchronization and prevent a later WorkOS identity from reusing another account's cached documents. Existing `orgs/<org>/<user>` stores are moved into this layout on first startup; the existing account device identity is preserved and a fresh Local scope is created.

## Rust workspace

| Path | Responsibility |
| --- | --- |
| `crates/proto` | Shared entities, agent events, usage, secrets, and pure view derivations |
| `crates/platform` | Login-shell process environment and suspend/wake detection |
| `crates/session-doc` | Session schema, command ledger, render parts, and transcript projections |
| `crates/registry-model` | Workspace registry rows, HLC operations, and optimistic local state |
| `crates/store` | Local SQLite document snapshots and processed-command ledger |
| `crates/sync` | Loro session-room and workspace-registry network clients |
| `crates/harness` | Common harness trait, controls, environment provider, and test mock |
| `crates/harness-{claude,codex,pi}` | Isolated production CLI adapters and protocol tests |
| `crates/mcp` | Loopback MCP host, bearer leases, tool schemas, and backend contract |
| `crates/vcs` | Device-local repositories, VCS commands, workspaces, and forge review lookup |
| `crates/terminal` | Engine-side PTY ownership, replay, and lifecycle |
| `crates/rpc` | Generic envelopes, clients/servers, and in-memory/WebSocket transports |
| `crates/api` | Product RPC methods, shared models, and typed unary/JSON-stream/binary-stream contracts |
| `crates/relay` | DeviceRoom frame codec, host relay, and peer link cache |
| `crates/engine` | Runtime assembly, sessions, document hosts, capability coordination, accounts, secrets, and usage |
| `crates/update` | Release checks, downloads, verification, swaps, and restart support |
| `crates/ui` | gpui desktop shell and views |
| `apps/jolt` | Binary, CLI auth, daemon management, and environment resolution |
| `apps/ios` | SwiftUI mobile viewport |
| `edge` | Worker, Durable Objects, R2 routes, and WorkOS integration |

## Design invariants

- A session runs on exactly one host device at a time.
- A space fixes its owning device and folder.
- Transcript/session commands use Loro; workspace index rows use RegistryRoom current state.
- Command outcomes are host-written and locally claimed before execution.
- Viewports use the same RPC envelope in-process, over localhost, and through the relay.
- Secret values never cross the device relay.
- Usage telemetry remains device-local and outside synchronized documents.
- Live status and presence are freshness-gated; durable state remains independent of heartbeats.

See [Synchronization](sync.md), [RPC](rpc.md), and [Security and data](security.md) for protocol-level detail.

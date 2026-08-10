# Agent harnesses

A Jolt harness adapts an installed coding-agent CLI into one normalized stream of text, reasoning, tool, usage, input, steering, compaction, error, and completion events. The engine starts and owns the CLI process on the session's host device.

## Capability overview

| Harness | Integration | Steering | Model discovery | Authentication |
| --- | --- | --- | --- | --- |
| Claude Code | `claude` stream-json and control protocol | Step boundary | CLI control protocol | Claude CLI; account slots can be managed in Jolt |
| Codex | `codex app-server` JSON-RPC | Native `turn/steer` | `model/list` | Codex CLI; account slots can be managed in Jolt |
| Pi | `pi --mode rpc` JSONL | Native RPC steering | Authenticated provider/model catalog | Pi owns provider auth; use Pi's `/login` |

All three support resume, interrupt escalation, normalized usage, images where the CLI supports them, and structured input bridging.

## Product MCP

For each live Claude Code, Codex, or Pi process, Jolt additively injects a product-owned Streamable HTTP MCP server named `jolt`. The identity-scoped engine starts the loopback listener lazily on an ephemeral port and issues a chat-scoped bearer credential that is revoked when the run ends. The server exposes goal lifecycle tools and `request_answers`, but no prompts, resources, or general-purpose coding tools; it does not replace harness-native coding tools.

Claude receives the server through `--mcp-config`; Codex receives per-process `mcp_servers.jolt` configuration. Because Pi has no native MCP client, Jolt explicitly loads its bundled bridge extension for the live process. The bridge is independent of project trust and does not install packages or change Pi settings. Existing user and project extensions and MCP servers remain available.

## Executable discovery

Jolt first uses the current process `PATH`, then supplements it with a login-shell PATH snapshot and common Node-version-manager locations. This is important for apps launched from Finder, Dock, launchd, or systemd, which often receive a minimal environment.

A service installed with `jolt daemon install` captures the current `PATH` into its unit. Reinstall the unit after materially changing that PATH, or put overrides in the service environment.

Pi can be overridden explicitly:

```bash
JOLT_PI_EXECUTABLE=/absolute/path/to/pi Jolt
```

Version-control executable overrides are documented in [Environment variables](environment-variables.md).

## Harness updates

Each device checks its installed Claude Code, Codex, and Pi versions in the background and publishes device-local update status over RPC. A signed-in desktop also watches the other engine-host devices in its account, including headless machines. Offline device watches retry when the device becomes reachable. **Settings → Harnesses** provides a device selector, manual refresh, installed/latest versions, and inline update state; harness updates do not generate app or system notifications. An **Update** action is offered only when Jolt can prove a supported install path; executable overrides and other unmanaged installs receive manual instructions instead.

Jolt never applies a harness update from a background check. Installation starts only when the user chooses **Update** on that harness row; for a remote device the action sends a typed request to that device's engine.

An accepted update fences new durable commands for only that harness. Persistent processes already parked between turns retire immediately and keep their native resume metadata. Active turns and input requests are never interrupted: the updater waits for them to reach a clean idle boundary, retires the old process, runs the provider-owned updater, verifies `--version`, then releases pending commands against the new executable. Other harnesses and open terminals remain available.

Jolt invokes `claude update` for managed native/npm installs, `codex update`, and `pi update --self`; detected Homebrew installs use `brew upgrade --cask` with a forced cask-API metadata refresh. Update commands are selected by the host engine from the resolved executable; clients never submit arbitrary shell commands. Bounded updater stdout/stderr is recorded in `{data_dir}/logs/jolt-headed.log` or `jolt-headless.log`; failures also include a concise output tail on the device banner.

## Claude Code

Jolt launches Claude Code with streaming JSON input/output and keeps stdin open for steering. It normalizes text and reasoning deltas, tool calls/results, usage, rate-limit events, and final status. Claude's `AskUserQuestion` control request becomes Jolt's question panel. Coding tools are auto-approved for unattended full-access operation.

Harness session IDs are retained independently per harness and working directory on the chat row. Interrupt begins with the CLI's control path and escalates process termination if the child does not exit.

**Accounts:** **Settings → Accounts** can detect, save, activate, forget, and add Claude Code login slots on a selected device. Claude login uses a browser flow with a pasted code. Account quota windows are CLI/provider reports, not Jolt token accounting.

## Codex

Jolt speaks JSON-RPC to `codex app-server`. It starts or resumes a thread, starts turns with the selected model and effort, forces `danger-full-access` with approval policy `never`, and maps item/delta notifications into the common transcript model.

Command and file-change approval requests are auto-approved for unattended operation. Steering and interrupts use Codex's native turn methods. Unexpected exits include a bounded stderr tail in the surfaced error.

**Accounts:** **Settings → Accounts** can manage Codex login slots on a selected device. Jolt opens the device-authorization page on the device running the UI, shows the one-time code there, and polls until the Codex CLI on the selected device writes the new session.

## Pi

Jolt starts Pi in RPC mode in the session working directory. Models are provider-qualified because one Pi installation can expose multiple authenticated providers. Pi supplies its available models and each model's exact thinking-level ladder.

Pi provider credentials remain in Pi's config. Authenticate outside Jolt:

```bash
pi
/login
```

### Project trust

When a folder contains project-local Pi settings, extensions, skills, prompts, or related resources and no saved trust decision applies, Jolt exposes a project-resources choice in the model options.

A choice can apply for one run or be saved to Pi's trust store. Jolt passes the resulting `--approve` or `--no-approve` decision to Pi. Review project extensions before trusting them; they execute with the local user's permissions.

### Tool access

Jolt exposes Pi with either full local tool access or a read-only built-in tool set. Full access inherits the permissions and operating-system isolation of the engine process.

Pi extension UI requests are bridged into Jolt's input surface. Native Pi bash handles `!` and `!!`, including context inclusion semantics.

## Models, reasoning, and options

Model catalogs are fetched from the device that will run the session, so every viewport presents the host's installed models.

The composer persists a concrete model, reasoning level, and harness-specific option map on the chat. Existing chats may change harness as well as model. Jolt defaults coding harnesses to unattended full access; Pi can additionally expose a read-only tool set. Harness-specific examples include context-window, service-tier, tool-access, or project-trust choices. Unsupported or newly added wire fields are treated as additive where possible.

## Steering, switching, and continuation

A non-empty composer during a live steerable run sends a durable `steer` command only when the selected harness matches the active run. A different harness settles the current run before starting the replacement; it is never steered into the old process. The host otherwise routes steering into the active harness mailbox at the harness's supported boundary. If no live run can accept it, the engine can turn it into the next turn rather than discarding user intent.

Cmd+Enter on macOS (Ctrl+Enter elsewhere) instead adds a distinct turn to Jolt's engine-owned queue. Queued turns stay outside the transcript until dispatched, drain together in FIFO order at the next clean turn boundary, and can be cancelled while pending. An interruption or error pauses the queue so it cannot restart work unexpectedly; the composer can resume it explicitly. Because dispatch happens between turns, this behavior is identical across Claude Code, Codex, and Pi.

The harness-native session ID and transcript-coverage cursor are updated after settled turns. Resume is scoped to the harness, host device, and working directory where the CLI conversation was created.

On a first switch, Jolt injects a bounded structured handoff compiled from the privacy-safe synced transcript, active goal, rendered commands, and immutable turn-diff manifests. Returning to a previously used harness resumes its original native conversation and sends only the turns missed since its coverage cursor. Raw tool output and native protocol data remain device-local and never enter the handoff. A rejected native resume invalidates only that harness/cwd generation and retries fresh with a full handoff.

When compaction finishes, Jolt arms a hidden continuation. Any subsequent user or agent message cancels it; if the harness instead settles without resuming, Jolt sends the continuation into the same live session. This guard uses the normalized compaction lifecycle and therefore applies equally to Claude Code, Codex, and Pi.

Jolt goals use the same harness-neutral layer. The engine prepends an active-goal contract without adding it to the visible user message, accounts normalized usage, and schedules hidden continuation turns until the goal completes, blocks, pauses, errors, or reaches its budget. Agents read and mutate only their chat's goal through the `goal_get`, `goal_update`, `goal_complete`, `goal_report_blocked`, `goal_pause`, and `goal_resume` MCP tools. Objective, budget, creation, editing, and clearing remain user-owned through `/goal`; agents cannot resume user- or system-paused goals. Jolt pauses locally hosted active goals after an engine restart rather than silently restarting autonomous work.

Agents can call `request_answers` with typed, single-choice, or multi-choice questions. The tool emits the same normalized input lifecycle as native harness questions, opens Jolt's paged composer UI, waits for the durable `respondInput` command, and returns the selected or typed labels to the calling agent. `/answer` remains the user-invoked extraction flow for questions already emitted in assistant prose.

## Harness secrets

Use **Settings → Secrets** to define an environment variable for one or more harnesses.

- Values are write-only from the UI.
- Values stay in the host OS credential store: macOS Keychain, Windows Credential Manager, or Secret Service on Linux.
- Only labels, environment-variable names, and harness scopes are stored in Jolt's local metadata.
- Values are injected into selected harness child processes only.
- Values never enter the workspace registry, canonical transcript rows, SessionHub, edge relay, or remote RPC surface.

Secrets are device-local. Add the value separately on every device that needs it.

## Normalized transcript privacy

Full tool input is available to the local run journal while the run executes, but the synced render projection keeps only fields required to explain the tool call. For example, file-write contents, edit before/after bodies, arbitrary MCP input, and web-fetch prompts are stripped before entering the session document.

See [Security and data](security.md) for the complete trust boundary.

## Source map

- Harness interface: `crates/harness/src/lib.rs`
- Claude adapter: `crates/harness/src/claude/`
- Codex adapter: `crates/harness/src/codex/`
- Pi adapter: `crates/harness/src/pi/`
- Harness registry: `crates/engine/src/registry.rs`
- Product MCP host: `crates/engine/src/mcp.rs`
- Environment injection: `crates/harness/src/environment.rs`
- Secure secret storage: `crates/engine/src/secrets.rs`

# Agent harnesses

A Jolt harness adapts an installed coding-agent CLI into one normalized stream of text, reasoning, tool, usage, input, steering, compaction, error, and completion events. The engine starts and owns the CLI process on the session's host device.

## Capability overview

| Harness | Integration | Steering | Model discovery | Authentication |
| --- | --- | --- | --- | --- |
| Claude Code | `claude` stream-json and control protocol | Step boundary | CLI control protocol | Claude CLI; account slots can be managed in Jolt |
| Codex | `codex app-server` JSON-RPC | Native `turn/steer` | `model/list` | Codex CLI; account slots can be managed in Jolt |
| Pi | `pi --mode rpc` JSONL | Native RPC steering | Authenticated provider/model catalog | Pi owns provider auth; use Pi's `/login` |

All three support resume, interrupt escalation, normalized usage, images where the CLI supports them, and structured input bridging.

## Executable discovery

Jolt first uses the current process `PATH`, then supplements it with a login-shell PATH snapshot and common Node-version-manager locations. This is important for apps launched from Finder, Dock, launchd, or systemd, which often receive a minimal environment.

A service installed with `jolt daemon install` captures the current `PATH` into its unit. Reinstall the unit after materially changing that PATH, or put overrides in the service environment.

Pi can be overridden explicitly:

```bash
JOLT_PI_EXECUTABLE=/absolute/path/to/pi Jolt
```

Version-control executable overrides are documented in [Environment variables](environment-variables.md).

## Claude Code

Jolt launches Claude Code with streaming JSON input/output and keeps stdin open for steering. It normalizes text and reasoning deltas, tool calls/results, usage, rate-limit events, and final status. Claude's `AskUserQuestion` control request becomes Jolt's question panel.

Harness session IDs are stored on the chat row and resumed only from the same working directory. Interrupt begins with the CLI's control path and escalates process termination if the child does not exit.

**Accounts:** **Settings → Accounts** can detect, save, activate, forget, and add Claude Code login slots on a selected device. Claude login uses a browser flow with a pasted code. Account quota windows are CLI/provider reports, not Jolt token accounting.

## Codex

Jolt speaks JSON-RPC to `codex app-server`. It starts or resumes a thread, starts turns with the selected model, effort, sandbox, and approval policy, and maps item/delta notifications into the common transcript model.

Command and file-change approval requests are answered through Jolt's harness policy. Steering and interrupts use Codex's native turn methods. Unexpected exits include a bounded stderr tail in the surfaced error.

**Accounts:** **Settings → Accounts** can manage Codex login slots on a selected device. Codex login completes through the browser and Jolt polls until the CLI writes the new session.

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

The composer persists a concrete model, reasoning level, sandbox, and harness-specific option map on the chat. Harness-specific examples include context-window, service-tier, tool-access, or project-trust choices. Unsupported or newly added wire fields are treated as additive where possible.

## Steering and continuation

A non-empty composer during a live steerable run sends a durable `steer` command. The host routes it into the active harness mailbox at the harness's supported boundary. If no live run can accept it, the engine can turn it into the next turn rather than discarding user intent.

The harness-native session ID is updated after successful runs. Resume is scoped to the working directory where the CLI conversation was created.

## Harness secrets

Use **Settings → Secrets** to define an environment variable for one or more harnesses.

- Values are write-only from the UI.
- Values stay in the host OS credential store: macOS Keychain, Windows Credential Manager, or Secret Service on Linux.
- Only labels, environment-variable names, and harness scopes are stored in Jolt's local metadata.
- Values are injected into selected harness child processes only.
- Values never enter the workspace registry, Loro documents, edge relay, or remote RPC surface.

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
- Environment injection: `crates/harness/src/environment.rs`
- Secure secret storage: `crates/engine/src/secrets.rs`

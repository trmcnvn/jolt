# Security and data

Jolt coordinates powerful local coding agents. Its security boundary is the signed-in user plus each device's operating-system account.

## Authentication and tenancy

Desktop Jolt runs locally without an account. Production Account scopes use WorkOS AuthKit: the engine is a public client that builds the authorize URL, while the edge Worker holds the WorkOS API key and performs code exchange and refresh. Local scopes never receive an edge token or join registry, session, or device rooms; the public release endpoint remains available for update checks.

Each signed-in user has one hidden Jolt organization:

- zero memberships: Jolt creates `Personal`;
- one membership: Jolt adopts it;
- multiple memberships: Jolt stops with an explicit setup error.

Registry rooms are named from organization and user identity. Session and device rooms are claimed by the authenticated user. The Worker verifies JWTs and stamps identity into Durable Object requests; clients cannot supply the trusted internal identity header directly.

Development auth is intentionally weaker. An edge with `AUTH_MODE=dev` accepts the configured development bearer and should not be exposed as a production service.

## Transport

Production traffic uses HTTPS/WSS. Transcript documents are not end-to-end encrypted at the application layer: the edge can process synchronized document bytes and stores snapshots/backups.

Local engine RPC binds only `127.0.0.1`. It has no additional IPC authentication token, so other processes running as the same machine user—or any process able to reach that loopback port—should be treated as inside the local trust boundary.

The product MCP endpoint is separate from engine RPC. It binds a loopback-only ephemeral port, validates the HTTP host, rejects browser-origin requests, and requires a random bearer credential scoped to one live harness process and chat. Goal and answer tools derive the target chat and live input bridge from that authenticated lease and never accept an arbitrary chat ID. Jolt passes the endpoint and credential through the reserved `JOLT_MCP_URL` and `JOLT_MCP_BEARER_TOKEN` child environment variables where required; they are never persisted, synchronized, relayed, journaled, or logged, and the credential is revoked when the run ends. Pi's bundled bridge captures and removes both variables before harness tools can inherit them.

## Agent execution

Harness subprocesses inherit the engine user's filesystem and process permissions. Jolt intentionally defaults coding harnesses to unattended full-access operation:

- Claude Code tools are auto-approved.
- Codex runs with `danger-full-access` and approval policy `never`.
- Pi defaults to full local tools and can expose a read-only built-in tool set; execution inherits the engine process's operating-system isolation.
- Project-local Pi extensions and settings can execute code after trust is granted.

Use operating-system isolation, containers, or a dedicated user account when a repository is not trusted. A remote viewport does not make execution remote-safe; it asks the host engine to act with that host user's authority.

## Jolt login sessions

The saved Jolt refresh session lives at `{data_dir}/session.json` with owner-only permissions. Refresh tokens rotate, so the engine owns session refresh while running. Standalone `login` and `logout` refuse to mutate the same data directory concurrently.

Local and Account stores live under separate `scopes/` directories. Switching to Local does not sign out, but the relay remains hardwired to Account and cannot route into Local. Signing out returns the viewport to Local and preserves the account cache for the same identity's next sign-in.

## Agent accounts

Claude Code and Codex account switching operates on each CLI's local credential files or native credential location. Jolt stores named local slots so one device can activate a selected account. These values do not sync to other devices.

Pi authentication remains in Pi's config and provider credential store.

## Harness secrets

Harness secrets are a separate first-class store:

- values live in macOS Keychain, Windows Credential Manager, or Linux Secret Service;
- metadata (`label`, environment variable, harness scopes) lives in `harness-secrets.json`;
- values are write-only to the UI and never appear in snapshots or RPC responses;
- secret RPC methods are accepted only by the directly connected engine and rejected by its host relay;
- values are injected only into selected harness child environments;
- values do not modify Jolt's own process environment.

Do not put secret values in labels or environment-variable names.

## Transcripts and tool inputs

Session documents sync user prompts, assistant prose, renderable tool summaries, errors, and structured input state.

Before a tool call enters the document, Jolt strips fields that are unnecessary for rendering, including:

- file-write content;
- edit before/after bodies;
- web-fetch prompts;
- arbitrary MCP and unknown-tool input.

The host's local run journal can retain complete normalized events needed for recovery. Treat the host data directory as sensitive.

## Attachments

Composer images are uploaded in chunks to the host engine. The prompt stores host-local file references plus their SHA-256 content addresses, and supported harnesses receive inline image data. Account-scope upload commits wait for the edge mirror; transcript clients can therefore read the authenticated R2 object when the host is offline. Legacy path-only messages remain host-dependent.

The edge attachment store is:

- content-addressed by SHA-256 within each chat;
- scoped under the authenticated user and chat;
- hash-verified on upload;
- limited to 32 MiB per object;
- served only after authentication;
- deleted with the chat through the registry’s durable artifact-purge queue.

Jolt also caches encoded/decoded images locally for transcript rendering. Signing out on iOS clears identity-scoped document caches; desktop data remains until removed from its data directory.

## Usage and telemetry

Detailed harness usage is written to `{identity_dir}/usage.sqlite` on the host device. It is not embedded in Loro messages or workspace rows.

A viewport can request summaries from reachable devices and merge them for **Usage breakdown**. That RPC includes aggregate token, model, harness, cwd/space, call, session, and provider-reported cost data; it does not transfer the underlying event database.

Jolt's update checker contacts the configured edge release endpoint.

## Workspace and presence metadata

The workspace registry synchronizes device names/platform/version, spaces and paths, chat metadata/configuration, activity previews, checkout identifiers, and session status. Presence is ephemeral but these registry fields are edge-visible current state.

Repository file contents are not stored in the workspace registry. File mention search returns verified relative paths only.

## Logs

Long-running engines log lifecycle, identifiers, sync counters, and bounded subprocess stderr tails. Avoid adding arbitrary prompts, environments, credentials, or tool payloads to diagnostics. Log files under the data directory should be protected like other local application data.

## Operational guidance

- Review Pi project resources before saving trust.
- Run untrusted agents in an isolated OS account or container.
- Keep the loopback RPC port inaccessible from untrusted network namespaces.
- Use **Settings → Secrets** rather than shell profiles when a value should reach only selected harnesses.
- Stop the engine before copying, deleting, or editing its identity/auth stores.
- Use production WorkOS mode for any publicly reachable edge deployment.

# Jolt documentation

Jolt is a native, multi-device ADE for Claude Code, Codex, and Pi. Every computer runs an engine that owns its local agent processes and working folders. Desktop and iOS clients are viewports that can follow and control those engines from anywhere.

## Start here

- [Quickstart](quickstart.md) — install Jolt, connect an agent CLI, and run a first thread.
- [Using Jolt](using-jolt.md) — spaces, threads, the composer, terminals, changes, and usage.
- [Agent harnesses](harnesses.md) — Claude Code, Codex, Pi, model discovery, trust, and harness secrets.
- [CLI and headless engines](cli.md) — login, daemon management, updates, and sync diagnostics.
- [Settings](settings.md) — every desktop settings page, hotkeys, notifications, and local persistence.
- [Mobile feature parity](mobile-parity.md) — required shared capabilities and paired iOS/Android workflow.
- [Jolt for iOS](ios.md) — build and use the native mobile viewport.
- [Android implementation plan](android.md) — proposed Kotlin stack, structure, protocol work, risks, and delivery phases.

## Internals

- [Architecture](architecture.md) — process topology, data ownership, storage, and crate map.
- [Synchronization](sync.md) — workspace registry, Loro session documents, device relay, and recovery.
- [RPC](rpc.md) — envelopes, transports, routing, and the current method surface.
- [Security and data](security.md) — trust boundaries, credentials, attachments, and local-only data.
- [Environment variables](environment-variables.md) — supported runtime configuration and diagnostics.
- [Development](development.md) — build, test, run, package, and source layout.

## Documentation conventions

These pages describe Jolt. Use source links and present-tense behavior when extending them. Important implementation entry points are listed at the end of the internal reference pages so agents can move from a concept to the owning code quickly.

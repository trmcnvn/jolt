# Development

## Prerequisites

Desktop/engine development requires:

- stable Rust with `rustfmt` and Clippy (`rust-toolchain.toml` installs them);
- platform libraries required by gpui;
- Zig 0.16.0 for the normal source-build environment;
- Node.js/npm for `edge/`;
- agent CLIs only when testing their real harness integrations.

The iOS app requires Xcode 26 or newer and the iOS 26 SDK.

## Build and run

Build the desktop binary:

```bash
cargo build -p jolt
./target/debug/Jolt
```

Run a headless engine:

```bash
./target/debug/Jolt headless
```

The binary target is named `Jolt`; packaged installers expose it as the lowercase `jolt` command.

Normal development builds use limited debug information and incremental compilation. For a debugging session that needs full symbols, use the isolated, non-incremental `dbg` profile:

```bash
cargo build --profile dbg -p jolt
./target/dbg/Jolt
```

## Build cache

Inspect Rust build artifacts and reclaim rebuildable caches explicitly:

```bash
scripts/target-cache.sh status
scripts/target-cache.sh clean-incremental       # dry run
scripts/target-cache.sh clean-incremental --yes
scripts/target-cache.sh clean-debug             # dry run of all dev/test artifacts
scripts/target-cache.sh clean-debug --yes
```

Cleanup refuses to run while Cargo or rustc is active. It is never automatic. The demo and E2E scripts warn when `target/` exceeds 20 GiB or its incremental cache exceeds 10 GiB; override those thresholds with `JOLT_TARGET_WARN_GIB` and `JOLT_INCREMENTAL_WARN_GIB`.

## Offline demo

The demo script builds Jolt, starts an isolated mock-harness daemon, seeds spaces/sessions, and opens the desktop viewport:

```bash
scripts/dev-demo.sh
scripts/dev-demo.sh --slow
```

It uses `/tmp/jolt-demo-*` data roots and does not require WorkOS or an edge deployment.

## Local edge

Install edge dependencies and start Wrangler in development-auth mode:

```bash
npm -C edge ci
npm -C edge run dev
```

Run Jolt against it with an isolated data directory:

```bash
JOLT_DATA_DIR=/tmp/jolt-dev \
JOLT_EDGE_URL=http://localhost:27640 \
JOLT_EDGE_TOKEN=alice@org1 \
JOLT_ORG_ID=org1 \
JOLT_WORKOS_CLIENT_ID= \
cargo run -p jolt --bin Jolt
```

The development bearer format is `user@org`. Do not expose a dev-auth Worker publicly.

## Checks

Rust baseline:

```bash
cargo fmt --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Edge baseline:

```bash
npm -C edge run typecheck
npm -C edge test
npm -C edge run build
```

Run the real two-engine/real-edge command path:

```bash
scripts/e2e-smoke.sh
```

The smoke test starts Wrangler, two headless mock-harness engines under the same user, queues a run from device B into a chat hosted by device A, and verifies nudge, execution, transcript sync, and status convergence.

Focused harness tests use fake CLI fixtures and require no provider credentials:

```bash
cargo test -p jolt-harness
```

## iOS

```bash
cd apps/ios
xcodebuild -project Jolt.xcodeproj -scheme Jolt \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Registry merge behavior has Swift tests under `apps/ios/JoltTests`. The app also has demo, benchmark, and E2E launch modes described in [Jolt for iOS](ios.md).

## Packaging

Linux:

```bash
scripts/package-linux.sh
```

Produces `target/package/jolt-<version>-linux-<arch>.tar.gz` with the binary, desktop entry, icon, and per-user installer.

macOS:

```bash
scripts/package-macos.sh
```

Produces a DMG and app tarball. Set `CODESIGN_IDENTITY` to sign. Set the three `NOTARYTOOL_*` values with the signing identity to notarize and staple.

Release tags drive `.github/workflows/release.yml`, which packages Linux and macOS artifacts, builds a SHA-256 manifest, and uploads artifacts to the release R2 bucket. Edge changes deploy through `.github/workflows/deploy.yml` after TypeScript checks and tests.

## Source layout

```text
apps/
  jolt/       CLI and headed/headless binary
  ios/        SwiftUI mobile viewport
crates/
  proto/      shared wire/domain types and view derivations
  doc/        Loro session schema and workspace registry model
  sync/       room clients and local snapshot store
  harness/    Claude Code, Codex, Pi adapters
  engine/     machine-local backend capabilities
  rpc/        transports, envelopes, and device relay
  update/     release check/apply logic
  ui/         gpui desktop application
edge/         Cloudflare Worker, Durable Objects, R2 routes
scripts/      demo, smoke, and packaging entry points
dist/         desktop packaging assets
```

See [Architecture](architecture.md) for ownership boundaries and [Synchronization](sync.md) for wire/data semantics.

## Common change paths

### Add or change an RPC

1. Define or reuse wire types in `crates/proto`.
2. Add the method constant in `crates/rpc/src/lib.rs`.
3. Parse and dispatch it in `crates/engine/src/rpc.rs`.
4. Explicitly decide whether it is relay-forwardable, stream-valued, or local-only.
5. Update desktop/iOS clients and tests.
6. Update [RPC](rpc.md).

### Change workspace rows

Keep Rust (`crates/doc/src/registry.rs`), TypeScript (`edge/src/registry-core.ts`), and Swift (`apps/ios/Jolt/Sync/RegistryCore.swift`) semantics aligned. Update shared test vectors and preserve per-field clock behavior.

### Change session documents

Keep Rust schema encoding compatible with the TypeScript tail/sidecar materializer in `edge/src/session-doc/`. Consider continuation limits, render-part privacy, mixed-version readers, and iOS decoding.

### Change a harness

Keep normalization behind the shared `Harness` trait and emit the common `AgentEvent` model. Preserve cancellation, child-process cleanup, bounded stderr diagnostics, resume scoping, and environment-secret injection.

## Documentation

Write documentation in the present tense. When a change alters a user workflow, data owner, protocol, environment variable, or trust boundary, update the relevant page in `docs/` in the same change. Keep `docs/README.md` as the navigation index and link internal details to their owning source modules.

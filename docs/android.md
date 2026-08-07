# Jolt for Android — implementation plan

> Status: proposed. No Android project exists yet.

The Android app is a native Kotlin parity port of Jolt for iOS, not a separate product track. It must follow the shared [mobile feature parity policy](mobile-parity.md): Android cannot ship capabilities iOS lacks, and mobile feature work updates both native apps together.

Like iOS, Android is a viewport onto the same Jolt account rather than an on-device agent engine. It syncs workspace registry rows and bounded transcript projections from the edge, persists commands before submission, and uses device-relay RPC only when a host computer must touch a filesystem or CLI.

## Product scope

### Parity release

The first releasable Android build must match the complete current iOS capability set:

- Sign in to the existing WorkOS account and apply the zero/one/multiple-organization rule.
- Register a persistent viewer device with `platform: "android"`, publish presence, show host online state, and explain queued offline runs.
- Show attention-sorted sessions across all devices with a searchable space filter and filtered space deep links.
- Add spaces by selecting a host and browsing its folders.
- Start sessions on a selected host with live harness/model/reasoning discovery and Git/Jujutsu ref, reuse, or isolated-checkout planning.
- Open cached sessions offline and stream tail-first transcript projections while foregrounded.
- Render equivalent Markdown, code highlighting, grouped tools, errors, structured input, turn-change summaries, TeX, and Mermaid.
- Send, queue, steer, interrupt, answer structured questions, and recover durable commands after process death.
- Support `/answer`, `/bro`, `!command`, `!!command`, and verified `@` file/directory mentions.
- Pick or paste images where Android supports it, upload them on the first or later turns, and read them back through R2 or host relay.
- Change model, reasoning, and ref for later turns.
- Archive, delete with confirmation, rename where iOS exposes it, copy, and use native context actions.

The app does not run harnesses, terminals, checkout or historical diff viewers, agent-account switching, secrets, or desktop settings locally. Android viewer rows must remain excluded from engine-host, space-owner, harness, VCS, usage-fan-out, and relay-host selectors.

The implementation phases below are development milestones, not independently releasable feature tiers. Source fallbacks and reduced interactions are acceptable only in development builds while closing the iOS parity gap.

### Parity boundary

- No Android-only product capability may ship. A proposed mobile feature must be implemented for iOS and Android together.
- Platform-native presentation may differ: Android can use predictive back, adaptive list-detail layouts, system Photo Picker, and Android accessibility APIs while preserving the same outcomes as iOS.
- Background push notifications require a shared mobile design and delivery path; Android must not add FCM notifications ahead of iOS.
- Android Auto, Wear OS, widgets, and an on-device engine remain out of scope unless equivalent mobile scope is deliberately approved.

## Technical direction

| Area | Choice | Reason |
| --- | --- | --- |
| Language | Kotlin, Java 17 toolchain | Native Android APIs and coroutine support without a second runtime |
| UI | Jetpack Compose + Material 3, themed with Jolt tokens | Native adaptive UI, accessibility, and state-driven rendering |
| State | Unidirectional data flow with `ViewModel` + `StateFlow` | Explicit lifecycle and testable screen state |
| Concurrency | Kotlin coroutines, `Flow`, structured scopes | Fits sockets, local-first repositories, and Compose |
| HTTP/WebSocket | OkHttp | One client supports HTTP, text WebSockets, and binary relay frames |
| JSON | `kotlinx.serialization` with `JsonElement` for open wire fields | Typed DTOs while preserving explicit JSON `null` and unknown additive fields |
| Database | Room + KSP | Transactional registry state and durable command outbox |
| Preferences | DataStore | Small non-secret installation and UI settings |
| Credentials | Android Keystore-backed AES-GCM storage | Keeps refresh tokens out of Room, DataStore, logs, and backups |
| Images | Coil Compose with OkHttp integration | Bounded loading and cache integration for R2/relay attachments |
| Markdown | CommonMark Java plus GFM extensions and a Jolt Compose renderer | Avoids a WebView for ordinary transcript content and permits parity styling |
| Background retry | WorkManager | Durable, network-constrained outbox retry after process death |
| Navigation | Navigation Compose | Native back stack, deep links, and predictive-back integration |
| Dependency injection | Manual `AppContainer` initially | Constructor injection without Hilt/KSP overhead beyond Room |

Use Gradle Kotlin DSL, a checked-in wrapper, and `gradle/libs.versions.toml`. Pin the latest mutually compatible stable Android Gradle Plugin, Kotlin, Compose BOM, and libraries when scaffolding begins rather than recording quickly stale versions in this plan.

Recommended platform baseline:

- `minSdk 26`;
- `compileSdk` and `targetSdk` set to the latest stable SDK available at implementation time;
- application ID and namespace `dev.trmcnvn.jolt.android`;
- edge-to-edge rendering and current Android back-navigation behavior.

Do not begin with Kotlin Multiplatform, a Rust/JNI bridge, Retrofit, Paging 3, Hilt, or SQLCipher. The custom socket protocols make Retrofit a poor fit; the transcript's mutable live tail does not map cleanly to Paging; and full transcript caches are not currently encrypted on other viewports. Revisit these only for a demonstrated need.

## Dependencies

The initial version catalog should contain only the dependencies used by the first implemented slice.

### Runtime

- AndroidX Activity Compose
- Compose BOM: UI, Foundation, Material 3, runtime, tooling preview
- AndroidX Lifecycle runtime Compose and ViewModel Compose
- AndroidX Navigation Compose
- AndroidX Room runtime, Room KTX, and Room compiler via KSP
- AndroidX DataStore Preferences
- AndroidX WorkManager
- AndroidX Browser for the WorkOS Custom Tab
- Kotlin coroutines Android
- Kotlin serialization JSON
- OkHttp
- Coil Compose and Coil's OkHttp network module
- CommonMark core plus table, strikethrough, and autolink extensions
- Material 3 adaptive/window-size APIs when the list-detail layout starts

### Tests and quality

- JUnit
- `kotlinx-coroutines-test`
- Turbine for `Flow` assertions
- OkHttp MockWebServer for HTTP and WebSocket behavior
- Room's in-memory test support
- Compose UI test APIs
- AndroidX Test and Espresso for device tests
- Macrobenchmark and Baseline Profile tooling before release

Use Android Lint as the baseline. Add a separate formatter or static analyzer only if the repository adopts it consistently; do not introduce overlapping rule sets during scaffolding.

## Project structure

Start with a small number of Gradle modules and package features internally. Extract feature modules only when ownership or build times justify them.

```text
apps/android/
  settings.gradle.kts
  build.gradle.kts
  gradle/libs.versions.toml
  gradle/wrapper/
  app/                         # application, navigation, screens, Android resources
    src/main/kotlin/dev/trmcnvn/jolt/android/
      app/                     # JoltApplication, AppContainer, lifecycle wiring
      auth/                    # sign-in route and session state
      home/                    # sessions and searchable space filter
      session/                 # transcript, composer, input requests
      newsession/              # host catalogs, refs, checkout plan
      spaces/                  # space list and remote folder browser
      settings/                # viewer-local settings added later
      design/                  # Jolt tokens and shared Compose components
  core/model/                  # domain models with no Android UI dependency
  core/protocol/               # wire DTOs, HLC merge, relay codec, command encoding
  core/data/                   # Room, files, repositories, OkHttp clients, workers
  benchmark/                   # macrobenchmarks and baseline-profile generation
```

`core/protocol` should remain pure Kotlin/JVM where practical so registry, transcript, command, and binary-codec tests run without an emulator. `core/data` owns Android storage and lifecycle-aware networking. Compose screens consume domain models, not wire DTOs or Room entities.

## Runtime architecture

```text
Compose screen
  → ViewModel exposes StateFlow<UiState>
    → repository performs optimistic local mutation
      → Room transaction / identity-scoped file cache
      → edge HTTP or WebSocket client
      → optional host DeviceRoom RPC
```

Use one application-scoped `AppContainer` to construct the token manager, database factory, OkHttp client, protocol clients, repositories, and workers. Each signed-in identity gets an account-scoped component and storage namespace. Signing out cancels its scopes and workers, closes sockets, removes credentials, and clears identity-scoped registry, transcript, attachment, and outbox data.

Socket clients should be single-writer coroutine actors or otherwise serialize state transitions. UI lifecycle events only request `start`, `kick`, or `stop`; they must not manipulate sockets directly.

## Network and synchronization

Android should use the existing mobile protocols rather than joining complete Loro session documents.

1. **Registry:** JSON text WebSocket to `/registry/{orgId}/ws`, with the same cursor, pending-batch, HLC, presence, probe, and recovery rules as Rust, TypeScript, and Swift.
2. **Transcript:** JSON WebSocket to `/transcript/{chatId}/ws`; authenticated HTTP loads historical pages from `/transcript/{chatId}/page?id=...`.
3. **Commands:** idempotent HTTP submission to `/command/{chatId}`, followed by a best-effort durable nudge to `/device/{deviceId}/nudge`.
4. **Relay RPC:** binary WebSocket to `/device/{deviceId}/ws?role=client&connId=...`, carrying `uleb128(header length) + header JSON + NDJSON RPC payload`.
5. **Attachments:** relay RPC stages host-local uploads; authenticated R2 reads use `/attachments/{chatId}/{sha256}` before falling back to host relay chunks.

There is no supported Loro JVM binding in the current Loro language set, but this is not a parity blocker: registry synchronization is JSON and mobile transcripts and commands use edge projections.

### Local persistence

Use Room transactions for:

- authoritative registry rows, cursor, GC floor, HLC state, and pending operation batches;
- transcript manifests and page metadata;
- durable command outbox records, attempts, expiry, and optimistic message IDs;
- account/device metadata that must update atomically with those records.

Store transcript page bodies as atomic files under an identity/chat namespace, indexed by Room, with a 128 MiB byte-budgeted LRU. Let Coil own a separate bounded encoded image cache. DataStore holds the installation device ID, device display name, and non-secret UI preferences.

A command enqueue must commit before optimistic UI is shown or network submission starts. Foreground code attempts immediate delivery; a unique, network-constrained WorkManager job retries pending commands. Edge acknowledgement marks the command submitted, while the matching host-written message ID retires its optimistic overlay. Nudge failure never reverts an acknowledged command.

### Authentication

Use an Android Custom Tab, never an embedded WebView, for WorkOS AuthKit. Preserve and validate a single-use OAuth `state` value across process recreation. The edge continues to own code exchange, refresh, organization listing, and `Personal` organization creation.

For development, a custom URI scheme is acceptable. Before public distribution, prefer a verified HTTPS App Link and host `assetlinks.json` for every release-signing certificate. This avoids another Android application claiming the callback. Register the final redirect URI in WorkOS.

Keep the refresh token encrypted with an AES-GCM key generated in Android Keystore. Keep the access token in memory and refresh it through a mutex-protected single-flight token manager. Exclude token material and identity caches from Android backup. Never log authorization headers, command bodies, attachment paths, or WebSocket URLs containing bearer query parameters.

## UI translation

| Desktop | Android |
| --- | --- |
| Searchable sidebar and session list | Sessions-first home screen with searchable space filter |
| Device-local tabs | Navigation stack on compact screens; list-detail on wide screens |
| Global New Session canvas | Global New Session destination with space sheet/pane |
| Add-space palette | Host selector and remote folder browser |
| Model/reasoning popovers | Modal bottom sheets |
| Hover actions | Long-press context menus and explicit overflow actions |
| Archive/Delete actions | Swipe actions with undo for archive and confirmation for delete |
| Virtual transcript list | Compose `LazyColumn` with stable row keys and unloaded-page placeholders |
| Turn diff viewer | Collapsed changed-files summary only |

Reuse the Geist fonts and Jolt mark already shipped with iOS, subject to confirming their redistribution metadata. Use Jolt design tokens rather than default dynamic color; support light/dark only when the product palette defines both.

The transcript renderer needs immutable row models and content fingerprints so streaming updates recompose only the mutable tail. Parse Markdown and highlight code on `Dispatchers.Default`, cache results by message/part revision, preserve the user's scroll anchor when an older page is inserted, and follow the tail only when already near the bottom.

Use the system Photo Picker and `content://` streams; request no broad media-storage permission. Sniff image bytes, transcode unsupported formats, enforce the existing count/size limits before retaining full buffers, and avoid decoding full-resolution images merely to show thumbnails.

## Protocol compatibility and testing

The largest maintenance risk is adding a fourth protocol implementation. Before feature work, establish platform-neutral fixtures for:

- registry merge, tombstone, HLC, reseed, and explicit-null behavior;
- registry socket frame tags and reconnect ordering;
- transcript bootstrap, live-page sequence gaps, continuations, and unknown additive fields;
- every durable command payload;
- byte-exact DeviceRoom relay frames, including JSON header key order and ULEB128 boundaries;
- attachment trailer parsing and SHA-256 addressing;
- Markdown and syntax-highlighting cases shared with desktop and iOS.

Kotlin tests should consume the same checked-in JSON/binary fixtures as Rust and TypeScript instead of copying expected values into a fourth test suite. Unknown JSON fields should be ignored where compatibility allows; unknown enum tags that change behavior should fail visibly.

Minimum CI once the project exists:

```bash
./gradlew test lint assembleDebug
./gradlew connectedDebugAndroidTest       # emulator job
./gradlew :benchmark:connectedCheck       # benchmark device job
```

Use API 26 and the current stable API in CI, plus at least one physical-device pass before release. Add MockWebServer tests for process-style reconnects, token refresh, protocol silence, out-of-order command retries, host-offline relay responses, and transcript sequence gaps.

## Principal challenges

1. **Protocol drift:** Rust, TypeScript, Swift, and Kotlin can silently disagree on nullability, numeric widths, enum tags, or binary framing. Shared fixtures are a prerequisite, not cleanup.
2. **Android background limits:** Doze and process death make permanent sockets unreliable. Presence and live transcript streaming are foreground features; Room + WorkManager provide durable command delivery.
3. **Transcript performance:** streaming Markdown, syntax highlighting, variable-height rows, and prepended history can cause excessive recomposition and scroll jumps. Benchmark long and rapidly streaming transcripts early.
4. **OAuth callback security:** a generic custom scheme can be intercepted. Production should use a verified App Link, correct signing fingerprints, state validation, and edge-owned code exchange.
5. **Credential persistence:** AndroidX encrypted-preference APIs have changed over time. Own a small Keystore/AES-GCM wrapper and test key invalidation, reinstall, restore, and lock-screen transitions.
6. **Relay exactness:** header ordering, ULEB128 parsing, NDJSON splitting, reconnect races, and per-call deadlines need byte-level compatibility tests.
7. **Attachment memory:** `content://` providers may not expose a length and camera images can be very large. Stream to a bounded temporary file, hash incrementally, and decode sampled previews.
8. **Rich transcript parity:** Compose text selection, GFM tables, inline math, TeX, and Mermaid do not have one obvious production-grade renderer. Source fallbacks can unblock development, but the Android parity release is blocked until behavior matches iOS through an accepted native or safely isolated renderer.
9. **Device diversity:** IME resizing, predictive back, display cutouts, fold states, font scaling, and OEM socket behavior need explicit test coverage.
10. **Distribution:** Play signing, App Link fingerprints, privacy/data-safety declarations, backup rules, crash reporting policy, and release provenance must be settled before beta.

## Delivery sequence

### 0. Contract and scaffold

- Confirm application ID, minimum SDK, redirect URI, and distribution channel.
- Add the Gradle project and CI cache/check jobs.
- Create shared protocol fixtures and Kotlin conformance tests.
- Implement app configuration, manual dependency container, secure token storage, and debug demo data.
- Treat subsequent mobile feature changes as paired iOS/Android work; iOS receives only fixes and maintenance without an Android counterpart during catch-up.

### 1. Read-only vertical slice

- Sign in, organization setup, persistent Android viewer registration, and registry sync.
- Sessions home, space filtering, presence, and cached offline reopen.
- Transcript bootstrap/live socket, historical page loading, basic Markdown/code/tool rendering.

**Gate:** a production account can open the same long session on desktop, iOS, and Android with matching ordering and reconnect behavior.

### 2. Durable control plane

- Room outbox, immediate submission, WorkManager retries, nudges, and optimistic echoes.
- Run, steer, interrupt, queued/offline status, and structured answers.
- Foreground/background/process-death recovery tests.

**Gate:** killing the app at each enqueue/submission step causes neither command loss nor duplicate host execution.

### 3. Session and space creation

- Device relay client and RPC catalogs.
- Add-space folder browser.
- New-session flow, harness/model/reasoning selection, refs, existing/isolated checkout plans, and file mentions.

### 4. Attachments and interaction parity

- System photo picker, bounded staging, chunked relay upload, R2-first readback, and cache eviction.
- Archive/delete/rename, copy/context actions, syntax-language parity, accessibility, and wide-screen layout.

### 5. Release hardening

- Macrobenchmarks and baseline profile.
- Offline/reconnect soak tests and low-memory testing.
- Verified App Link, release signing, R8, backup exclusions, data-safety review, and beta distribution.

## Decisions to make before scaffolding

Recommended defaults are shown first:

- **Distribution:** Play internal testing, then closed beta; also produce a signed APK for direct testers.
- **Production auth callback:** verified HTTPS App Link; custom scheme only for debug/fallback.
- **Minimum SDK:** API 26 unless audience data justifies a higher floor.
- **MVP form factors:** phones first, but no fixed-width assumptions; wide-screen list-detail in phase 4.
- **Notifications:** no Firebase dependency in the parity release; add mobile push only through a paired iOS/Android design.
- **Math/Mermaid:** permit source fallback only during development; select a safe renderer that reaches iOS-equivalent behavior before beta.

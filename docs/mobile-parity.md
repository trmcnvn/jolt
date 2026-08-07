# Mobile feature parity

Jolt for iOS and Jolt for Android are two native implementations of one mobile product. They must expose the same user capabilities and protocol behavior. Platform conventions may change presentation, but neither app is a separate feature track.

## Required policy

- Android is a parity port of iOS, not an opportunity to add Android-only product features.
- Until Android reaches the current iOS baseline, iOS is the behavioral reference and Android is not release-ready.
- Once Android scaffolding begins, new mobile capabilities must be implemented for both apps in the same change and enabled in the same release train.
- During the catch-up period, iOS may receive platform fixes, security fixes, and maintenance. New iOS capabilities must include their Android implementation rather than increasing the parity gap.
- If equivalent behavior is blocked on either platform, hold the feature from both release builds or reduce both to the same supported behavior.
- Edge, registry, transcript, command, relay, attachment, and rendering contract changes must remain readable by both mobile clients before deployment.

"Same feature" means equivalent outcomes, data durability, offline/recovery behavior, security, and error handling. It does not require pixel-identical controls. Native navigation, sheets, menus, pickers, back behavior, accessibility APIs, and adaptive layout should follow each operating system.

## Shared capability baseline

Both apps must provide:

- WorkOS sign-in, token refresh, the single-organization setup rule, sign-out cleanup, and development auth;
- persistent viewer-device registration, presence, host online state, and queued-offline messaging;
- attention-sorted sessions across devices, searchable space filtering, and filtered space deep links;
- add-space host selection and remote folder browsing;
- new-session creation with host harness/model/reasoning discovery and Git/Jujutsu ref, reuse, or isolated-checkout planning;
- tail-first transcript manifests/pages/live updates, offline page caching, unloaded-history placeholders, and sequence-gap recovery;
- equivalent Markdown, code-language highlighting, grouped tools, errors, structured input, turn-change summaries, TeX, and Mermaid behavior;
- send, queue, steer, interrupt, answer, and durable process-death-safe command submission;
- `/answer`, `/bro`, `!command`, `!!command`, and verified `@` file/directory mentions;
- image selection/paste where the OS supports it, bounded staging, host upload, R2-first readback, and cache cleanup;
- model, reasoning, and ref changes for later turns;
- archive, delete with confirmation, rename where exposed, copy, and context actions;
- equivalent accessibility labels, dynamic text behavior, loading/empty/error states, and demo/test coverage.

Desktop-only terminals, checkout/historical diff viewers, agent-account switching, harness secrets, and desktop settings remain absent from both mobile apps unless deliberately added to both.

## Change workflow

Every mobile-affecting change should:

1. identify whether it changes a capability, wire contract, persistence rule, renderer, or only platform-specific implementation;
2. update shared JSON/binary/golden fixtures first when protocol or rendering behavior changes;
3. implement and test the iOS and Android behavior together;
4. update both platform documents and the shared capability baseline when scope changes;
5. verify mixed-version behavior before deploying an edge change;
6. report explicitly if either platform was not built or tested.

A mobile feature change is incomplete when only one native app implements it. Do not hide a permanent parity gap behind an issue or follow-up task.

## Allowed single-platform changes

A change may target one app only when it does not alter shared user capability, for example:

- an OS-specific crash, lifecycle, IME, accessibility, or rendering bug;
- build tooling, signing, SDK migration, or dependency maintenance;
- performance work that preserves output and behavior;
- a platform-native layout correction;
- tests that close an existing coverage gap.

If a supposed fix changes behavior users can rely on, treat it as a paired mobile change.

## Release gate

An Android beta is blocked until it passes the complete shared capability baseline against the current iOS release. After that point, mobile release notes and acceptance checks are shared: a capability ships only when both apps pass their platform tests and cross-language protocol fixtures.

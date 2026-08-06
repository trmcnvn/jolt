# Jolt for iOS

The native SwiftUI viewport for Jolt. It syncs workspace registry rows and tail-first transcript pages, then controls the computer hosting a session through a durable command outbox and relay RPC. No agent engine or full session document runs on the phone.

## Build

Requires Xcode 26+ and the iOS 26 SDK.

```bash
cd apps/ios
xcodebuild -project Jolt.xcodeproj -scheme Jolt \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `Jolt.xcodeproj` in Xcode and run the shared scheme.

See [`docs/ios.md`](../../docs/ios.md) for capabilities, sign-in, synchronization, demo modes, and source ownership. See [`docs/architecture.md`](../../docs/architecture.md) for the complete Jolt topology.

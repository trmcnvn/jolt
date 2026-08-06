// Tail-first session projection + durable command outbox for one chat. The
// phone never holds the complete Loro session document: transcript pages come
// from the edge, while commands persist locally before the edge idempotently
// appends them to canonical Loro. Optimistic echoes use client-minted message
// ids until the host writes matching transcript entries.

import Foundation
import Loro
import Observation

private let pendingSendOverlayTtlMs: Int64 = 30_000

private struct PendingSendOverlay {
    let messageId: String
    let startedAt: Int64
}

@MainActor
@Observable
final class SessionStore {
    let chatId: String
    /// The chat's host device — nudge target for cold-host command drains.
    var hostDeviceId: String?
    private(set) var entries: [MessageEntry] = []
    private(set) var transcriptManifest: MobileTranscriptManifest?
    private(set) var transcriptPages: [MobileTranscriptPage] = []
    private(set) var loadingHistory = false
    private(set) var historyLoadFailed = false
    @ObservationIgnored private var transcriptSequence: UInt64 = 0
    /// Bumped on every change to `entries` / `pendingSends`. The transcript's
    /// row builder memoizes on it, so a body re-eval that was triggered by
    /// something else (scrolling) costs O(1) instead of re-deriving every row.
    private(set) var revision: UInt64 = 0
    /// Whether this chat's transcript has already been revealed once.
    ///
    /// Lives on the store, not the view: the reveal gate is `@State`, so any
    /// re-creation of TranscriptView reset it to "hidden" and blanked an
    /// already-visible transcript until the settle loop finished. The store is
    /// cached per chat, so it outlives that churn.
    @ObservationIgnored var hasRevealed = false
    /// Transcript parse/row cache — store-owned so parses survive view
    /// churn, and prewarmed off-main whenever a projection lands so opening
    /// the chat never parses markdown inside the first body pass.
    @ObservationIgnored let transcriptCache = TranscriptBuilderCache()
    private(set) var connected = false
    /// Client-minted ids of sends the host hasn't materialized yet.
    private(set) var pendingSends: [(messageId: String, text: String, at: Int64)] = []
    /// Latest command awaiting a matching host-written transcript entry.
    private var pendingSendOverlay: PendingSendOverlay?

    @ObservationIgnored private var projectionClient: TranscriptProjectionClient?
    @ObservationIgnored private var outboxSubmitting: Set<String> = []
    private let config: AppConfig

    /// Demo mode: no room, entries driven externally.
    private let offline: Bool
    /// Demo hook: invoked instead of the command plane when offline.
    @ObservationIgnored var demoResponder: ((String) -> Void)?

    init(chatId: String, config: AppConfig, offline: Bool = false) {
        self.chatId = chatId
        self.config = config
        self.offline = offline
        AttachmentImageCache.shared.configure(config: config)
    }

    // MARK: Attachments (uploads target the chat's host device)

    @ObservationIgnored private var hostRelay: (deviceId: String, client: DeviceRelayClient)?

    private func relayToHost() throws -> DeviceRelayClient {
        guard let hostDeviceId else { throw RelayError.hostOffline }
        if let hostRelay, hostRelay.deviceId == hostDeviceId {
            return hostRelay.client
        }
        let relay = DeviceRelayClient(deviceId: hostDeviceId, config: config)
        hostRelay = (hostDeviceId, relay)
        return relay
    }

    /// Chunked upload of one staged image to the host device; returns the
    /// durable absolute path on that device (what the refs trailer carries).
    func uploadAttachment(name: String, data: Data) async throws -> String {
        try await uploadAttachmentChunked(relay: relayToHost(), chatId: chatId,
                                          name: name, data: data)
    }

    func extractQuestions(sourceMessageId: String) async throws -> ExtractQuestionsResult {
        try await relayToHost().call(
            method: "ExtractQuestions",
            params: ["chatId": chatId, "sourceMessageId": sourceMessageId]
        )
    }

    /// Search the chat's verified checkout on its host. Results contain only
    /// workspace-relative paths; the harness reads contents when needed.
    func searchFiles(query: String) async throws -> [FileSearchMatch] {
        try await relayToHost().call(
            method: "SearchFiles",
            params: ["chatId": chatId, "query": query]
        )
    }

    /// Demo-mode injection point (also used by previews).
    func setEntries(_ new: [MessageEntry]) {
        entries = new
        revision &+= 1
        transcriptCache.prewarm(entries: entries)
    }

    func start() {
        guard projectionClient == nil, !offline else { return }
        if let cached = TranscriptPageDisk.loadBootstrap(chatId: chatId) {
            apply(cached)
        }
        let client = TranscriptProjectionClient(chatId: chatId, config: config)
        projectionClient = client
        client.start { [weak self] event in
            self?.apply(event)
        }
        retryOutbox()
    }

    /// Backgrounding hook: persist immediately.
    func flushToDisk() {
        // Transcript pages and commands are persisted independently; there is
        // no full iOS Loro session snapshot in projection mode.
    }

    /// Foreground hook: revive the room after a suspension (see
    /// RoomClient.kick).
    func kickRoom() {
        projectionClient?.reconnect()
        retryOutbox()
    }

    func stop() {
        projectionClient?.stop()
        projectionClient = nil
        connected = false
    }

    // MARK: Paged projection

    private func apply(_ event: MobileTranscriptEvent) {
        switch event {
        case .bootstrap(let bootstrap):
            apply(bootstrap)
        case .page(let sequence, let page):
            guard sequence == transcriptSequence &+ 1 else {
                kickRoom()
                return
            }
            transcriptSequence = sequence
            if let index = transcriptPages.firstIndex(where: { $0.id == page.id }) {
                transcriptPages[index] = page
            } else {
                transcriptPages.append(page)
            }
            if let descriptor = transcriptManifest?.pages.firstIndex(where: { $0.id == page.id }) {
                transcriptManifest?.pages[descriptor].revision = page.revision
                transcriptManifest?.pages[descriptor].messageCount = page.messages.count
            }
            rebuildProjectedEntries()
        }
    }

    private func apply(_ bootstrap: MobileTranscriptBootstrap) {
        connected = true
        transcriptSequence = bootstrap.sequence
        let incomingIds = Set(bootstrap.pages.map(\.id))
        let valid = Dictionary(uniqueKeysWithValues: bootstrap.manifest.pages.map { ($0.id, $0.revision) })
        let retained = transcriptPages.filter { page in
            !incomingIds.contains(page.id) && valid[page.id] == page.revision
        }
        transcriptManifest = bootstrap.manifest
        transcriptPages = (retained + bootstrap.pages).sorted { $0.firstOrdinal < $1.firstOrdinal }
        rebuildProjectedEntries()
    }

    private func rebuildProjectedEntries() {
        entries = transcriptPages
            .sorted { $0.firstOrdinal < $1.firstOrdinal }
            .flatMap(\.messages)
        let ids = Set(entries.map(\.id))
        pendingSends.removeAll { ids.contains($0.messageId) }
        if let pendingSendOverlay, ids.contains(pendingSendOverlay.messageId) {
            self.pendingSendOverlay = nil
        }
        revision &+= 1
        transcriptCache.prewarm(entries: entries)
    }

    var previousTranscriptPageId: String? {
        guard let first = transcriptPages.min(by: { $0.firstOrdinal < $1.firstOrdinal }) else {
            return nil
        }
        return transcriptManifest?.pages.first(where: { $0.id == first.id })?.previousPageId
    }

    var unloadedTranscriptHeight: CGFloat {
        guard let manifest = transcriptManifest,
              let first = transcriptPages.map(\.firstOrdinal).min() else { return 0 }
        return manifest.pages
            .filter { $0.firstOrdinal < first }
            .reduce(0) { height, page in
                height + min(48_000, max(320, CGFloat(page.messageCount) * 92 + CGFloat(page.estimatedBytes) * 0.18))
            }
    }

    func loadPreviousTranscriptPage() {
        guard !loadingHistory, let pageId = previousTranscriptPageId,
              let client = projectionClient else { return }
        loadingHistory = true
        historyLoadFailed = false
        Task { @MainActor [weak self] in
            guard let self else { return }
            var lastError: Error?
            for delay in [UInt64(0), 250_000_000, 1_000_000_000] {
                if delay > 0 { try? await Task.sleep(nanoseconds: delay) }
                do {
                    let page = try await client.page(id: pageId)
                    if !self.transcriptPages.contains(where: { $0.id == page.id }) {
                        self.transcriptPages.append(page)
                    }
                    self.loadingHistory = false
                    self.historyLoadFailed = false
                    self.rebuildProjectedEntries()
                    return
                } catch {
                    lastError = error
                }
            }
            self.loadingHistory = false
            self.historyLoadFailed = true
            if let lastError {
                roomLog.error("transcript page \(pageId, privacy: .public) failed: \(lastError.localizedDescription, privacy: .public)")
            }
        }
    }

    // MARK: Legacy local decoder (demo/bench fixtures)

    /// Whole-doc decode used only by synthetic demo/benchmark fixtures.
    nonisolated static func decodeEntries(from doc: LoroDoc) -> [MessageEntry]? {
        guard let root = doc.getDeepValue().mapValue else { return nil }
        let raw = (root["messages"]?.listValue ?? []).compactMap(entryFrom)
        return joinContinuations(raw)
    }

    nonisolated private static func entryFrom(_ value: LoroValue) -> MessageEntry? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let roleStr = m["role"]?.stringValue,
              let role = MessageRole(rawValue: roleStr) else { return nil }
        let parts = (m["parts"]?.listValue ?? []).compactMap(partFrom)
        return MessageEntry(id: id, role: role, parts: parts,
                            createdAt: m["createdAt"]?.i64Value ?? 0,
                            deviceId: m["deviceId"]?.stringValue ?? "",
                            status: m["status"]?.stringValue.flatMap(MessageStatus.init(rawValue:)),
                            continuationOf: m["continuationOf"]?.stringValue)
    }

    nonisolated private static func partFrom(_ value: LoroValue) -> MessagePart? {
        guard let m = value.mapValue,
              let id = m["id"]?.stringValue,
              let kind = m["kind"]?.stringValue else { return nil }
        switch kind {
        case "text":
            return .text(id: id, text: m["text"]?.stringValue ?? "")
        case "tool":
            guard let callMap = m["call"]?.mapValue else { return nil }
            let tag = callMap["kind"]?.stringValue ?? "unknown"
            var fields: [String: AnyHashable] = [:]
            for (k, v) in callMap where k != "kind" {
                if let s = v.stringValue { fields[k] = s }
                else if let b = v.boolValue { fields[k] = b }
                else if let i = v.i64Value { fields[k] = i }
                else if let list = v.listValue {
                    // ApplyPatch changes / Todo items — keep a JSON echo.
                    fields[k] = list.map { "\($0.jsonObject)" }
                }
            }
            // isError presence IS the resolution marker (schema.rs:96).
            let isError = m["isError"]?.boolValue
            return .tool(id: id, call: RenderToolCall(tag: tag, fields: fields),
                         isError: isError ?? false, resolved: isError != nil)
        case "input":
            var questions: [UserInputQuestion] = []
            if let list = m["questions"]?.listValue,
               let data = try? JSONSerialization.data(withJSONObject: list.map(\.jsonObject)),
               let decoded = try? JSONDecoder().decode([UserInputQuestion].self, from: data) {
                questions = decoded
            }
            return .input(id: id, requestId: id, questions: questions,
                          resolved: m["resolved"]?.boolValue ?? false)
        case "error":
            return .error(id: id, message: m["message"]?.stringValue ?? "")
        case "changes":
            guard let rawDiff = m["diff"]?.jsonObject,
                  JSONSerialization.isValidJSONObject(rawDiff),
                  let data = try? JSONSerialization.data(withJSONObject: rawDiff),
                  let diff = try? JSONDecoder().decode(TurnDiffSummary.self, from: data) else {
                return nil
            }
            return .changes(id: id, diff: diff)
        default:
            return nil
        }
    }

    /// schema.rs join_continuation_entries: concatenate continuation parts onto
    /// the root in list order; orphans surface standalone.
    nonisolated static func joinContinuations(_ raw: [MessageEntry]) -> [MessageEntry] {
        var roots: [MessageEntry] = []
        var index: [String: Int] = [:]
        for entry in raw {
            if let rootId = entry.continuationOf, let ix = index[rootId] {
                roots[ix].parts.append(contentsOf: entry.parts)
            } else {
                index[entry.id] = roots.count
                roots.append(entry)
            }
        }
        return roots
    }

    // MARK: Derived

    var lastEntryId: String? { entries.last?.id }

    func sendPending(now: Int64 = nowMs()) -> Bool {
        guard let pendingSendOverlay else { return false }
        return now - pendingSendOverlay.startedAt <= pendingSendOverlayTtlMs
    }

    var liveEntry: MessageEntry? {
        entries.last(where: { $0.status == .streaming })
    }

    /// The unresolved input request to surface in the question panel.
    var openInputRequest: (entryId: String, requestId: String, questions: [UserInputQuestion])? {
        for entry in entries.reversed() {
            for part in entry.parts.reversed() {
                // An empty question list can't be answered, so it must not take
                // the composer's place — leaving the user with no way to type.
                if case .input(_, let requestId, let questions, let resolved) = part,
                   !resolved, !questions.isEmpty {
                    return (entry.id, requestId, questions)
                }
            }
        }
        return nil
    }

    // MARK: Command plane (ledger rule 1: append-only, own entries only)

    func sendRun(prompt: String, chat: Chat, attachments: [String] = []) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let sentAt = nowMs()
        let request = runRequest(prompt: prompt, chat: chat, attachments: attachments)
        queueCommand(kind: "run", payload: [
            "kind": "run",
            "request": encodableJSON(request),
            "messageId": messageId,
        ])
        pendingSends.append((messageId, prompt, sentAt))
        beginPendingSend(messageId: messageId, at: sentAt)
        revision &+= 1
    }

    func sendHiddenPrompt(prompt: String, chat: Chat) {
        let request = runRequest(prompt: prompt, chat: chat)
        queueCommand(kind: "run", payload: [
            "kind": "hiddenPrompt",
            "request": encodableJSON(request),
        ])
    }

    func sendBash(command: String, excludeFromContext: Bool, chat: Chat) {
        let messageId = UUID().uuidString.lowercased()
        queueCommand(kind: "bash", payload: [
            "kind": "bash",
            "command": command,
            "excludeFromContext": excludeFromContext,
            "cwd": chat.cwd ?? "",
            "messageId": messageId,
        ])
        beginPendingSend(messageId: messageId, at: nowMs())
    }

    private func runRequest(prompt: String, chat: Chat,
                            attachments: [String] = []) -> RunRequest {
        RunRequest(prompt: prompt,
                   model: chat.config?.model,
                   reasoning: chat.config?.reasoning,
                   modelOptions: chat.config?.modelOptions ?? [:],
                   cwd: chat.cwd ?? "",
                   sandbox: chat.config?.sandbox ?? "workspace-write",
                   attachments: attachments)
    }

    func sendSteer(prompt: String) {
        if offline {
            demoResponder?(prompt)
            return
        }
        let messageId = UUID().uuidString.lowercased()
        let sentAt = nowMs()
        queueCommand(kind: "steer", payload: [
            "kind": "steer",
            "prompt": prompt,
            "messageId": messageId,
        ])
        pendingSends.append((messageId, prompt, sentAt))
        beginPendingSend(messageId: messageId, at: sentAt)
        revision &+= 1
    }

    private func beginPendingSend(messageId: String, at: Int64) {
        pendingSendOverlay = PendingSendOverlay(messageId: messageId, startedAt: at)
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(pendingSendOverlayTtlMs) * 1_000_000)
            guard self?.pendingSendOverlay?.messageId == messageId else { return }
            self?.pendingSendOverlay = nil
        }
    }

    func sendInterrupt() {
        queueCommand(kind: "interrupt", payload: ["kind": "interrupt"])
    }

    func createGoal(objective: String, tokenBudget: UInt64?) {
        var operation: [String: Any] = [
            "action": "create",
            "objective": objective,
        ]
        if let tokenBudget {
            operation["tokenBudget"] = tokenBudget
        } else {
            operation["tokenBudget"] = NSNull()
        }
        sendGoal(operation)
    }

    func editGoal(_ goal: Goal, objective: String, tokenBudget: UInt64?) {
        var operation: [String: Any] = [
            "action": "edit",
            "goalId": goal.id,
            "expectedRevision": goal.revision,
            "objective": objective,
        ]
        if let tokenBudget {
            operation["tokenBudget"] = tokenBudget
        } else {
            operation["tokenBudget"] = NSNull()
        }
        sendGoal(operation)
    }

    func pauseGoal(_ goal: Goal) {
        sendGoal([
            "action": "pause",
            "goalId": goal.id,
            "expectedRevision": goal.revision,
        ])
    }

    func resumeGoal(_ goal: Goal) {
        sendGoal([
            "action": "resume",
            "goalId": goal.id,
            "expectedRevision": goal.revision,
        ])
    }

    func clearGoal(_ goal: Goal) {
        sendGoal([
            "action": "clear",
            "goalId": goal.id,
            "expectedRevision": goal.revision,
        ])
    }

    private func sendGoal(_ operation: [String: Any]) {
        queueCommand(kind: "goal", payload: [
            "kind": "goal",
            "operation": operation,
        ])
    }

    func respondInput(requestId: String, answers: [UserInputAnswer]) {
        queueCommand(kind: "respondInput", payload: [
            "kind": "respondInput",
            "requestId": requestId,
            "answers": answers.map(encodableJSON),
        ])
    }

    /// Persist locally first, then idempotently append to the edge's canonical
    /// Loro command ledger. Acceptance means durable; transcript appearance of
    /// the client-minted message id remains the execution acknowledgement.
    private func queueCommand(kind: String, payload: [String: Any]) {
        let issuedAt = nowMs()
        let commandId = UUID().uuidString.lowercased()
        var command: [String: Any] = [
            "id": commandId,
            "kind": kind,
            "payload": payload,
            "issuedBy": config.deviceId,
            "issuedAt": issuedAt,
            "expiresAt": issuedAt + commandDefaultTtlMs,
        ]
        if let turnId = lastEntryId {
            command["basedOn"] = ["turnId": turnId, "frontier": NSNull()]
        }
        guard persistOutbox(command, id: commandId) else { return }
        submitOutbox(command, id: commandId)
    }

    private var outboxDirectory: URL {
        let directory = FileManager.default.urls(for: .applicationSupportDirectory,
                                                  in: .userDomainMask)[0]
            .appendingPathComponent("JoltCommandOutbox", isDirectory: true)
            .appendingPathComponent(chatId, isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    private func persistOutbox(_ command: [String: Any], id: String) -> Bool {
        guard JSONSerialization.isValidJSONObject(command),
              let data = try? JSONSerialization.data(withJSONObject: command) else { return false }
        do {
            try data.write(to: outboxDirectory.appendingPathComponent("\(id).json"), options: .atomic)
            return true
        } catch {
            roomLog.error("command outbox write failed: \(error.localizedDescription, privacy: .public)")
            return false
        }
    }

    private func retryOutbox() {
        guard let files = try? FileManager.default.contentsOfDirectory(at: outboxDirectory,
                                                                       includingPropertiesForKeys: nil)
        else { return }
        for file in files where file.pathExtension == "json" {
            guard let data = try? Data(contentsOf: file),
                  let command = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let id = command["id"] as? String else { continue }
            submitOutbox(command, id: id)
        }
    }

    private func submitOutbox(_ command: [String: Any], id: String) {
        guard !outboxSubmitting.contains(id), let client = projectionClient else { return }
        outboxSubmitting.insert(id)
        Task { @MainActor [weak self] in
            guard let self else { return }
            var delay: UInt64 = 250_000_000
            while self.outboxSubmitting.contains(id) {
                do {
                    try await client.submit(command: command)
                    try? FileManager.default.removeItem(at: self.outboxDirectory.appendingPathComponent("\(id).json"))
                    self.outboxSubmitting.remove(id)
                    self.nudgeHost()
                    return
                } catch {
                    try? await Task.sleep(nanoseconds: delay)
                    delay = min(delay * 2, 15_000_000_000)
                }
            }
        }
    }

    static func wipeCommandOutbox() {
        let root = FileManager.default.urls(for: .applicationSupportDirectory,
                                             in: .userDomainMask)[0]
            .appendingPathComponent("JoltCommandOutbox", isDirectory: true)
        try? FileManager.default.removeItem(at: root)
    }

    /// Durable-nudge the host device so a cold host opens the doc and drains
    /// (doc_host.rs nudge_remote_host). Fire-and-forget; the command is
    /// durable in the doc regardless.
    private func nudgeHost() {
        guard let hostDeviceId else { return }
        Task { [config, chatId] in
            await config.nudge(deviceId: hostDeviceId, chatId: chatId)
        }
    }
}

private func encodableJSON<T: Encodable>(_ value: T) -> Any {
    guard let data = try? JSONEncoder().encode(value),
          let obj = try? JSONSerialization.jsonObject(with: data) else { return [:] }
    return obj
}

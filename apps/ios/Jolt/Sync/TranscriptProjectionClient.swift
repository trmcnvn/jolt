import Foundation

enum TranscriptPageDisk {
    private static let budgetBytes: Int64 = 128 * 1024 * 1024

    private static var root: URL {
        let url = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("JoltTranscriptPages", isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private static func directory(chatId: String) -> URL {
        let url = root.appendingPathComponent(chatId.replacingOccurrences(of: "/", with: "_"),
                                               isDirectory: true)
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private static func pageURL(chatId: String, pageId: String) -> URL {
        directory(chatId: chatId)
            .appendingPathComponent("page_\(pageId.replacingOccurrences(of: "/", with: "_")).json")
    }

    static func loadBootstrap(chatId: String) -> MobileTranscriptBootstrap? {
        let url = directory(chatId: chatId).appendingPathComponent("bootstrap.json")
        guard let data = try? Data(contentsOf: url),
              let bootstrap = try? JSONDecoder().decode(MobileTranscriptBootstrap.self, from: data)
        else { return nil }
        try? FileManager.default.setAttributes([.modificationDate: Date()],
                                                ofItemAtPath: url.path)
        let pages = bootstrap.pages.map { page in
            loadPage(chatId: chatId, pageId: page.id) ?? page
        }
        return MobileTranscriptBootstrap(sequence: bootstrap.sequence,
                                         manifest: bootstrap.manifest,
                                         pages: pages)
    }

    static func saveBootstrap(_ data: Data, chatId: String) {
        try? data.write(to: directory(chatId: chatId).appendingPathComponent("bootstrap.json"),
                        options: .atomic)
        prune()
    }

    static func loadPage(chatId: String, pageId: String) -> MobileTranscriptPage? {
        let url = pageURL(chatId: chatId, pageId: pageId)
        guard let data = try? Data(contentsOf: url) else { return nil }
        try? FileManager.default.setAttributes([.modificationDate: Date()],
                                                ofItemAtPath: url.path)
        return try? JSONDecoder().decode(MobileTranscriptPage.self, from: data)
    }

    static func savePage(_ data: Data, chatId: String, pageId: String) {
        try? data.write(to: pageURL(chatId: chatId, pageId: pageId), options: .atomic)
        prune()
    }

    static func wipeAll() {
        try? FileManager.default.removeItem(at: root)
    }

    private static func prune() {
        let keys: Set<URLResourceKey> = [.contentModificationDateKey, .fileSizeKey, .isRegularFileKey]
        guard let enumerator = FileManager.default.enumerator(at: root,
                                                               includingPropertiesForKeys: Array(keys)) else { return }
        let files = enumerator.compactMap { $0 as? URL }.compactMap { url -> (URL, Date, Int64)? in
            guard let values = try? url.resourceValues(forKeys: keys), values.isRegularFile == true else { return nil }
            return (url, values.contentModificationDate ?? .distantPast, Int64(values.fileSize ?? 0))
        }
        var total = files.reduce(Int64(0)) { $0 + $1.2 }
        guard total > budgetBytes else { return }
        for file in files.sorted(by: { $0.1 < $1.1 }) where total > budgetBytes {
            try? FileManager.default.removeItem(at: file.0)
            total -= file.2
        }
    }
}

private enum WireJSON: Decodable, Hashable {
    case null
    case bool(Bool)
    case integer(Int64)
    case number(Double)
    case string(String)
    case array([WireJSON])
    case object([String: WireJSON])

    init(from decoder: Decoder) throws {
        let box = try decoder.singleValueContainer()
        if box.decodeNil() { self = .null }
        else if let value = try? box.decode(Bool.self) { self = .bool(value) }
        else if let value = try? box.decode(Int64.self) { self = .integer(value) }
        else if let value = try? box.decode(Double.self) { self = .number(value) }
        else if let value = try? box.decode(String.self) { self = .string(value) }
        else if let value = try? box.decode([WireJSON].self) { self = .array(value) }
        else { self = .object(try box.decode([String: WireJSON].self)) }
    }

    var hashable: AnyHashable? {
        switch self {
        case .null: nil
        case .bool(let value): value
        case .integer(let value): value
        case .number(let value): value
        case .string(let value): value
        case .array(let values): values.compactMap(\.hashable).map { String(describing: $0) }.joined(separator: ",")
        case .object(let values): values.keys.sorted().map { "\($0)=\(String(describing: values[$0]?.hashable))" }.joined(separator: ",")
        }
    }
}

struct MobileTranscriptPageDescriptor: Decodable, Hashable {
    let id: String
    var revision: String
    let firstOrdinal: Int
    var messageCount: Int
    let estimatedBytes: Int
    let previousPageId: String?
    let nextPageId: String?
    let live: Bool
}

struct MobileTranscriptTurnDescriptor: Decodable, Hashable {
    let messageId: String
    let ordinal: Int
    let pageId: String
    let promptPreview: String
    let replyPreview: String?
}

struct MobileTranscriptManifest: Decodable, Hashable {
    let catalogRevision: String
    let totalMessages: Int
    var pages: [MobileTranscriptPageDescriptor]
    let turns: [MobileTranscriptTurnDescriptor]
}

private struct WireMessagePart: Decodable {
    let id: String
    let kind: String
    let text: String?
    let call: [String: WireJSON]?
    let isError: Bool?
    let questions: [UserInputQuestion]?
    let resolved: Bool?
    let message: String?
    let diff: TurnDiffSummary?
}

private struct WireMessageEntry: Decodable {
    let id: String
    let role: MessageRole
    let parts: [WireMessagePart]
    let createdAt: Int64
    let deviceId: String
    let status: MessageStatus?
    let continuationOf: String?

    func messageEntry() -> MessageEntry {
        let decodedParts: [MessagePart] = parts.compactMap { part in
            switch part.kind {
            case "text":
                return .text(id: part.id, text: part.text ?? "")
            case "tool":
                guard let call = part.call else { return nil }
                let tag: String
                if case .string(let value)? = call["kind"] { tag = value } else { tag = "unknown" }
                let fields = call.reduce(into: [String: AnyHashable]()) { result, field in
                    guard field.key != "kind", let value = field.value.hashable else { return }
                    result[field.key] = value
                }
                return .tool(id: part.id, call: RenderToolCall(tag: tag, fields: fields),
                             isError: part.isError ?? false, resolved: part.isError != nil)
            case "input":
                return .input(id: part.id, requestId: part.id,
                              questions: part.questions ?? [], resolved: part.resolved ?? false)
            case "error":
                return .error(id: part.id, message: part.message ?? "")
            case "changes":
                guard let diff = part.diff else { return nil }
                return .changes(id: part.id, diff: diff)
            default:
                return nil
            }
        }
        return MessageEntry(id: id, role: role, parts: decodedParts, createdAt: createdAt,
                            deviceId: deviceId, status: status, continuationOf: continuationOf)
    }
}

struct MobileTranscriptPage: Decodable, Hashable {
    let id: String
    var revision: String
    let firstOrdinal: Int
    private let wireMessages: [WireMessageEntry]

    enum CodingKeys: String, CodingKey { case id, revision, firstOrdinal, wireMessages = "messages" }

    var messages: [MessageEntry] { wireMessages.map { $0.messageEntry() } }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.id == rhs.id && lhs.revision == rhs.revision
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(id)
        hasher.combine(revision)
    }
}

struct MobileTranscriptBootstrap: Decodable {
    let sequence: UInt64
    let manifest: MobileTranscriptManifest
    let pages: [MobileTranscriptPage]
}

private struct TranscriptEnvelope: Decodable {
    let type: String
    let bootstrap: MobileTranscriptBootstrap?
    let sequence: UInt64?
    let page: MobileTranscriptPage?
}

enum MobileTranscriptEvent {
    case bootstrap(MobileTranscriptBootstrap)
    case page(sequence: UInt64, page: MobileTranscriptPage)
}

@MainActor
final class TranscriptProjectionClient {
    private let chatId: String
    private let config: AppConfig
    private var socket: URLSessionWebSocketTask?
    private var task: Task<Void, Never>?
    private var stopped = false

    init(chatId: String, config: AppConfig) {
        self.chatId = chatId
        self.config = config
    }

    func start(events: @escaping (MobileTranscriptEvent) -> Void) {
        guard task == nil else { return }
        stopped = false
        task = Task { [weak self] in await self?.run(events: events) }
    }

    func reconnect() {
        socket?.cancel(with: .goingAway, reason: nil)
    }

    func stop() {
        stopped = true
        task?.cancel()
        task = nil
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
    }

    func page(id: String) async throws -> MobileTranscriptPage {
        guard let token = await config.currentToken() else {
            if let cached = TranscriptPageDisk.loadPage(chatId: chatId, pageId: id) { return cached }
            throw URLError(.userAuthenticationRequired)
        }
        var url = config.edgeURL.appending(path: "transcript/\(chatId)/page")
        url.append(queryItems: [URLQueryItem(name: "id", value: id)])
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
                throw URLError(.badServerResponse)
            }
            let page = try JSONDecoder().decode(MobileTranscriptPage.self, from: data)
            TranscriptPageDisk.savePage(data, chatId: chatId, pageId: id)
            return page
        } catch {
            if let cached = TranscriptPageDisk.loadPage(chatId: chatId, pageId: id) { return cached }
            throw error
        }
    }

    func submit(command: [String: Any]) async throws {
        guard let token = await config.currentToken() else { throw URLError(.userAuthenticationRequired) }
        var request = URLRequest(url: config.edgeURL.appending(path: "command/\(chatId)"))
        request.httpMethod = "POST"
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: command)
        let (_, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) else {
            throw URLError(.badServerResponse)
        }
    }

    private func run(events: @escaping (MobileTranscriptEvent) -> Void) async {
        var delay: UInt64 = 250_000_000
        while !stopped && !Task.isCancelled {
            do {
                guard let url = await config.transcriptSocketURL(chatId: chatId) else {
                    throw URLError(.userAuthenticationRequired)
                }
                let socket = URLSession.shared.webSocketTask(with: url)
                self.socket = socket
                socket.resume()
                delay = 250_000_000
                while !stopped && !Task.isCancelled {
                    let message = try await socket.receive()
                    let data: Data
                    switch message {
                    case .string(let text): data = Data(text.utf8)
                    case .data(let bytes): data = bytes
                    @unknown default: continue
                    }
                    let envelope = try JSONDecoder().decode(TranscriptEnvelope.self, from: data)
                    switch envelope.type {
                    case "bootstrap":
                        guard let bootstrap = envelope.bootstrap else { continue }
                        if let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                           let bootstrapObject = object["bootstrap"],
                           let bootstrapData = try? JSONSerialization.data(withJSONObject: bootstrapObject) {
                            TranscriptPageDisk.saveBootstrap(bootstrapData, chatId: chatId)
                        }
                        events(.bootstrap(bootstrap))
                    case "page":
                        guard let sequence = envelope.sequence, let page = envelope.page else { continue }
                        if let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                           let pageObject = object["page"],
                           let pageData = try? JSONSerialization.data(withJSONObject: pageObject) {
                            TranscriptPageDisk.savePage(pageData, chatId: chatId, pageId: page.id)
                        }
                        events(.page(sequence: sequence, page: page))
                    default:
                        continue
                    }
                }
            } catch {
                socket?.cancel(with: .goingAway, reason: nil)
                socket = nil
                guard !stopped && !Task.isCancelled else { return }
                try? await Task.sleep(nanoseconds: delay)
                delay = min(delay * 2, 15_000_000_000)
            }
        }
    }
}

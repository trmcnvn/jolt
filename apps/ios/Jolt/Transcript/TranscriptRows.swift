// Transcript row model — a port of crates/ui/src/shell/transcript.rs
// rows_for_entry. One row = one markdown top-level block / tool group / chip,
// never one message. Buffered prose creates no rows until its semantic reveal
// marker; live tool updates remeasure only their group.
//
// Stable ids: markdown rows are "{entryId}#{partId}.{blockIx}", tool groups
// "{entryId}#g{groupIx}", chips "{entryId}#{partId}".

import Foundation

enum RowKind {
    case user(text: String, blocks: [TopBlock])
    case markdown(block: MDBlock, streaming: Bool)
    case toolGroup(tools: [ToolItem], active: Bool)
    case inputChip(header: String, resolved: Bool)
    case errorChip(message: String)
    case harnessSwitch(from: String, to: String)
    case changes(diff: TurnDiffSummary)
}

struct ToolItem: Hashable {
    var call: RenderToolCall
    var isError: Bool
    var resolved: Bool
}

struct TranscriptRow: Identifiable {
    var id: String
    /// Content fingerprint — SwiftUI diff key; a changed version re-renders
    /// exactly one row.
    var version: UInt64
    var turnStart: Bool
    var kind: RowKind
    var entryId: String
    var timestamp: Int64?
    /// "{entryId}#{partId}" for markdown rows, nil otherwise — two adjacent
    /// rows sharing it are blocks of the same part (the tighter gap).
    var partKey: String?
    /// Leading gap, resolved at build time. It depends on the PREVIOUS row, so
    /// deriving it in the view body forced an `enumerated()` copy of the whole
    /// row array on every frame; now the body just reads it.
    var topGap: CGFloat = 0
}

/// A settled part's parse, keyed by content so a completed block is parsed
/// once rather than on every rebuild.
struct CompletedParse {
    var source: String
    var blocks: [TopBlock]
}

enum TranscriptRowBuilder {
    /// Split entries into rows. `parsers` caches one incremental parser per
    /// "{entryId}#{partId}" so the streaming tail re-parses O(delta + tail);
    /// `completed` memoizes settled parts so they parse exactly once.
    static func rows(entries: [MessageEntry],
                     pendingSends: [(messageId: String, text: String, at: Int64)],
                     parsers: inout [String: IncrementalMarkdownParser],
                     completed: inout [String: CompletedParse]) -> [TranscriptRow] {
        var rows: [TranscriptRow] = []
        var live = Set<String>()
        for entry in entries {
            rowsForEntry(entry, into: &rows, parsers: &parsers,
                         completed: &completed, live: &live)
        }
        // Optimistic echo: pending sends share their client-minted id, so the
        // host's real entry replaces them without a flicker.
        let ids = Set(entries.map(\.id))
        for pending in pendingSends where !ids.contains(pending.messageId) {
            rows.append(TranscriptRow(id: pending.messageId,
                                      version: fnv1a(pending.text) | 1,
                                      turnStart: true,
                                      kind: .user(text: pending.text,
                                                  blocks: userMarkdownBlocks(pending.text)),
                                      entryId: pending.messageId,
                                      timestamp: nil,
                                      partKey: nil))
        }
        // Drop memos for parts that no longer exist. The count guard keeps the
        // common (append-only) rebuild from copying the dict every token.
        if completed.count > live.count {
            completed = completed.filter { live.contains($0.key) }
        }
        for ix in rows.indices {
            rows[ix].topGap = gap(for: rows[ix],
                                  previous: ix > 0 ? rows[ix - 1] : nil,
                                  isFirst: ix == 0)
        }
        return rows
    }

    private static func gap(for row: TranscriptRow,
                            previous: TranscriptRow?,
                            isFirst: Bool) -> CGFloat {
        if isFirst { return TranscriptView.gapTurn + 10 }
        if row.turnStart { return TranscriptView.gapTurn }
        // Same part ⇒ these are sibling markdown blocks, not a new turn.
        if let key = row.partKey, key == previous?.partKey { return MD.blockGap }
        return TranscriptView.gapBlock
    }

    private static func userMarkdownBlocks(_ raw: String) -> [TopBlock] {
        let text = parseUserMessageImages(raw).text
        return MarkdownParser.parse(projectFileMentions(text).markdownText)
    }

    private static func rowsForEntry(_ entry: MessageEntry,
                                     into rows: inout [TranscriptRow],
                                     parsers: inout [String: IncrementalMarkdownParser],
                                     completed: inout [String: CompletedParse],
                                     live: inout Set<String>) {
        let streaming = entry.status == .streaming
        let settled = entry.status != nil && !streaming

        if entry.role == .user {
            // One bubble row per user message.
            let text = entry.parts.compactMap { part -> String? in
                if case .text(_, let t) = part { return t }
                return nil
            }.joined(separator: "\n")
            guard !text.isEmpty else { return }
            rows.append(TranscriptRow(id: entry.id, version: fnv1a(text),
                                      turnStart: true,
                                      kind: .user(text: text, blocks: userMarkdownBlocks(text)),
                                      entryId: entry.id, timestamp: entry.createdAt,
                                      partKey: nil))
            return
        }

        let entryRowStart = rows.count
        var first = true
        let hasSuccessfulFileMutation = entry.parts.contains { part in
            guard case .tool(_, let call, let isError, let resolved) = part else { return false }
            return resolved && !isError && isFileMutation(call)
        }
        let showChanges = hasSuccessfulFileMutation && entry.parts.contains { part in
            if case .changes = part { return true }
            return false
        }
        var pendingTools: [ToolItem] = []
        var groupIx = 0
        let lastRevealIx = entry.parts.lastIndex { part in
            if case .textReveal = part { return true }
            return false
        }
        let lastContentIx = entry.parts.lastIndex { part in
            if case .textReveal = part { return false }
            return true
        }

        func flushTools(lastIx: Int?) {
            guard !pendingTools.isEmpty else { return }
            let active = streaming && lastIx == lastContentIx
            let id = "\(entry.id)#g\(groupIx)"
            var version = toolFingerprint(pendingTools)
            if active { version ^= 1 }
            rows.append(TranscriptRow(id: id, version: version, turnStart: first,
                                      kind: .toolGroup(tools: pendingTools, active: active),
                                      entryId: entry.id, timestamp: nil, partKey: nil))
            first = false
            pendingTools = []
            groupIx += 1
        }

        for (ix, part) in entry.parts.enumerated() {
            switch part {
            case .tool(_, let call, let isError, let resolved):
                // The filesystem snapshot is authoritative. Once it exists,
                // successful mutation chips are duplicate noise; failed chips
                // remain visible because they still explain the turn.
                if showChanges, resolved, !isError, isFileMutation(call) { continue }
                pendingTools.append(ToolItem(call: call, isError: isError, resolved: resolved))
                if ix == lastContentIx { flushTools(lastIx: ix) }

            case .text(let partId, let text):
                flushTools(lastIx: ix - 1)
                let revealed = !streaming || (lastRevealIx.map { ix < $0 } ?? false)
                guard revealed, !text.isEmpty else { continue }
                let key = "\(entry.id)#\(partId)"
                live.insert(key)
                let blocks = parse(text: text, key: key, streaming: false,
                                   parsers: &parsers, completed: &completed)
                for (blockIx, top) in blocks.enumerated() {
                    rows.append(TranscriptRow(
                        id: "\(key).\(blockIx)", version: top.fingerprint << 1,
                        turnStart: first, kind: .markdown(block: top.block, streaming: false),
                        entryId: entry.id, timestamp: nil, partKey: key))
                    first = false
                }

            case .textReveal:
                break

            case .input(let partId, _, let questions, let resolved):
                flushTools(lastIx: ix - 1)
                let header = questions.first?.header ?? "Question"
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)",
                                          version: fnv1a(header) | (resolved ? 1 : 0),
                                          turnStart: first,
                                          kind: .inputChip(header: header, resolved: resolved),
                                          entryId: entry.id, timestamp: nil, partKey: nil))
                first = false

            case .error(let partId, let message):
                flushTools(lastIx: ix - 1)
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)", version: fnv1a(message),
                                          turnStart: first,
                                          kind: .errorChip(message: message),
                                          entryId: entry.id, timestamp: nil, partKey: nil))
                first = false

            case .harnessSwitch(let partId, let from, let to):
                flushTools(lastIx: ix - 1)
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)",
                                          version: fnv1a("\(from):\(to)"),
                                          turnStart: first,
                                          kind: .harnessSwitch(from: from, to: to),
                                          entryId: entry.id, timestamp: nil, partKey: nil))
                first = false

            case .changes(let partId, let diff):
                guard showChanges else { continue }
                flushTools(lastIx: ix - 1)
                rows.append(TranscriptRow(id: "\(entry.id)#\(partId)",
                                          version: fnv1a(diff.catalogRevision),
                                          turnStart: first, kind: .changes(diff: diff),
                                          entryId: entry.id, timestamp: nil, partKey: nil))
                first = false
            }
        }
        flushTools(lastIx: lastContentIx)
        if settled, rows.count > entryRowStart {
            rows[rows.count - 1].timestamp = entry.createdAt
            rows[rows.count - 1].version ^= 1 << 62
        }
    }

    private static func isFileMutation(_ call: RenderToolCall) -> Bool {
        ["writeFile", "editFile", "applyPatch"].contains(call.tag)
    }

    private static func parse(text: String, key: String, streaming: Bool,
                              parsers: inout [String: IncrementalMarkdownParser],
                              completed: inout [String: CompletedParse]) -> [TopBlock] {
        if streaming {
            let parser = parsers[key] ?? IncrementalMarkdownParser()
            parser.setText(text)
            parsers[key] = parser
            return parser.blocks
        }
        // Completed. Drop the live parser either way, then serve from the memo:
        // rows are rebuilt on every doc update, and re-parsing every settled
        // part each time made a rebuild O(whole transcript) — the dominant cost
        // of opening a long cached session, and paid again per streamed token.
        let handoff = parsers.removeValue(forKey: key)
        if let hit = completed[key], hit.source == text {
            return hit.blocks
        }
        // Adopt the live parser's tree on the live→complete flip, else parse.
        let blocks = handoff?.source == text
            ? (handoff?.blocks ?? MarkdownParser.parse(text))
            : MarkdownParser.parse(text)
        completed[key] = CompletedParse(source: text, blocks: blocks)
        return blocks
    }

    private static func toolFingerprint(_ tools: [ToolItem]) -> UInt64 {
        var hash: UInt64 = 0xcbf29ce484222325
        for tool in tools {
            for byte in tool.call.tag.utf8 {
                hash ^= UInt64(byte)
                hash = hash &* 0x100000001b3
            }
            hash ^= UInt64(tool.call.fields.count) &+ (tool.isError ? 2 : 0) &+ (tool.resolved ? 4 : 0)
            hash = hash &* 0x100000001b3
            for (k, v) in tool.call.fields.sorted(by: { $0.key < $1.key }) {
                for byte in "\(k)=\(v)".utf8 {
                    hash ^= UInt64(byte)
                    hash = hash &* 0x100000001b3
                }
            }
        }
        return hash << 3
    }

    static func fnv1a(_ text: String) -> UInt64 {
        var hash: UInt64 = 0xcbf29ce484222325
        for byte in text.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x100000001b3
        }
        return hash << 1
    }
}

// MARK: - Tool chip content (transcript.rs tool_chip_content_raw)

extension RenderToolCall {
    var chipLabel: String {
        switch tag {
        case "exec": return "Run"
        case "readFile": return "Read"
        case "writeFile": return "Write"
        case "editFile": return "Edit"
        case "applyPatch": return "Patch"
        case "search": return "Search"
        case "glob": return "Glob"
        case "webFetch": return "Fetch"
        case "webSearch": return "Web"
        case "todo": return "Todo"
        case "spawnAgent": return "Agent"
        case "mcp": return "MCP"
        default: return "Tool"
        }
    }

    var chipDetail: String {
        switch tag {
        case "exec": return string("command") ?? ""
        case "readFile":
            let path = shortPath(string("path") ?? "")
            let offset = positiveInt("offset")
            let limit = positiveInt("limit")
            guard offset != nil || limit != nil else { return path }
            let start = offset ?? 1
            if let limit {
                let (sum, overflow) = start.addingReportingOverflow(limit - 1)
                let end = overflow ? Int64.max : sum
                return "\(path):\(start)-\(end)"
            }
            return "\(path):\(start)+"
        case "writeFile", "editFile": return shortPath(string("path") ?? "")
        case "applyPatch":
            let changes = (fields["changes"] as? [String])?.count ?? 0
            return changes == 1 ? "1 file" : "\(changes) files"
        case "search": return string("pattern") ?? ""
        case "glob": return string("pattern") ?? ""
        case "webFetch": return string("url") ?? ""
        case "webSearch": return string("query") ?? ""
        case "todo":
            return string("summary") ?? "task list"
        case "spawnAgent":
            guard let agentType = string("agentType"), !agentType.isEmpty else {
                return "Spawned subagent"
            }
            return "Spawned \(agentType) subagent"
        case "mcp":
            let server = string("server").map { "\($0) · " } ?? ""
            return server + (string("tool") ?? "")
        default: return string("name") ?? ""
        }
    }

    var chipIcon: TablerIcon {
        switch tag {
        case "exec": return .terminal
        case "readFile", "applyPatch": return .fileText
        case "writeFile": return .filePlus
        case "editFile": return .pencil
        case "search": return .search
        case "glob": return .folder
        case "webFetch", "webSearch": return .world
        case "todo": return .listCheck
        case "spawnAgent": return .users
        default: return .apps
        }
    }

    private func positiveInt(_ key: String) -> Int64? {
        guard let value = fields[key] as? Int64, value > 0 else { return nil }
        return value
    }

    private func shortPath(_ path: String) -> String {
        let comps = path.split(separator: "/")
        guard comps.count > 2 else { return path }
        return comps.suffix(2).joined(separator: "/")
    }
}

/// "Ran 3 commands · edited 2 files · 1 failed" (transcript.rs
/// tool_group_summary).
func toolGroupSummary(_ tools: [ToolItem]) -> String {
    var segments: [String] = []
    let runs = tools.filter { $0.call.tag == "exec" }.count
    if runs > 0 { segments.append(runs == 1 ? "ran 1 command" : "ran \(runs) commands") }
    let edits = tools.filter { ["editFile", "writeFile", "applyPatch"].contains($0.call.tag) }.count
    if edits > 0 { segments.append(edits == 1 ? "edited 1 file" : "edited \(edits) files") }
    let reads = tools.filter { $0.call.tag == "readFile" }.count
    if reads > 0 { segments.append(reads == 1 ? "read 1 file" : "read \(reads) files") }
    let searches = tools.filter { ["search", "glob", "webSearch", "webFetch"].contains($0.call.tag) }.count
    if searches > 0 { segments.append(searches == 1 ? "1 search" : "\(searches) searches") }
    let agents = tools.filter { $0.call.tag == "spawnAgent" }.count
    if agents > 0 { segments.append(agents == 1 ? "spawned 1 agent" : "spawned \(agents) agents") }
    let other = tools.count - runs - edits - reads - searches - agents
    if other > 0 { segments.append(other == 1 ? "1 tool" : "\(other) tools") }
    let failed = tools.filter(\.isError).count
    if failed > 0 { segments.append("\(failed) failed") }
    guard var summary = segments.first else { return "\(tools.count) tools" }
    summary = summary.prefix(1).uppercased() + summary.dropFirst()
    return ([summary] + segments.dropFirst()).joined(separator: " · ")
}

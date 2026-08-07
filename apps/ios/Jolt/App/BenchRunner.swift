// Transcript rendering benchmark — launch with `-bench`. Builds a synthetic
// long transcript and measures cold, warm, and revision-cached row projection.

import Foundation

@MainActor
enum BenchRunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("bench.log")
    }

    static func log(_ line: String) {
        print("BENCH: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data("\(line)\n".utf8))
            try? handle.close()
        } else {
            try? Data("\(line)\n".utf8).write(to: logURL)
        }
    }

    static func run() async {
        try? FileManager.default.removeItem(at: logURL)
        for turns in [50, 200, 500] {
            measure(turns: turns)
        }
        log("done")
    }

    private static func measure(turns: Int) {
        var entries: [MessageEntry] = []
        let build = time { entries = syntheticEntries(turns: turns) }

        var rowCount = 0
        let cold = best(3) {
            var parsers: [String: IncrementalMarkdownParser] = [:]
            var memo: [String: CompletedParse] = [:]
            let rows = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                                 parsers: &parsers, completed: &memo)
            rowCount = rows.count
        }

        var parsers: [String: IncrementalMarkdownParser] = [:]
        var memo: [String: CompletedParse] = [:]
        _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                      parsers: &parsers, completed: &memo)
        let warm = best(5) {
            _ = TranscriptRowBuilder.rows(entries: entries, pendingSends: [],
                                          parsers: &parsers, completed: &memo)
        }

        let cache = TranscriptBuilderCache()
        _ = cache.rows(revision: 1, entries: entries, pendingSends: [])
        let cached = best(5) {
            _ = cache.rows(revision: 1, entries: entries, pendingSends: [])
        }

        log("--- \(turns) turns · \(entries.count) entries · \(rowCount) rows")
        log(String(format: "fixture build              %8.2f ms", build))
        log(String(format: "row build cold             %8.2f ms", cold))
        log(String(format: "row build warm             %8.2f ms   %.0fx", warm, cold / max(warm, 0.0001)))
        log(String(format: "scroll frame               %8.4f ms   %.0fx", cached, cold / max(cached, 0.0001)))
    }

    private static func time(_ body: () -> Void) -> Double {
        let start = CFAbsoluteTimeGetCurrent()
        body()
        return (CFAbsoluteTimeGetCurrent() - start) * 1_000
    }

    private static func best(_ count: Int, _ body: () -> Void) -> Double {
        var lowest = Double.greatestFiniteMagnitude
        for _ in 0..<count { lowest = min(lowest, time(body)) }
        return lowest
    }

    /// A large transcript for the `-big` demo route.
    static func syntheticEntries(turns: Int) -> [MessageEntry] {
        (0..<turns).flatMap { index in
            [
                MessageEntry(
                    id: "u\(index)",
                    role: .user,
                    parts: [.text(id: "t0", text: "Turn \(index): the ref dropdown still hangs on open — dig into it.")],
                    createdAt: Int64(index * 1_000),
                    deviceId: "bench",
                    status: .complete,
                    continuationOf: nil
                ),
                MessageEntry(
                    id: "a\(index)",
                    role: .assistant,
                    parts: assistantParts(index: index),
                    createdAt: Int64(index * 1_000 + 1),
                    deviceId: "dev-mac",
                    status: .complete,
                    continuationOf: nil
                ),
            ]
        }
    }

    private static func assistantParts(index: Int) -> [MessagePart] {
        var parts: [MessagePart] = [.text(id: "t0", text: prose(index))]
        for tool in 0..<4 {
            let callIndex = index * 4 + tool
            parts.append(.tool(id: "k\(index).\(tool)", call: toolCall(callIndex),
                               isError: callIndex % 17 == 0, resolved: true))
        }
        parts.append(.text(id: "t1", text: closing(index)))
        return parts
    }

    private static func toolCall(_ index: Int) -> RenderToolCall {
        switch index % 4 {
        case 0:
            RenderToolCall(tag: "exec", fields: ["command": "rg -n 'render_branch_popover' crates/ui/src"])
        case 1:
            RenderToolCall(tag: "readFile", fields: ["path": "crates/ui/src/pickers.rs"])
        case 2:
            RenderToolCall(tag: "editFile", fields: ["path": "crates/ui/src/pickers.rs"])
        default:
            RenderToolCall(tag: "search", fields: ["pattern": "render_branch_popover\\("])
        }
    }

    private static func prose(_ index: Int) -> String {
        """
        ## Pass \(index): where the dropdown stalls

        The dropdown's open handler awaits `loadRefs()` **before** it paints, so
        the menu can't render until the full ref index resolves. On a repo with
        many refs that's a visible hang, and it is paid again on every open
        because the result is never memoized between mounts.

        1. `loadRefs()` walks every ref and builds a fresh array each call
        2. The handler `await`s it inline instead of rendering an empty menu
        3. `useRefIndex` has no cache, so remount re-does the whole walk

        | Stage | Cost | Cached |
        | --- | --- | --- |
        | `loadRefs` | O(refs) | no |
        | `useRefIndex` | O(refs) | no |
        | paint | O(visible) | n/a |

        ```ts
        const refs = useRefIndex()
        useEffect(() => { void warmRefIndex() }, [])
        return <Menu items={refs ?? []} loading={refs == null} />
        ```
        """
    }

    private static func closing(_ index: Int) -> String {
        """
        Landed the pass-\(index) change behind `refIndexCache`. Open latency drops to
        a paint, and the index warms in the background on first hover.
        """
    }
}

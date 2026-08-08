import XCTest
@testable import Jolt

final class TranscriptToolGroupTests: XCTestCase {
    func testCollapsedActiveGroupPreviewsOnlyLatestTool() {
        let tools = [
            ToolItem(call: RenderToolCall(tag: "readFile", fields: ["path": "a.swift"]),
                     isError: false, resolved: true),
            ToolItem(call: RenderToolCall(tag: "exec", fields: ["command": "swift test"]),
                     isError: false, resolved: false),
        ]

        let preview = visibleToolRange(toolCount: tools.count, open: false, active: true)
        let hidden = visibleToolRange(toolCount: tools.count, open: false, active: false)
        let expanded = visibleToolRange(toolCount: tools.count, open: true, active: true)

        XCTAssertEqual(preview.map { tools[$0].call.tag }, ["exec"])
        XCTAssertTrue(hidden.isEmpty)
        XCTAssertEqual(expanded.map { tools[$0].call.tag }, ["readFile", "exec"])
    }

    func testBufferedAssistantTextAppearsOnlyAfterRevealBoundary() {
        let buffered = MessageEntry(
            id: "assistant-1",
            role: .assistant,
            parts: [.text(id: "t0", text: "Complete thought")],
            createdAt: 1,
            deviceId: "device-1",
            status: .streaming,
            continuationOf: nil
        )
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]

        let hidden = TranscriptRowBuilder.rows(entries: [buffered], pendingSends: [],
                                                parsers: &parsers, completed: &completed)
        XCTAssertTrue(hidden.isEmpty)

        var revealed = buffered
        revealed.parts.append(.textReveal(id: "r1"))
        let visible = TranscriptRowBuilder.rows(entries: [revealed], pendingSends: [],
                                                 parsers: &parsers, completed: &completed)
        XCTAssertEqual(visible.count, 1)
        guard case .markdown(_, let streaming) = visible[0].kind else {
            return XCTFail("Expected completed Markdown")
        }
        XCTAssertFalse(streaming)
    }

    func testToolBoundaryRevealsPriorProseButNotTheNewTail() {
        let entry = MessageEntry(
            id: "assistant-1",
            role: .assistant,
            parts: [
                .text(id: "t0", text: "Before tool"),
                .textReveal(id: "r1"),
                .tool(id: "read-1",
                      call: RenderToolCall(tag: "readFile", fields: ["path": "a.swift"]),
                      isError: false, resolved: true),
                .text(id: "t3", text: "After tool"),
            ],
            createdAt: 1,
            deviceId: "device-1",
            status: .streaming,
            continuationOf: nil
        )
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]

        let rows = TranscriptRowBuilder.rows(entries: [entry], pendingSends: [],
                                               parsers: &parsers, completed: &completed)

        XCTAssertEqual(rows.count, 2)
        guard case .markdown = rows[0].kind else {
            return XCTFail("Expected revealed Markdown before tool")
        }
        guard case .toolGroup = rows[1].kind else {
            return XCTFail("Expected tool group after Markdown")
        }
    }

    func testTrailingStreamingToolGroupIsActiveWithoutOpeningByDefault() {
        let entry = MessageEntry(
            id: "assistant-1",
            role: .assistant,
            parts: [
                .text(id: "t0", text: "Buffered prose"),
                .tool(id: "read-1",
                      call: RenderToolCall(tag: "readFile", fields: ["path": "a.swift"]),
                      isError: false, resolved: true),
                .tool(id: "exec-1",
                      call: RenderToolCall(tag: "exec", fields: ["command": "swift test"]),
                      isError: false, resolved: false),
            ],
            createdAt: 1,
            deviceId: "device-1",
            status: .streaming,
            continuationOf: nil
        )
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]

        let rows = TranscriptRowBuilder.rows(entries: [entry], pendingSends: [],
                                               parsers: &parsers, completed: &completed)

        XCTAssertEqual(rows.count, 1)
        guard case .toolGroup(let tools, let active) = rows[0].kind else {
            return XCTFail("Expected a tool group")
        }
        XCTAssertTrue(active)
        let preview = visibleToolRange(toolCount: tools.count, open: false, active: active)
        XCTAssertEqual(preview.map { tools[$0].call.tag }, ["exec"])
    }
}

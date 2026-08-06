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

    func testTrailingStreamingToolGroupIsActiveWithoutOpeningByDefault() {
        let entry = MessageEntry(
            id: "assistant-1",
            role: .assistant,
            parts: [
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

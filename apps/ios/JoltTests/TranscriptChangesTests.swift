import XCTest
@testable import Jolt

final class TranscriptChangesTests: XCTestCase {
    func testTurnDiffSummaryDecodesFromFullManifest() throws {
        let data = Data(#"""
        {
            "catalogRevision":"catalog-1",
            "chatId":"chat-1",
            "assistantMessageId":"assistant-1",
            "deviceId":"device-1",
            "cwd":"/tmp/repo",
            "vcs":"git",
            "files":[{
                "id":"file-1",
                "path":"Sources/App.swift",
                "status":"modified",
                "additions":8,
                "deletions":3,
                "binary":false,
                "rowCount":12,
                "estimatedBytes":512,
                "completeness":"complete",
                "pageIds":["page-1"]
            }],
            "pages":[],
            "additions":8,
            "deletions":3,
            "truncated":false,
            "completedAt":"2026-08-07T00:00:00Z"
        }
        """#.utf8)

        let diff = try JSONDecoder().decode(TurnDiffSummary.self, from: data)

        XCTAssertEqual(diff.catalogRevision, "catalog-1")
        XCTAssertEqual(diff.files.map(\.path), ["Sources/App.swift"])
        XCTAssertEqual(diff.additions, 8)
        XCTAssertEqual(diff.deletions, 3)
    }

    func testChangesRowReplacesSuccessfulMutationChips() {
        let diff = TurnDiffSummary(
            catalogRevision: "catalog-1",
            files: [
                TurnDiffFileSummary(id: "file-1", path: "Sources/App.swift",
                                    additions: 8, deletions: 3),
            ],
            additions: 8,
            deletions: 3,
            truncated: false
        )
        let entry = MessageEntry(
            id: "assistant-1",
            role: .assistant,
            parts: [
                .tool(id: "edit-1",
                      call: RenderToolCall(tag: "editFile", fields: ["path": "Sources/App.swift"]),
                      isError: false, resolved: true),
                .tool(id: "write-failed",
                      call: RenderToolCall(tag: "writeFile", fields: ["path": "Sources/Other.swift"]),
                      isError: true, resolved: true),
                .changes(id: "changes", diff: diff),
            ],
            createdAt: 1,
            deviceId: "device-1",
            status: .complete,
            continuationOf: nil
        )
        var parsers: [String: IncrementalMarkdownParser] = [:]
        var completed: [String: CompletedParse] = [:]

        let rows = TranscriptRowBuilder.rows(entries: [entry], pendingSends: [],
                                               parsers: &parsers, completed: &completed)

        XCTAssertEqual(rows.count, 2)
        guard case .toolGroup(let tools, _) = rows[0].kind else {
            return XCTFail("Expected the failed mutation chip")
        }
        XCTAssertEqual(tools.map(\.call.tag), ["writeFile"])
        guard case .changes(let rowDiff) = rows[1].kind else {
            return XCTFail("Expected the authoritative Changes row")
        }
        XCTAssertEqual(rowDiff, diff)
    }
}

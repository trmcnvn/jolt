import SwiftUI
import XCTest
@testable import Jolt

final class ComposerBehaviorTests: XCTestCase {
    func testShellCommandPrefixes() {
        XCTAssertEqual(parseShellCommand("! cargo test"),
                       ShellCommand(command: "cargo test", excludeFromContext: false))
        XCTAssertEqual(parseShellCommand("  !!pwd  "),
                       ShellCommand(command: "pwd", excludeFromContext: true))
        XCTAssertNil(parseShellCommand("!"))
        XCTAssertNil(parseShellCommand("!!"))
        XCTAssertNil(parseShellCommand("!!!echo ordinary prompt"))
        XCTAssertFalse(hasShellPrefix("!!!echo ordinary prompt"))
    }

    func testJujutsuRefPreservesWireRevisionAndKind() throws {
        let data = Data(#"{"name":"Working copy · abcdef12","revision":"@","kind":"workingCopy","current":true}"#.utf8)
        let ref = try JSONDecoder().decode(RepoRef.self, from: data)

        XCTAssertEqual(ref.id, "@")
        XCTAssertEqual(ref.kind, .workingCopy)
        XCTAssertTrue(ref.isJujutsu)
    }

    func testChatConfigPreservesModelOptions() throws {
        let data = Data(#"{"harness":"pi","model":"provider/model","reasoning":"high","modelOptions":{"projectTrust":"trust","toolAccess":"readOnly"},"sandbox":"workspace-write"}"#.utf8)
        let config = try JSONDecoder().decode(ChatConfig.self, from: data)
        let encoded = try JSONEncoder().encode(config)
        let roundTrip = try JSONDecoder().decode(ChatConfig.self, from: encoded)

        XCTAssertEqual(roundTrip.modelOptions["projectTrust"], .string("trust"))
        XCTAssertEqual(roundTrip.modelOptions["toolAccess"], .string("readOnly"))
    }

    func testFileMentionTokenRequiresBoundary() {
        XCTAssertEqual(fileMentionToken(in: "open @comp", cursorOffset: 10),
                       FileMentionToken(range: 5..<10, query: "comp"))
        XCTAssertNil(fileMentionToken(in: "name@example.com", cursorOffset: 16))
        XCTAssertEqual(fileMentionToken(in: "(@src now", cursorOffset: 5),
                       FileMentionToken(range: 1..<5, query: "src"))
    }

    func testFileMentionSelectionFromPreviousTextFallsBackSafely() {
        let previous = "previous value"
        let stale = TextSelection(insertionPoint: previous.endIndex)
        XCTAssertEqual(fileMentionCursorOffset(in: "", selection: stale), 0)
    }

    func testFileMentionWireEncodingAndProjection() {
        let link = fileMentionLink(path: "src/a file#[x].rs", isDirectory: false)
        XCTAssertEqual(link, #"[a file#\[x\].rs](jolt-file:src/a%20file%23%5Bx%5D.rs)"#)

        let projected = projectFileMentions("Review \(link) before sending")
        XCTAssertEqual(projected.plainText, "Review @a file#[x].rs before sending")
        XCTAssertEqual(projected.markdownText, "Review `@a file#[x].rs` before sending")
    }

    func testUserMarkdownParsesInlineAndFencedCode() {
        let blocks = MarkdownParser.parse("Run `cargo test`:\n\n```swift\nprint(\"ok\")\n```")

        XCTAssertEqual(blocks.count, 2)
        guard case .paragraph(let runs) = blocks[0].block else {
            return XCTFail("Expected paragraph")
        }
        XCTAssertTrue(runs.contains { $0.text == "cargo test" && $0.style.code })
        guard case .codeBlock(let language, let code) = blocks[1].block else {
            return XCTFail("Expected code block")
        }
        XCTAssertEqual(language, "swift")
        XCTAssertEqual(code, "print(\"ok\")")
    }

    @MainActor
    func testSelectedMentionEncodesOnlyOnSubmission() {
        let state = FileMentionDraft()
        state.update(text: "Review @comp", selection: nil, contextKey: "chat") { _ in [] }
        let insertion = state.accept(
            FileSearchMatch(path: "crates/ui/src/composer.rs", isDir: false),
            in: "Review @comp"
        )

        XCTAssertEqual(insertion?.text, "Review @composer.rs ")
        XCTAssertEqual(state.encodedPrompt(insertion?.text ?? ""),
                       "Review [composer.rs](jolt-file:crates/ui/src/composer.rs) ")

        let prefixed = "Please " + (insertion?.text ?? "")
        XCTAssertEqual(state.encodedPrompt(prefixed),
                       "Please Review [composer.rs](jolt-file:crates/ui/src/composer.rs) ")

        let edited = prefixed.replacingOccurrences(of: "@composer.rs", with: "@other.rs")
        XCTAssertEqual(state.encodedPrompt(edited), edited)
    }
}

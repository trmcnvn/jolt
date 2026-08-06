import SwiftUI
import XCTest
import RaTeX
import BeautifulMermaid
@testable import Jolt

final class ComposerBehaviorTests: XCTestCase {
    @MainActor
    func testNativeComposerPasteRoutesClipboardImage() {
        let pasteboard = UIPasteboard.general
        let previousItems = pasteboard.items
        defer { pasteboard.items = previousItems }
        pasteboard.image = UIGraphicsImageRenderer(size: CGSize(width: 2, height: 2)).image {
            $0.cgContext.setFillColor(UIColor.red.cgColor)
            $0.cgContext.fill(CGRect(origin: .zero, size: CGSize(width: 2, height: 2)))
        }

        let textView = ImagePasteTextView()
        var pastedProviders: [NSItemProvider] = []
        textView.onPasteImages = { pastedProviders = $0 }

        XCTAssertTrue(textView.canPerformAction(#selector(ImagePasteTextView.paste(_:)),
                                                withSender: nil))
        textView.paste(nil)
        XCTAssertEqual(pastedProviders.count, 1)
    }

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

    func testGoalCommandRequiresExactInvocation() {
        XCTAssertTrue(isGoalCommand("/goal"))
        XCTAssertFalse(isGoalCommand("/goal pause"))
        XCTAssertFalse(isGoalCommand("/goal --tokens 12000 finish the migration"))
        XCTAssertFalse(isGoalCommand("/goalkeeper"))
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

    func testMarkdownParsesDollarAndTexMath() {
        let blocks = MarkdownParser.parse(
            #"Euler $e^{i\pi}+1=0$ and \(x^2\). Then $$\sum_i i$$."#
        )
        guard case .paragraph(let runs) = blocks.first?.block else {
            return XCTFail("Expected paragraph")
        }
        let math = runs.compactMap { run in
            run.style.math.map { ($0, run.text) }
        }
        XCTAssertEqual(math.count, 3)
        XCTAssertEqual(math[0].0, .inline)
        XCTAssertEqual(math[0].1, #"e^{i\pi}+1=0"#)
        XCTAssertEqual(math[1].0, .inline)
        XCTAssertEqual(math[1].1, "x^2")
        XCTAssertEqual(math[2].0, .display)
        XCTAssertEqual(math[2].1, #"\sum_i i"#)
    }

    func testMathBodyIsNotParsedAsMarkdown() {
        let blocks = MarkdownParser.parse(
            #"Both $a*b*c$ and \( x_[i] * y \), then $$ z_1 * z_2 $$ and $a$$b$."#
        )
        guard case .paragraph(let runs) = blocks.first?.block else {
            return XCTFail("Expected paragraph")
        }
        let math = runs.filter { $0.style.math != nil }
        XCTAssertEqual(math.map(\.text), ["a*b*c", " x_[i] * y ", " z_1 * z_2 ", "a", "b"])
    }

    func testMarkdownConvertsOnlyClosedMultilineDisplayMath() {
        let closed = MarkdownParser.parse("$$\n\\frac{1}{2}\n$$")
        guard case .codeBlock(let language, let code) = closed.first?.block else {
            return XCTFail("Expected a math block")
        }
        XCTAssertEqual(language, "math")
        XCTAssertEqual(code, #"\frac{1}{2}"#)

        let open = MarkdownParser.parse("$$\n\\frac{1}{2}")
        XCTAssertFalse(open.contains { top in
            if case .codeBlock(let language, _) = top.block { return language == "math" }
            return false
        })
    }

    func testMathDelimitersRemainLiteralInsideCode() {
        let blocks = MarkdownParser.parse(
            "open \\(x and `$a*b*c$ \\(y\\)`\n\n```txt\n$$\na*b*c\n$$\n\\[z\\]\n```"
        )
        guard case .paragraph(let runs) = blocks[0].block else {
            return XCTFail("Expected paragraph")
        }
        XCTAssertTrue(runs.allSatisfy { $0.style.math == nil })
        XCTAssertEqual(runs.map(\.text).joined(), "open \\(x and $a*b*c$ \\(y\\)")
        guard case .codeBlock(_, let code) = blocks[1].block else {
            return XCTFail("Expected code block")
        }
        XCTAssertEqual(code, "$$\na*b*c\n$$\n\\[z\\]")
    }

    func testMarkdownWrapperKeepsNestedRichFencesLiteral() {
        let source = "```markdown\nInline $e^{i\\pi}+1=0$\n\n```math\n\\frac{1}{2}\n```\n\n```mermaid\nflowchart LR\n A --> B\n```\n```"
        let blocks = MarkdownParser.parse(source)
        XCTAssertEqual(blocks.count, 1)
        guard case .codeBlock(let language, let code) = blocks.first?.block else {
            return XCTFail("Expected one literal Markdown code block")
        }
        XCTAssertEqual(language, "markdown")
        XCTAssertTrue(code.contains("Inline $e^{i\\pi}+1=0$"))
        XCTAssertTrue(code.contains("```math\n\\frac{1}{2}\n```"))
        XCTAssertTrue(code.contains("```mermaid\nflowchart LR\n A --> B\n```"))
    }

    func testNativeRichRenderEnginesAcceptChatExamples() throws {
        let formula = try RaTeXEngine.shared.parse(
            #"\frac{-b \pm \sqrt{b^2-4ac}}{2a}"#,
            displayMode: true,
            color: UIColor.white
        )
        XCTAssertGreaterThan(formula.width, 0)
        XCTAssertGreaterThan(formula.height + formula.depth, 0)

        let diagram = MermaidDiagram(
            source: "flowchart LR\n A[Start] --> B{Ready?}\n B -->|Yes| C[Done]",
            theme: .zincDark
        )
        XCTAssertNil(diagram.parseError)
        XCTAssertGreaterThan(diagram.diagramBounds.width, 0)
        XCTAssertGreaterThan(diagram.diagramBounds.height, 0)
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

import XCTest
@testable import Jolt

final class HighlightTests: XCTestCase {
    func testCommonLanguageTagsResolve() {
        let tags: [(String, HighlightLanguage)] = [
            ("c", .c),
            ("hpp", .cpp),
            ("java", .java),
            ("c#", .cSharp),
            ("kts", .kotlin),
            ("swift", .swift),
            ("rb", .ruby),
            ("lua", .lua),
            ("php", .php),
            ("sql", .sql),
            ("yml", .yaml),
            ("htm", .html),
            ("css", .css),
            ("Dockerfile", .dockerfile),
        ]

        for (tag, expected) in tags {
            XCTAssertEqual(HighlightLanguage.forTag(tag), expected, tag)
        }
    }

    func testCommonLanguageProfilesHighlightRepresentativeSyntax() {
        let samples: [(HighlightLanguage, String, String)] = [
            (.c, "int main(void) { return 0; }", "return"),
            (.cpp, "class Widget { public: bool ready; };", "class"),
            (.java, "public record User(String name) {}", "record"),
            (.cSharp, "public record User(string Name);", "record"),
            (.kotlin, "data class User(val name: String)", "data"),
            (.swift, "struct User { let name: String }", "struct"),
            (.ruby, "class User; def name; nil; end; end", "def"),
            (.lua, "local function load() return nil end", "local"),
            (.php, "public function load(): ?string { return null; }", "function"),
            (.sql, "SELECT * FROM users WHERE id = 42", "SELECT"),
            (.yaml, "runs-on: macos-latest", "runs-on"),
            (.html, "<section class=\"hero\">text</section>", "section"),
            (.css, ".hero { background-color: transparent; }", "background-color"),
            (.dockerfile, "FROM rust:1.90 AS builder", "FROM"),
        ]

        for (language, line, expected) in samples {
            let tokens = classifiedText(line, language: language)
            XCTAssertTrue(
                tokens.contains(where: { $0.text == expected && $0.cls == .keyword }),
                "Missing \(expected) for \(language): \(tokens.map(\.text))"
            )
        }
    }

    func testNewMultilineConstructsCarryBetweenLines() {
        let samples: [(HighlightLanguage, String)] = [
            (.java, "var text = \"\"\"hello\nworld\"\"\";"),
            (.kotlin, "val text = \"\"\"hello\nworld\"\"\""),
            (.swift, "let text = \"\"\"hello\nworld\"\"\""),
            (.lua, "--[[ hello\nworld ]] local done = true"),
            (.html, "<!-- hello\nworld --> <main>done</main>"),
        ]

        for (language, source) in samples {
            let lines = Highlighter.highlight(code: source, language: language)
            XCTAssertEqual(lines.count, 2)
            XCTAssertTrue(
                lines[1].contains(where: { $0.cls == .stringLit || $0.cls == .comment }),
                "Missing carried token for \(language)"
            )
        }
    }

    private func classifiedText(
        _ line: String,
        language: HighlightLanguage
    ) -> [(text: String, cls: TokenClass)] {
        let characters = Array(line)
        var carry = LineCarry()
        return Highlighter.tokenizeLine(characters, language: language, carry: &carry).map { span in
            (String(characters[span.range]), span.cls)
        }
    }
}

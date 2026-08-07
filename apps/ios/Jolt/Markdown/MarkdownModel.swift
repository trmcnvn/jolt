// Markdown block model — a port of crates/ui/src/markdown/parser.rs.
//
// The transcript renders one row per *top-level block*, so the model is
// block-first: a parsed document is a flat list of `TopBlock`s whose content
// hash doubles as the row-version key for the virtualizer. Inline content is a
// run model (adjacent same-style runs merged) rather than an AST, which keeps
// rendering a single pass over styled spans.

import Foundation
import Markdown

struct InlineStyle: Hashable {
    var bold = false
    var italic = false
    var code = false
    var strikethrough = false
    var math: MDMathKind? = nil
    var link: String? = nil

    static let plain = InlineStyle()
}

enum MDMathKind: Hashable {
    case inline, display
}

struct InlineRun: Hashable {
    var text: String
    var style: InlineStyle
}

enum MDAlign: Hashable {
    case left, center, right, none
}

struct MDListItem: Hashable {
    /// Task-list checkbox state; nil for plain items.
    var checked: Bool?
    var children: [MDBlock]
}

indirect enum MDBlock: Hashable {
    case paragraph([InlineRun])
    case heading(level: Int, [InlineRun])
    case codeBlock(language: String?, code: String)
    case blockquote([MDBlock])
    case list(orderedStart: Int?, items: [MDListItem])
    case table(header: [[InlineRun]], rows: [[[InlineRun]]], align: [MDAlign])
    case rule
}

/// A top-level block plus the 1-based source line it starts on (the stable
/// re-parse anchor) and a content hash used as the row diff key.
struct TopBlock: Hashable {
    var startLine: Int
    var block: MDBlock

    /// FNV-1a-style stable content fingerprint (row version key).
    var fingerprint: UInt64 {
        var hasher = Hasher()
        block.hash(into: &hasher)
        return UInt64(bitPattern: Int64(hasher.finalize()))
    }
}

// MARK: - AST walk (swift-markdown → block model)

enum MarkdownParser {
    private static let inlineOpen: Character = "\u{80}"
    private static let inlineClose: Character = "\u{81}"
    private static let displayOpen: Character = "\u{82}"
    private static let displayClose: Character = "\u{83}"
    private static let literalDollar: Character = "\u{84}"
    private static let nestedBacktick: Character = "\u{16}"
    private static let nestedTilde: Character = "\u{17}"

    /// Parse a complete markdown source into top-level blocks.
    static func parse(_ source: String) -> [TopBlock] {
        let nested = normalizeNestedMarkdownFences(source)
        let quoted = normalizeTripleBlockquotes(nested)
        let document = Document(parsing: normalizeMathSource(quoted))
        return document.children.compactMap { child in
            guard let block = convertBlock(child) else { return nil }
            let line = child.range?.lowerBound.line ?? 1
            return TopBlock(startLine: line, block: block)
        }
    }

    /// Keep copyable Markdown fixtures literal when they contain their own
    /// triple-backtick examples. Equal-length fences are not nestable in
    /// CommonMark, so hide inner closing markers behind one-byte sentinels.
    private static func normalizeNestedMarkdownFences(_ source: String) -> String {
        let lines = source.split(separator: "\n", omittingEmptySubsequences: false)
        var wrapper: (marker: Character, count: Int, depth: Int)?
        return lines.map { raw -> String in
            let line = String(raw)
            guard let fence = parsedFenceLine(line) else { return line }
            if let active = wrapper {
                guard fence.marker == active.marker, fence.count >= active.count else {
                    return line
                }
                if fence.rest.allSatisfy(\.isWhitespace) {
                    if active.depth == 0 {
                        wrapper = nil
                        return line
                    }
                    wrapper = (active.marker, active.count, active.depth - 1)
                    var chars = Array(line)
                    let sentinel = active.marker == "`" ? nestedBacktick : nestedTilde
                    for ix in fence.indent..<(fence.indent + fence.count) {
                        chars[ix] = sentinel
                    }
                    return String(chars)
                }
                wrapper = (active.marker, active.count, active.depth + 1)
                return line
            }

            let language = fence.rest.split(whereSeparator: \.isWhitespace)
                .first.map(String.init)?.lowercased() ?? ""
            if ["markdown", "md", "mdown", "commonmark"].contains(language) {
                wrapper = (fence.marker, fence.count, 0)
            }
            return line
        }.joined(separator: "\n")
    }

    /// Treat the common `>>> quote` shorthand as one quote block instead of
    /// three nested block quotes. Character-for-character replacement keeps
    /// incremental parser line anchors stable.
    private static func normalizeTripleBlockquotes(_ source: String) -> String {
        let lines = source.split(separator: "\n", omittingEmptySubsequences: false)
        var fence: (marker: Character, count: Int)?
        return lines.map { raw -> String in
            let line = String(raw)
            if let active = fence {
                if let candidate = parsedFenceLine(line),
                   candidate.marker == active.marker,
                   candidate.count >= active.count,
                   candidate.rest.allSatisfy(\.isWhitespace) {
                    fence = nil
                }
                return line
            }
            if let opening = parsedFenceLine(line) {
                fence = (opening.marker, opening.count)
                return line
            }

            var chars = Array(line)
            let indent = chars.prefix { $0 == " " }.count
            guard indent <= 3, chars.count >= indent + 3,
                  chars[indent] == ">", chars[indent + 1] == ">", chars[indent + 2] == ">",
                  chars.count == indent + 3 || chars[indent + 3].isWhitespace else {
                return line
            }
            chars[indent + 1] = " "
            chars[indent + 2] = " "
            return String(chars)
        }.joined(separator: "\n")
    }

    private static func parsedFenceLine(_ line: String)
        -> (marker: Character, count: Int, rest: String, indent: Int)? {
        let chars = Array(line)
        let indent = chars.prefix { $0 == " " }.count
        guard indent <= 3, indent < chars.count else { return nil }
        let marker = chars[indent]
        guard marker == "`" || marker == "~" else { return nil }
        let count = chars[indent...].prefix { $0 == marker }.count
        guard count >= 3 else { return nil }
        return (marker, count, String(chars.dropFirst(indent + count)), indent)
    }

    private static func convertBlock(_ markup: Markup) -> MDBlock? {
        switch markup {
        case let paragraph as Paragraph:
            return .paragraph(inlines(of: paragraph))
        case let heading as Heading:
            return .heading(level: heading.level, inlines(of: heading))
        case let code as CodeBlock:
            var body = restoreMathSentinels(code.code)
            if body.hasSuffix("\n") { body.removeLast() }
            let lang = code.language.flatMap { $0.isEmpty ? nil : $0 }
            return .codeBlock(language: lang, code: body)
        case let quote as BlockQuote:
            return .blockquote(quote.children.compactMap(convertBlock))
        case let list as UnorderedList:
            return .list(orderedStart: nil, items: listItems(of: list))
        case let list as OrderedList:
            return .list(orderedStart: Int(list.startIndex), items: listItems(of: list))
        case let table as Markdown.Table:
            // `.cells`/`.rows` are lazy sequences — materialize eagerly.
            let header: [[InlineRun]] = table.head.cells.map { inlines(of: $0) }
            let rows: [[[InlineRun]]] = table.body.rows.map { row in
                row.cells.map { inlines(of: $0) } as [[InlineRun]]
            }
            let align: [MDAlign] = table.columnAlignments.map {
                switch $0 {
                case .left: return .left
                case .center: return .center
                case .right: return .right
                case nil: return .none
                }
            }
            return .table(header: header, rows: rows, align: align)
        case is ThematicBreak:
            return .rule
        case let html as HTMLBlock:
            // No HTML rendering — surface the raw source as a code block,
            // matching the desktop's plain-text fallback behavior.
            return .codeBlock(language: "html", code: restoreMathSentinels(html.rawHTML)
                .trimmingCharacters(in: .newlines))
        default:
            // Unknown/unsupported block: flatten to a paragraph of its text.
            let text = restoreMathSentinels(markup.format())
            guard !text.isEmpty else { return nil }
            return .paragraph([InlineRun(text: text, style: .plain)])
        }
    }

    private static func listItems(of list: Markup) -> [MDListItem] {
        list.children.compactMap { child in
            guard let item = child as? ListItem else { return nil }
            let checked: Bool? = item.checkbox.map { $0 == .checked }
            return MDListItem(checked: checked, children: item.children.compactMap(convertBlock))
        }
    }

    private static func inlines(of container: Markup) -> [InlineRun] {
        var runs: [InlineRun] = []
        for child in container.children {
            collectInline(child, style: .plain, into: &runs)
        }
        return mergeRuns(runs)
    }

    private static func collectInline(_ markup: Markup, style: InlineStyle, into runs: inout [InlineRun]) {
        switch markup {
        case let text as Markdown.Text:
            appendMathRuns(text.string, style: style, into: &runs)
        case let code as InlineCode:
            var s = style; s.code = true
            runs.append(InlineRun(text: restoreMathSentinels(code.code), style: s))
        case let strong as Strong:
            var s = style; s.bold = true
            strong.children.forEach { collectInline($0, style: s, into: &runs) }
        case let em as Emphasis:
            var s = style; s.italic = true
            em.children.forEach { collectInline($0, style: s, into: &runs) }
        case let strike as Strikethrough:
            var s = style; s.strikethrough = true
            strike.children.forEach { collectInline($0, style: s, into: &runs) }
        case let link as Markdown.Link:
            var s = style; s.link = link.destination
            link.children.forEach { collectInline($0, style: s, into: &runs) }
        case let image as Markdown.Image:
            // Images render as their alt text (desktop parity: no inline images).
            let alt = image.children.compactMap { ($0 as? Markdown.Text)?.string }.joined()
            runs.append(InlineRun(text: alt.isEmpty ? (image.source ?? "") : alt, style: style))
        case is SoftBreak:
            runs.append(InlineRun(text: " ", style: style))
        case is LineBreak:
            runs.append(InlineRun(text: "\n", style: style))
        case let html as InlineHTML:
            runs.append(InlineRun(text: restoreMathSentinels(html.rawHTML), style: style))
        default:
            for child in markup.children {
                collectInline(child, style: style, into: &runs)
            }
        }
    }

    /// Merge adjacent runs with identical style so rendering sees minimal spans.
    static func mergeRuns(_ runs: [InlineRun]) -> [InlineRun] {
        var merged: [InlineRun] = []
        for run in runs {
            if run.text.isEmpty { continue }
            if var last = merged.last, last.style == run.style, run.style.math == nil {
                last.text += run.text
                merged[merged.count - 1] = last
            } else {
                merged.append(run)
            }
        }
        return linkifyBareWebURLs(merged)
    }

    /// Split bare HTTP(S) URLs out of ordinary prose runs while preserving
    /// every existing style flag. Explicit Markdown links and code/math remain
    /// unchanged.
    private static func linkifyBareWebURLs(_ runs: [InlineRun]) -> [InlineRun] {
        var linked: [InlineRun] = []
        for run in runs {
            guard run.style.link == nil, !run.style.code, run.style.math == nil else {
                linked.append(run)
                continue
            }

            let text = run.text
            var plainStart = text.startIndex
            var searchFrom = text.startIndex
            var found = false
            while let start = nextWebURLStart(in: text, from: searchFrom) {
                searchFrom = text.index(after: start)
                if start > text.startIndex {
                    let previous = text[text.index(before: start)]
                    if previous.isLetter || previous.isNumber || previous == "_" { continue }
                }

                var scannedEnd = start
                while scannedEnd < text.endIndex {
                    let character = text[scannedEnd]
                    if character.isWhitespace || bareURLDelimiters.contains(character) { break }
                    scannedEnd = text.index(after: scannedEnd)
                }
                let candidate = String(text[start..<scannedEnd])
                let destination = trimmedBareURL(candidate)
                guard isWebURL(destination) else { continue }
                let end = text.index(start, offsetBy: destination.count)

                if plainStart < start {
                    linked.append(InlineRun(text: String(text[plainStart..<start]), style: run.style))
                }
                var linkStyle = run.style
                linkStyle.link = destination
                linked.append(InlineRun(text: destination, style: linkStyle))
                plainStart = end
                searchFrom = end
                found = true
            }

            if found {
                if plainStart < text.endIndex {
                    linked.append(InlineRun(text: String(text[plainStart...]), style: run.style))
                }
            } else {
                linked.append(run)
            }
        }
        return linked
    }

    private static let bareURLDelimiters: Set<Character> = [
        "<", ">", "\"", "'", "`", "\\", "“", "”", "‘", "’",
    ]

    private static func nextWebURLStart(in text: String,
                                        from: String.Index) -> String.Index? {
        let range = from..<text.endIndex
        return ["http://", "https://"]
            .compactMap { text.range(of: $0, range: range)?.lowerBound }
            .min()
    }

    private static func trimmedBareURL(_ candidate: String) -> String {
        var result = candidate
        while !result.isEmpty {
            while let last = result.last, ".,;:!?…".contains(last) {
                result.removeLast()
            }
            guard let close = result.last else { break }
            let open: Character
            switch close {
            case ")": open = "("
            case "]": open = "["
            case "}": open = "{"
            default: return result
            }
            guard result.filter({ $0 == close }).count > result.filter({ $0 == open }).count else {
                return result
            }
            result.removeLast()
        }
        return result
    }

    private static func isWebURL(_ candidate: String) -> Bool {
        guard let components = URLComponents(string: candidate),
              let scheme = components.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = components.host, !host.isEmpty else {
            return false
        }
        return true
    }

    /// Convert standalone display delimiters to `math` fences (same line
    /// count), then protect formula bodies from CommonMark inline parsing.
    private static func normalizeMathSource(_ source: String) -> String {
        let lines = source.split(separator: "\n", omittingEmptySubsequences: false)
        var markdownFence: (marker: Character, count: Int)?
        var closeMath: String?
        let fenced = lines.enumerated().map { lineIndex, raw -> String in
            let line = String(raw)
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if let delimiter = closeMath {
                if trimmed == delimiter {
                    closeMath = nil
                    return "```"
                }
                return line
            }
            if let active = markdownFence {
                if closesMarkdownFence(trimmed, active) { markdownFence = nil }
                return line
            }
            if let opening = markdownFenceMarker(trimmed) {
                markdownFence = opening
                return line
            }
            if isIndentedCodeLine(line) { return line }
            if trimmed == "$$" || trimmed == #"\["# {
                let candidate = trimmed == "$$" ? "$$" : #"\]"#
                if hasStandaloneClose(lines, after: lineIndex, delimiter: candidate) {
                    closeMath = candidate
                    return "```math"
                }
            }
            return line
        }.joined(separator: "\n")

        var protectedFence: (marker: Character, count: Int)?
        let protected = fenced.split(separator: "\n", omittingEmptySubsequences: false)
            .map { raw -> String in
                let line = String(raw)
                let trimmed = line.trimmingCharacters(in: .whitespaces)
                if let active = protectedFence {
                    if closesMarkdownFence(trimmed, active) { protectedFence = nil }
                    return line
                }
                if let opening = markdownFenceMarker(trimmed) {
                    protectedFence = opening
                    return line
                }
                if isIndentedCodeLine(line) { return line }
                return protectFormulas(in: line)
            }
            .joined(separator: "\n")

        // Preserve unmatched TeX delimiters and escaped dollars literally.
        // Valid formulas were encoded above, so CommonMark cannot reinterpret
        // operators such as `*`, `_`, or `[` inside the formula body.
        return protectLiteralDelimiters(in: protected)
    }

    private static func markdownFenceMarker(_ trimmed: String)
        -> (marker: Character, count: Int)? {
        guard let marker = trimmed.first, marker == "`" || marker == "~" else { return nil }
        let count = trimmed.prefix { $0 == marker }.count
        return count >= 3 ? (marker, count) : nil
    }

    private static func isIndentedCodeLine(_ line: String) -> Bool {
        line.hasPrefix("\t") || line.hasPrefix("    ")
    }

    private static func closesMarkdownFence(
        _ trimmed: String,
        _ active: (marker: Character, count: Int)
    ) -> Bool {
        let markerCount = trimmed.prefix { $0 == active.marker }.count
        guard markerCount >= active.count else { return false }
        return trimmed.dropFirst(markerCount).allSatisfy(\.isWhitespace)
    }

    private static func hasStandaloneClose(
        _ lines: [Substring],
        after lineIndex: Int,
        delimiter: String
    ) -> Bool {
        for raw in lines.dropFirst(lineIndex + 1) {
            let trimmed = String(raw).trimmingCharacters(in: .whitespaces)
            if trimmed == delimiter { return true }
            if markdownFenceMarker(trimmed) != nil { return false }
        }
        return false
    }

    private static func protectFormulas(in line: String) -> String {
        let chars = Array(line)
        let codeProtected = inlineCodeMask(chars)
        var out = ""
        var ix = 0
        while ix < chars.count {
            if codeProtected[ix] {
                out.append(chars[ix])
                ix += 1
                continue
            }

            let kind: MDMathKind
            let openCount: Int
            let close: [Character]
            let requiresTightBoundary: Bool
            if chars[ix] == "\\", !isEscaped(chars, at: ix), ix + 1 < chars.count,
               chars[ix + 1] == "(" || chars[ix + 1] == "[" {
                kind = chars[ix + 1] == "(" ? .inline : .display
                openCount = 2
                close = ["\\", chars[ix + 1] == "(" ? ")" : "]"]
                requiresTightBoundary = false
            } else if chars[ix] == "$", !isEscaped(chars, at: ix) {
                let display = ix + 1 < chars.count && chars[ix + 1] == "$"
                kind = display ? .display : .inline
                openCount = display ? 2 : 1
                close = display ? ["$", "$"] : ["$"]
                requiresTightBoundary = !display
            } else {
                out.append(chars[ix])
                ix += 1
                continue
            }

            let bodyStart = ix + openCount
            guard bodyStart < chars.count,
                  let end = findFormulaClose(
                    close, in: chars, from: bodyStart, excluding: codeProtected
                  ),
                  end > bodyStart,
                  (!requiresTightBoundary || (!chars[bodyStart].isWhitespace
                    && !chars[end - 1].isWhitespace)) else {
                out.append(contentsOf: chars[ix..<(ix + openCount)])
                ix += openCount
                continue
            }
            out.append(kind == .inline ? inlineOpen : displayOpen)
            out += encodeMathPayload(String(chars[bodyStart..<end]))
            out.append(kind == .inline ? inlineClose : displayClose)
            ix = end + close.count
        }
        return out
    }

    private static func inlineCodeMask(_ chars: [Character]) -> [Bool] {
        var protected = Array(repeating: false, count: chars.count)
        var ix = 0
        while ix < chars.count {
            guard chars[ix] == "`" else { ix += 1; continue }
            let count = chars[ix...].prefix { $0 == "`" }.count
            guard let end = findBacktickClose(count: count, in: chars, from: ix + count) else {
                ix += count
                continue
            }
            for marked in ix..<(end + count) { protected[marked] = true }
            ix = end + count
        }
        return protected
    }

    private static func findBacktickClose(
        count: Int,
        in chars: [Character],
        from start: Int
    ) -> Int? {
        var ix = start
        while ix < chars.count {
            if chars[ix] == "`" {
                let candidate = chars[ix...].prefix { $0 == "`" }.count
                if candidate == count { return ix }
                ix += candidate
            } else {
                ix += 1
            }
        }
        return nil
    }

    private static func findFormulaClose(
        _ close: [Character],
        in chars: [Character],
        from start: Int,
        excluding protected: [Bool]
    ) -> Int? {
        var ix = start
        while ix + close.count <= chars.count {
            if Array(chars[ix..<(ix + close.count)]) == close,
               !protected[ix],
               !isEscaped(chars, at: ix) {
                return ix
            }
            ix += 1
        }
        return nil
    }

    private static func isEscaped(_ chars: [Character], at index: Int) -> Bool {
        var slashes = 0
        var at = index
        while at > 0, chars[at - 1] == "\\" {
            slashes += 1
            at -= 1
        }
        return !slashes.isMultiple(of: 2)
    }

    private static func encodeMathPayload(_ source: String) -> String {
        let hex = Array("0123456789abcdef")
        var encoded = "jm"
        encoded.reserveCapacity(2 + source.utf8.count * 2)
        for byte in source.utf8 {
            encoded.append(hex[Int(byte >> 4)])
            encoded.append(hex[Int(byte & 0x0f)])
        }
        return encoded
    }

    private static func decodeMathPayload(_ payload: String) -> String? {
        guard payload.hasPrefix("jm") else { return nil }
        let hex = Array(payload.dropFirst(2))
        guard hex.count.isMultiple(of: 2) else { return nil }
        var bytes: [UInt8] = []
        bytes.reserveCapacity(hex.count / 2)
        var ix = 0
        while ix < hex.count {
            guard let high = hexValue(hex[ix]), let low = hexValue(hex[ix + 1]) else { return nil }
            bytes.append(high << 4 | low)
            ix += 2
        }
        return String(bytes: bytes, encoding: .utf8)
    }

    private static func hexValue(_ character: Character) -> UInt8? {
        switch character {
        case "0"..."9": UInt8(character.asciiValue! - Character("0").asciiValue!)
        case "a"..."f": UInt8(character.asciiValue! - Character("a").asciiValue! + 10)
        default: nil
        }
    }

    private static func protectLiteralDelimiters(in source: String) -> String {
        let chars = Array(source)
        var out = ""
        out.reserveCapacity(source.count)
        var ix = 0
        while ix < chars.count {
            guard chars[ix] == "\\", ix + 1 < chars.count else {
                out.append(chars[ix]); ix += 1; continue
            }
            var preceding = 0
            var at = ix
            while at > 0, chars[at - 1] == "\\" { preceding += 1; at -= 1 }
            guard preceding.isMultiple(of: 2) else {
                out.append(chars[ix]); ix += 1; continue
            }
            let replacement: Character? = switch chars[ix + 1] {
            case "(": inlineOpen
            case ")": inlineClose
            case "[": displayOpen
            case "]": displayClose
            case "$": literalDollar
            default: nil
            }
            if let replacement {
                out.append(replacement)
                ix += 2
            } else {
                out.append(chars[ix])
                ix += 1
            }
        }
        return out
    }

    private static func restoreMathSentinels(_ text: String) -> String {
        text.replacingOccurrences(of: String(inlineOpen), with: #"\("#)
            .replacingOccurrences(of: String(inlineClose), with: #"\)"#)
            .replacingOccurrences(of: String(displayOpen), with: #"\["#)
            .replacingOccurrences(of: String(displayClose), with: #"\]"#)
            .replacingOccurrences(of: String(literalDollar), with: "$")
            .replacingOccurrences(of: String(nestedBacktick), with: "`")
            .replacingOccurrences(of: String(nestedTilde), with: "~")
    }

    private static func appendMathRuns(_ text: String,
                                       style: InlineStyle,
                                       into runs: inout [InlineRun]) {
        let chars = Array(text)
        var plain = ""
        var ix = 0

        func flushPlain() {
            guard !plain.isEmpty else { return }
            runs.append(InlineRun(text: restoreMathSentinels(plain), style: style))
            plain = ""
        }

        while ix < chars.count {
            let kind: MDMathKind?
            let close: [Character]
            let openCount: Int
            if chars[ix] == displayOpen {
                kind = .display; close = [displayClose]; openCount = 1
            } else if chars[ix] == inlineOpen {
                kind = .inline; close = [inlineClose]; openCount = 1
            } else if chars[ix] == "$", ix + 1 < chars.count, chars[ix + 1] == "$" {
                kind = .display; close = ["$", "$"]; openCount = 2
            } else if chars[ix] == "$" {
                kind = .inline; close = ["$"]; openCount = 1
            } else {
                plain.append(chars[ix]); ix += 1; continue
            }

            let bodyStart = ix + openCount
            guard bodyStart < chars.count,
                  !chars[bodyStart].isWhitespace,
                  let end = findClosing(close, in: chars, from: bodyStart),
                  end > bodyStart,
                  !chars[end - 1].isWhitespace else {
                plain.append(contentsOf: chars[ix..<(ix + openCount)])
                ix += openCount
                continue
            }
            flushPlain()
            var mathStyle = style
            mathStyle.math = kind
            let payload = String(chars[bodyStart..<end])
            let body = decodeMathPayload(payload) ?? restoreMathSentinels(payload)
            runs.append(InlineRun(text: body, style: mathStyle))
            ix = end + close.count
        }
        flushPlain()
    }

    private static func findClosing(_ close: [Character],
                                    in chars: [Character],
                                    from start: Int) -> Int? {
        guard !close.isEmpty else { return nil }
        var ix = start
        while ix + close.count <= chars.count {
            if Array(chars[ix..<(ix + close.count)]) == close { return ix }
            ix += 1
        }
        return nil
    }
}

// MARK: - Incremental streaming parser (parser.rs IncrementalParser port)

/// Re-parses only the streaming tail: on append, parsing restarts from the
/// start of the *second-to-last* top-level block (covers continuation merges),
/// so per-append cost is O(delta + tail), never O(document). Link-reference
/// definitions (`[label]: url`) break the locality assumption and force full
/// re-parses.
final class IncrementalMarkdownParser {
    private(set) var source: String = ""
    private(set) var blocks: [TopBlock] = []
    private var fullOnly = false

    func setText(_ text: String) {
        if text == source { return }
        if !fullOnly, text.hasPrefix(source), !source.isEmpty {
            append(text)
        } else {
            reset(text)
        }
    }

    private func reset(_ text: String) {
        source = text
        fullOnly = Self.hasLinkDefs(text)
        blocks = MarkdownParser.parse(text)
    }

    private func append(_ text: String) {
        let delta = String(text.dropFirst(source.count))
        source = text
        if Self.hasLinkDefs(delta) {
            fullOnly = true
            blocks = MarkdownParser.parse(text)
            return
        }
        guard blocks.count >= 2 else {
            blocks = MarkdownParser.parse(text)
            return
        }
        // Stable boundary: the start line of the second-to-last block.
        let boundaryLine = blocks[blocks.count - 2].startLine
        let stable = Array(blocks.prefix(blocks.count - 2))
        let tailSource = Self.suffix(of: text, fromLine: boundaryLine)
        let tailBlocks = MarkdownParser.parse(tailSource).map { top in
            TopBlock(startLine: top.startLine + boundaryLine - 1, block: top.block)
        }
        blocks = stable + tailBlocks
    }

    /// The substring starting at the given 1-based line.
    private static func suffix(of text: String, fromLine line: Int) -> String {
        guard line > 1 else { return text }
        var remaining = line - 1
        var index = text.startIndex
        while remaining > 0, let nl = text[index...].firstIndex(of: "\n") {
            index = text.index(after: nl)
            remaining -= 1
        }
        return String(text[index...])
    }

    private static let linkDefPattern = /(?m)^\s{0,3}\[[^\]]+\]:/
    static func hasLinkDefs(_ text: String) -> Bool {
        text.contains(linkDefPattern)
    }
}

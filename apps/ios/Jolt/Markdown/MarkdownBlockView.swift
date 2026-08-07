// Markdown block rendering — metrics ported from crates/ui/src/markdown/render.rs.
//
// Every constant here mirrors the desktop values so the two apps read the same:
// body 14/22, headings (19/27, 16/24, 15/22, 14/22), code 12.5/18, block gap 12.
// Code blocks render one fixed-height row per line, so their height is analytic
// (lines × 18 + padding) and syntax highlighting is a pure recolor.

import SwiftUI
import RaTeX
import BeautifulMermaid

enum MD {
    static let textSize: CGFloat = 14
    static let lineHeight: CGFloat = 22
    static let blockGap: CGFloat = 12
    static let codeTextSize: CGFloat = 12.5
    static let codeLineHeight: CGFloat = 18
    static let codePaddingX: CGFloat = 12
    static let codePaddingY: CGFloat = 10

    static func headingMetrics(_ level: Int) -> (size: CGFloat, line: CGFloat) {
        switch level {
        case 1: return (19, 27)
        case 2: return (16, 24)
        case 3: return (15, 22)
        default: return (14, 22)
        }
    }
}

// MARK: - Inline runs → AttributedString

extension [InlineRun] {
    /// Styled text for a run list. `baseColor`/`size` let blockquotes and
    /// headings restyle without touching the run model.
    func attributed(size: CGFloat = MD.textSize,
                    weight: Font.Weight = .regular,
                    baseColor: Color = Theme.text) -> AttributedString {
        var result = AttributedString()
        for run in self {
            let visible = switch run.style.math {
            case .inline: "$\(run.text)$"
            case .display: "$$\(run.text)$$"
            case nil: run.text
            }
            var piece = AttributedString(visible)
            if run.style.code {
                piece.font = Theme.mono(size - 1.5)
                piece.foregroundColor = Theme.inlineCodeText
                piece.backgroundColor = Theme.inlineCodeWash
            } else {
                var w = weight
                if run.style.bold { w = .semibold }
                var font = Theme.sans(size, weight: w)
                if run.style.italic { font = font.italic() }
                piece.font = font
                piece.foregroundColor = baseColor
            }
            if run.style.strikethrough {
                piece.strikethroughStyle = Text.LineStyle(pattern: .solid, color: Theme.textMuted)
            }
            if let link = run.style.link {
                // Monochrome links: primary text + muted hairline underline,
                // never accent (desktop render.rs:536).
                piece.link = URL(string: link)
                piece.foregroundColor = baseColor
                piece.underlineStyle = Text.LineStyle(pattern: .solid, color: Theme.textMuted)
            }
            result += piece
        }
        return result
    }
}

extension [InlineRun] {
    /// Concatenated `Text` with inline-code runs tagged for the rounded-wash
    /// renderer (custom TextAttributes only attach via `Text.customAttribute`,
    /// not AttributedString). Settled rows render through this; live veiled
    /// rows use `attributed()` (square wash only while fading).
    func styled(size: CGFloat = MD.textSize,
                weight: Font.Weight = .regular,
                baseColor: Color = Theme.text) -> Text {
        var result = Text(verbatim: "")
        for run in self {
            if run.style.code {
                var piece = AttributedString(run.text)
                piece.font = Theme.mono(size - 1.5)
                piece.foregroundColor = Theme.inlineCodeText
                result = result + Text(piece).customAttribute(InlineCodeAttribute())
            } else {
                result = result + Text([run].attributed(size: size, weight: weight,
                                                        baseColor: baseColor))
            }
        }
        return result
    }
}

extension [InlineRun] {
    /// The veiled variant of `styled()`: runs are further split at veil-chunk
    /// boundaries so fading text keeps the renderer's rounded code washes.
    func styledVeiled(size: CGFloat = MD.textSize,
                      weight: Font.Weight = .regular,
                      baseColor: Color = Theme.text,
                      veil: RowVeil) -> Text {
        let total = reduce(0) { $0 + $1.text.count }
        let segments = veil.segments(totalLength: total)
        var result = Text(verbatim: "")
        var offset = 0
        for run in self {
            let chars = [Character](run.text)
            let runRange = offset..<(offset + chars.count)
            for segment in segments {
                let lower = Swift.max(segment.range.lowerBound, runRange.lowerBound)
                let upper = Swift.min(segment.range.upperBound, runRange.upperBound)
                guard lower < upper else { continue }
                let slice = String(chars[(lower - offset)..<(upper - offset)])
                if run.style.code {
                    var piece = AttributedString(slice)
                    piece.font = Theme.mono(size - 1.5)
                    piece.foregroundColor = Theme.inlineCodeText.opacity(segment.alpha)
                    result = result + Text(piece).customAttribute(InlineCodeAttribute())
                } else {
                    var sliced = run
                    sliced.text = slice
                    var attr = [sliced].attributed(size: size, weight: weight, baseColor: baseColor)
                    if segment.alpha < 1 {
                        for r in attr.runs {
                            let base: Color = attr[r.range].foregroundColor ?? baseColor
                            attr[r.range].foregroundColor = base.opacity(segment.alpha)
                        }
                    }
                    result = result + Text(attr)
                }
            }
            offset += chars.count
        }
        return result
    }
}

// MARK: - Block view

struct MarkdownBlockView: View {
    let block: MDBlock
    /// Identity for async highlight caching (row id).
    var cacheKey: String = ""
    /// Assistant rows fill the transcript column; user-bubble blocks keep
    /// their intrinsic width up to the bubble's cap.
    var fillsWidth = true
    /// Live tail rows keep rich fenced blocks as source until their closing
    /// fence and the response itself are both settled.
    var streaming = false

    var body: some View {
        switch block {
        case .paragraph(let runs):
            if runs.contains(where: { $0.style.math != nil }) {
                MathRunsView(runs: runs, size: MD.textSize, lineHeight: MD.lineHeight)
                    .frame(maxWidth: fillsWidth ? .infinity : nil, alignment: .leading)
            } else {
                runs.styled()
                    .textRenderer(InlineCodeRenderer())
                    .lineSpacing(MD.lineHeight - MD.textSize - 4)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: fillsWidth ? .infinity : nil, alignment: .leading)
                    .tint(Theme.text)
            }

        case .heading(let level, let runs):
            let m = MD.headingMetrics(level)
            if runs.contains(where: { $0.style.math != nil }) {
                MathRunsView(runs: runs, size: m.size, lineHeight: m.line, weight: .semibold)
                    .frame(maxWidth: fillsWidth ? .infinity : nil, alignment: .leading)
            } else {
                runs.styled(size: m.size, weight: .semibold)
                    .textRenderer(InlineCodeRenderer())
                    .lineSpacing(m.line - m.size - 4)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: fillsWidth ? .infinity : nil, alignment: .leading)
            }

        case .codeBlock(let language, let code):
            if !streaming, ["math", "latex", "tex"].contains(language?.lowercased() ?? "") {
                MathBlockView(source: code, cacheKey: cacheKey)
            } else if !streaming, language?.lowercased() == "mermaid" {
                MermaidBlockView(source: code, cacheKey: cacheKey)
            } else {
                CodeBlockView(language: language, code: code, cacheKey: cacheKey)
            }

        case .blockquote(let children):
            BlockquoteView(children: children, cacheKey: cacheKey, streaming: streaming)

        case .list(let orderedStart, let items):
            ListBlockView(orderedStart: orderedStart, items: items,
                          cacheKey: cacheKey, streaming: streaming)

        case .table(let header, let rows, let align):
            TableBlockView(header: header, rows: rows, align: align)

        case .rule:
            Rectangle()
                .fill(Theme.border)
                .frame(height: 1)
        }
    }
}

// MARK: - Math

private enum MathRunChunk {
    case inline([InlineRun])
    case display(InlineRun)
}

private struct MathRunsView: View {
    let runs: [InlineRun]
    let size: CGFloat
    let lineHeight: CGFloat
    var weight: Font.Weight = .regular

    private var chunks: [MathRunChunk] {
        var result: [MathRunChunk] = []
        var inline: [InlineRun] = []
        func flush() {
            guard !inline.isEmpty else { return }
            result.append(.inline(inline))
            inline = []
        }
        for run in runs {
            if run.style.math == .display {
                flush()
                result.append(.display(run))
            } else {
                inline.append(run)
            }
        }
        flush()
        return result
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(Array(chunks.enumerated()), id: \.offset) { _, chunk in
                switch chunk {
                case .inline(let runs):
                    MathFlowLayout(horizontalSpacing: 3, lineSpacing: 4) {
                        ForEach(Array(fragments(runs).enumerated()), id: \.offset) { _, run in
                            if run.style.math == .inline {
                                FormulaOrSource(run: run, size: size, displayMode: false)
                            } else {
                                [run].styled(size: size, weight: weight)
                                    .textRenderer(InlineCodeRenderer())
                                    .lineLimit(1)
                            }
                        }
                    }
                    .frame(maxWidth: .infinity, minHeight: lineHeight, alignment: .leading)

                case .display(let run):
                    ScrollView(.horizontal, showsIndicators: false) {
                        FormulaOrSource(run: run, size: size + 2, displayMode: true)
                            .padding(.horizontal, 4)
                    }
                    .frame(maxWidth: .infinity, alignment: .center)
                }
            }
        }
        .tint(Theme.text)
    }

    private func fragments(_ runs: [InlineRun]) -> [InlineRun] {
        runs.flatMap { run -> [InlineRun] in
            guard run.style.math == nil else { return [run] }
            return run.text.split(whereSeparator: \.isWhitespace).map { word in
                var fragment = run
                fragment.text = String(word)
                return fragment
            }
        }
    }
}

private struct FormulaOrSource: View {
    let run: InlineRun
    let size: CGFloat
    let displayMode: Bool

    private var isValid: Bool {
        (try? RaTeXEngine.shared.parse(
            run.text,
            displayMode: displayMode,
            color: UIColor(Theme.text)
        )) != nil
    }

    var body: some View {
        if isValid {
            RaTeXFormula(
                latex: run.text,
                fontSize: size,
                displayMode: displayMode,
                color: Theme.text
            )
            .contextMenu {
                Button {
                    UIPasteboard.general.string = run.text
                } label: {
                    Label("Copy formula", systemImage: "doc.on.doc")
                }
            }
        } else {
            Text(displayMode ? "$$\(run.text)$$" : "$\(run.text)$")
                .font(Theme.mono(size - 1))
                .foregroundStyle(Theme.textMuted)
        }
    }
}

/// Baseline-aware wrapping layout, using the ascent value published by
/// `RaTeXFormula` for formula children and SwiftUI's native text baseline for
/// prose children.
private struct MathFlowLayout: Layout {
    var horizontalSpacing: CGFloat = 3
    var lineSpacing: CGFloat = 4

    typealias Cache = [[(index: Int, size: CGSize)]]
    func makeCache(subviews: Subviews) -> Cache { [] }

    func sizeThatFits(proposal: ProposedViewSize,
                      subviews: Subviews,
                      cache: inout Cache) -> CGSize {
        let maxWidth = proposal.width ?? .infinity
        cache = lines(for: subviews, maxWidth: maxWidth)
        let height = cache.enumerated().reduce(CGFloat.zero) { total, item in
            total + (item.element.map(\.size.height).max() ?? 0)
                + (item.offset + 1 < cache.count ? lineSpacing : 0)
        }
        return CGSize(width: maxWidth.isFinite ? maxWidth : cacheWidth(cache), height: height)
    }

    func placeSubviews(in bounds: CGRect,
                       proposal: ProposedViewSize,
                       subviews: Subviews,
                       cache: inout Cache) {
        if cache.isEmpty { cache = lines(for: subviews, maxWidth: bounds.width) }
        var y = bounds.minY
        for line in cache {
            let baselines = line.map { item -> CGFloat in
                let formula = subviews[item.index][RaTeXFormulaAscentKey.self]
                if formula > 0 { return formula }
                let dimensions = subviews[item.index]
                    .dimensions(in: ProposedViewSize(item.size))
                let native = dimensions[.firstTextBaseline]
                return native > 0 ? native : item.size.height / 2
            }
            let baseline = baselines.max() ?? 0
            let height = line.map(\.size.height).max() ?? 0
            var x = bounds.minX
            for (position, item) in line.enumerated() {
                subviews[item.index].place(
                    at: CGPoint(x: x, y: y + baseline - baselines[position]),
                    proposal: ProposedViewSize(item.size)
                )
                x += item.size.width + horizontalSpacing
            }
            y += height + lineSpacing
        }
    }

    private func lines(for subviews: Subviews, maxWidth: CGFloat) -> Cache {
        var result: Cache = []
        var line: [(index: Int, size: CGSize)] = []
        var width: CGFloat = 0
        for (index, subview) in subviews.enumerated() {
            let size = subview.sizeThatFits(.unspecified)
            let needed = line.isEmpty ? size.width : size.width + horizontalSpacing
            if width + needed > maxWidth, !line.isEmpty {
                result.append(line)
                line = [(index, size)]
                width = size.width
            } else {
                line.append((index, size))
                width += needed
            }
        }
        if !line.isEmpty { result.append(line) }
        return result
    }

    private func cacheWidth(_ cache: Cache) -> CGFloat {
        cache.map { line in
            line.map(\.size.width).reduce(0, +)
                + CGFloat(max(0, line.count - 1)) * horizontalSpacing
        }.max() ?? 0
    }
}

private struct MathBlockView: View {
    let source: String
    var cacheKey = ""

    var body: some View {
        Group {
            if (try? RaTeXEngine.shared.parse(
                source,
                displayMode: true,
                color: UIColor(Theme.text)
            )) != nil {
                ScrollView(.horizontal, showsIndicators: false) {
                    RaTeXFormula(latex: source, fontSize: MD.textSize + 2,
                                 displayMode: true, color: Theme.text)
                        .padding(12)
                }
            } else {
                CodeBlockView(language: "math", code: source, cacheKey: cacheKey)
            }
        }
        .frame(maxWidth: .infinity, alignment: .center)
        .background(whiteAlpha(0.02))
        .clipShape(RoundedRectangle(cornerRadius: Theme.panelRadius))
        .overlay(RoundedRectangle(cornerRadius: Theme.panelRadius)
            .strokeBorder(Theme.border, lineWidth: 1))
        .contextMenu {
            Button {
                UIPasteboard.general.string = source
            } label: {
                Label("Copy formula", systemImage: "doc.on.doc")
            }
        }
    }
}

private struct MermaidBlockView: View {
    let source: String
    var cacheKey = ""
    @State private var parseError: Error?
    @State private var bounds = CGRect.zero

    private var diagramTheme: DiagramTheme {
        DiagramTheme(
            background: UIColor(Theme.bg),
            foreground: UIColor(Theme.text),
            line: UIColor(Theme.textMuted),
            accent: UIColor(Theme.accent),
            muted: UIColor(Theme.textMuted),
            surface: UIColor(Theme.surface),
            border: UIColor(Theme.borderStrong),
            font: UIFont(name: Theme.fontSansName, size: MD.textSize)
                ?? .systemFont(ofSize: MD.textSize),
            cornerRadius: 8
        )
    }

    var body: some View {
        Group {
            if parseError != nil {
                CodeBlockView(language: "mermaid", code: source, cacheKey: cacheKey)
            } else {
                let width = max(bounds.width, 300)
                let naturalHeight = max(bounds.height, 180)
                ScrollView([.horizontal, .vertical], showsIndicators: false) {
                    MermaidDiagramView(
                        source: source,
                        theme: diagramTheme,
                        parseError: $parseError,
                        diagramBounds: $bounds
                    )
                    .frame(width: width, height: naturalHeight)
                }
                .frame(maxWidth: .infinity)
                .frame(height: min(naturalHeight, 560))
                .padding(10)
            }
        }
        .background(Theme.bg)
        .clipShape(RoundedRectangle(cornerRadius: Theme.panelRadius))
        .overlay(RoundedRectangle(cornerRadius: Theme.panelRadius)
            .strokeBorder(Theme.border, lineWidth: 1))
        .contextMenu {
            Button {
                UIPasteboard.general.string = source
            } label: {
                Label("Copy Mermaid source", systemImage: "doc.on.doc")
            }
        }
    }
}

// MARK: - Code block

struct CodeBlockView: View {
    let language: String?
    let code: String
    var cacheKey: String = ""

    @State private var spans: [[TokenSpan]] = []

    private var lines: [String] { code.components(separatedBy: "\n") }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let language, !language.isEmpty {
                Text(language)
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 5)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(whiteAlpha(0.02))
                    .overlay(alignment: .bottom) {
                        Rectangle().fill(Theme.border).frame(height: 1)
                    }
            }
            ScrollView(.horizontal, showsIndicators: false) {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(lines.enumerated()), id: \.offset) { ix, line in
                        Text(attributedLine(line, spans: ix < spans.count ? spans[ix] : []))
                            .font(Theme.mono(MD.codeTextSize))
                            .lineLimit(1)
                            .frame(height: MD.codeLineHeight, alignment: .leading)
                    }
                }
                .padding(.horizontal, MD.codePaddingX)
                .padding(.vertical, MD.codePaddingY)
            }
        }
        .background(whiteAlpha(0.035))
        .clipShape(RoundedRectangle(cornerRadius: Theme.panelRadius))
        .overlay(
            RoundedRectangle(cornerRadius: Theme.panelRadius)
                .strokeBorder(whiteAlpha(0.06), lineWidth: 1)
        )
        .contextMenu {
            Button {
                UIPasteboard.general.string = code
            } label: {
                Label("Copy code", systemImage: "doc.on.doc")
            }
        }
        .task(id: code) {
            guard let lang = HighlightLanguage.forTag(language) else { return }
            let source = code
            let result = await Task.detached(priority: .utility) {
                Highlighter.highlight(code: source, language: lang)
            }.value
            spans = result
        }
    }

    /// Recolor a line by its token spans — same string, same font, paint only.
    private func attributedLine(_ line: String, spans: [TokenSpan]) -> AttributedString {
        var attr = AttributedString(line)
        attr.foregroundColor = Theme.text.opacity(0.9)
        guard !spans.isEmpty else { return attr }
        let chars = Array(line)
        for span in spans {
            guard span.range.lowerBound < chars.count else { continue }
            let upper = min(span.range.upperBound, chars.count)
            let prefix = String(chars[0..<span.range.lowerBound])
            let body = String(chars[span.range.lowerBound..<upper])
            guard let start = attr.index(afterCharacters: prefix.count),
                  let end = attr.index(afterCharacters: prefix.count + body.count) else { continue }
            attr[start..<end].foregroundColor = color(for: span.cls)
        }
        return attr
    }

    private func color(for cls: TokenClass) -> Color {
        switch cls {
        case .keyword: return Theme.tokenKeyword
        case .stringLit: return Theme.tokenString
        case .number: return Theme.tokenNumber
        case .comment: return Theme.textFaint
        }
    }
}

private extension AttributedString {
    func index(afterCharacters count: Int) -> AttributedString.Index? {
        characters.index(startIndex, offsetBy: count, limitedBy: endIndex)
    }
}

// MARK: - Blockquote

struct BlockquoteView: View {
    let children: [MDBlock]
    var cacheKey: String = ""
    var streaming = false

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(children.enumerated()), id: \.offset) { ix, child in
                MarkdownBlockView(block: child, cacheKey: "\(cacheKey)/q\(ix)",
                                  streaming: streaming)
                    .foregroundStyle(Theme.textMuted)
            }
        }
        .padding(.leading, 12)
        .padding(.trailing, 10)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            UnevenRoundedRectangle(topLeadingRadius: 0, bottomLeadingRadius: 0,
                                   bottomTrailingRadius: 6, topTrailingRadius: 6)
                .fill(Theme.accent.opacity(0.05))
        )
        .overlay(alignment: .leading) {
            Rectangle().fill(Theme.accent.opacity(0.6)).frame(width: 2)
        }
    }
}

// MARK: - Lists

struct ListBlockView: View {
    let orderedStart: Int?
    let items: [MDListItem]
    var cacheKey: String = ""
    var streaming = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(Array(items.enumerated()), id: \.offset) { ix, item in
                HStack(alignment: .top, spacing: 8) {
                    marker(ix: ix, item: item)
                        .frame(minWidth: 18, alignment: .trailing)
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(Array(item.children.enumerated()), id: \.offset) { cix, child in
                            MarkdownBlockView(block: child, cacheKey: "\(cacheKey)/l\(ix).\(cix)",
                                              streaming: streaming)
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder
    private func marker(ix: Int, item: MDListItem) -> some View {
        if let checked = item.checked {
            Image(systemName: checked ? "checkmark.square.fill" : "square")
                .font(.system(size: 12))
                .foregroundStyle(checked ? Theme.accent.opacity(0.85) : Theme.textMuted)
                .frame(height: MD.lineHeight)
        } else if let start = orderedStart {
            Text("\(start + ix).")
                .font(Theme.sans(MD.textSize))
                .foregroundStyle(Theme.accent.opacity(0.85))
                .frame(height: MD.lineHeight)
        } else {
            Circle()
                .fill(Theme.accent.opacity(0.85))
                .frame(width: 5, height: 5)
                .frame(height: MD.lineHeight)
        }
    }
}

// MARK: - Table

struct TableBlockView: View {
    let header: [[InlineRun]]
    let rows: [[[InlineRun]]]
    let align: [MDAlign]

    var body: some View {
        // Frameless: hairline rules only, no header fill, no outer radius
        // (desktop render.rs tables). Grid keeps columns content-proportional.
        ScrollView(.horizontal, showsIndicators: false) {
            Grid(alignment: .leading, horizontalSpacing: 0, verticalSpacing: 0) {
                GridRow {
                    ForEach(Array(header.enumerated()), id: \.offset) { ix, cell in
                        cellView(cell, weight: .bold, column: ix)
                    }
                }
                divider
                ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                    GridRow {
                        ForEach(Array(row.enumerated()), id: \.offset) { ix, cell in
                            cellView(cell, weight: .regular, column: ix)
                        }
                    }
                    divider
                }
            }
        }
    }

    private var divider: some View {
        Rectangle().fill(whiteAlpha(0.10)).frame(height: 1)
            .gridCellUnsizedAxes(.horizontal)
    }

    private func cellView(_ runs: [InlineRun], weight: Font.Weight, column: Int) -> some View {
        let alignment: Alignment = column < align.count
            ? (align[column] == .center ? .center : align[column] == .right ? .trailing : .leading)
            : .leading
        return runs.styled(weight: weight)
            .textRenderer(InlineCodeRenderer())
            .lineSpacing(MD.lineHeight - MD.textSize - 4)
            .padding(12)
            .frame(minWidth: 48, maxWidth: .infinity, alignment: alignment)
    }
}

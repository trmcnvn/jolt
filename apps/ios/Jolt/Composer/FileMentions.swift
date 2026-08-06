import Foundation
import Observation
import SwiftUI

/// Active `@query` token in character offsets. Character offsets remain valid
/// across String copies and are converted back to indices only at replacement.
struct FileMentionToken: Equatable {
    var range: Range<Int>
    var query: String
}

/// An `@` opens completion only at a token boundary, matching desktop Jolt.
func fileMentionToken(in text: String, cursorOffset: Int) -> FileMentionToken? {
    guard cursorOffset >= 0, cursorOffset <= text.count else { return nil }
    let cursor = text.index(text.startIndex, offsetBy: cursorOffset)
    let beforeCursor = text[..<cursor]
    let tokenStart = beforeCursor.lastIndex(where: { $0.isWhitespace })
        .map { text.index(after: $0) } ?? text.startIndex
    guard let at = text[tokenStart..<cursor].lastIndex(of: "@") else { return nil }

    let validBoundary = at == text.startIndex || text[..<at].last.map {
        $0.isWhitespace || $0 == "(" || $0 == "[" || $0 == "{"
    } == true
    guard validBoundary, !text[text.index(after: at)..<cursor].contains("@") else { return nil }

    let end = text[cursor...].firstIndex(where: { $0.isWhitespace }) ?? text.endIndex
    return FileMentionToken(
        range: text.distance(from: text.startIndex, to: at)..<text.distance(from: text.startIndex, to: end),
        query: String(text[text.index(after: at)..<cursor])
    )
}

func fileMentionCursorOffset(in text: String, selection: TextSelection?) -> Int {
    guard let selection else { return text.count }
    switch selection.indices {
    case .selection(let range):
        // SwiftUI can publish a selection from the previous TextField value in
        // the same update pass. Convert through this String's UTF-16 view and
        // fall back to the end instead of applying a foreign index directly.
        guard let upper = range.upperBound.samePosition(in: text.utf16) else {
            return text.count
        }
        let utf16Offset = text.utf16.distance(from: text.utf16.startIndex, to: upper)
        let cursor = String.Index(utf16Offset: utf16Offset, in: text)
        return text.distance(from: text.startIndex, to: cursor)
    case .multiSelection:
        return text.count
    @unknown default:
        return text.count
    }
}

private let fileMentionScheme = "jolt-file:"

private func fileMentionPathIsSafe(_ path: String) -> Bool {
    !path.isEmpty
        && !path.hasPrefix("/")
        && !path.contains("\\")
        && !path.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        && !path.split(separator: "/", omittingEmptySubsequences: false).contains {
            $0.isEmpty || $0 == "." || $0 == ".."
        }
}

private func percentEncodeMentionPath(_ path: String) -> String {
    let allowed = Set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~/".utf8)
    return path.utf8.map { byte in
        allowed.contains(byte) ? String(UnicodeScalar(byte)) : String(format: "%%%02X", byte)
    }.joined()
}

private func percentDecodeMentionPath(_ encoded: String) -> String? {
    var bytes: [UInt8] = []
    let raw = Array(encoded.utf8)
    var index = 0
    while index < raw.count {
        if raw[index] == Character("%").asciiValue {
            guard index + 2 < raw.count,
                  let value = UInt8(String(decoding: raw[(index + 1)...(index + 2)], as: UTF8.self),
                                    radix: 16) else { return nil }
            bytes.append(value)
            index += 3
        } else {
            bytes.append(raw[index])
            index += 1
        }
    }
    return String(bytes: bytes, encoding: .utf8)
}

private func escapeMentionLabel(_ label: String) -> String {
    label.replacingOccurrences(of: "\\", with: "\\\\")
        .replacingOccurrences(of: "[", with: "\\[")
        .replacingOccurrences(of: "]", with: "\\]")
}

func fileMentionLink(path: String, isDirectory: Bool) -> String {
    let path = path.hasSuffix("/") ? String(path.dropLast()) : path
    let basename = path.split(separator: "/").last.map(String.init) ?? path
    let target = path + (isDirectory ? "/" : "")
    return "[\(escapeMentionLabel(basename))](\(fileMentionScheme)\(percentEncodeMentionPath(target)))"
}

private struct SelectedFileMention {
    var range: NSRange
    var display: String
    var path: String
    var isDirectory: Bool
}

/// Completion and selected-mention state for one local composer draft. The
/// editor displays compact `@name` text; submission projects selections into
/// the canonical `jolt-file:` Markdown used by desktop and the transcript.
@MainActor
@Observable
final class FileMentionDraft {
    private(set) var token: FileMentionToken?
    private(set) var results: [FileSearchMatch] = []
    private(set) var loading = false
    private(set) var error: String?

    @ObservationIgnored private var selected: [SelectedFileMention] = []
    @ObservationIgnored private var previousText = ""
    @ObservationIgnored private var contextKey = ""
    @ObservationIgnored private var generation: UInt64 = 0
    @ObservationIgnored private var searchTask: Task<Void, Never>?

    var isOpen: Bool { token != nil }

    func update(
        text: String,
        selection: TextSelection?,
        contextKey: String,
        search: @escaping (String) async throws -> [FileSearchMatch]
    ) {
        reconcile(text)
        let candidate = fileMentionToken(in: text,
                                         cursorOffset: fileMentionCursorOffset(in: text,
                                                                             selection: selection))
        let next = candidate.flatMap { isSelectedMentionToken($0, in: text) ? nil : $0 }
        guard next != token || contextKey != self.contextKey else { return }

        let refining = token != nil && next != nil && contextKey == self.contextKey
        token = next
        self.contextKey = contextKey
        generation &+= 1
        searchTask?.cancel()
        error = nil
        if !refining {
            results = []
        }
        guard let next else {
            loading = false
            return
        }

        loading = true
        let request = generation
        searchTask = Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 80_000_000)
            guard !Task.isCancelled else { return }
            do {
                let matches = try await search(next.query)
                guard let self, !Task.isCancelled, self.generation == request else { return }
                self.results = matches
                self.error = nil
                self.loading = false
            } catch {
                guard let self, !Task.isCancelled, self.generation == request else { return }
                self.results = []
                self.error = Self.message(for: error)
                self.loading = false
            }
        }
    }

    func dismiss() {
        generation &+= 1
        searchTask?.cancel()
        token = nil
        results = []
        loading = false
        error = nil
    }

    func accept(_ match: FileSearchMatch, in text: String) -> (text: String, selection: TextSelection)? {
        reconcile(text)
        guard let token else { return nil }
        let start = text.index(text.startIndex, offsetBy: token.range.lowerBound)
        let end = text.index(text.startIndex, offsetBy: token.range.upperBound)
        let display = "@" + (match.path.split(separator: "/").last.map(String.init) ?? match.path)
        let nextCharacter = end < text.endIndex ? text[end] : nil
        let usesExistingSeparator = nextCharacter?.isWhitespace == true
            && nextCharacter != "\n" && nextCharacter != "\r"
        let inserted = display + (usesExistingSeparator ? "" : " ")

        var updated = text
        updated.replaceSubrange(start..<end, with: inserted)
        let utf16Location = text.utf16.distance(from: text.utf16.startIndex,
                                                to: start.samePosition(in: text.utf16)!)
        reconcile(updated)
        selected.append(SelectedFileMention(
            range: NSRange(location: utf16Location, length: display.utf16.count),
            display: display,
            path: match.path,
            isDirectory: match.isDir
        ))
        dismiss()

        var caret = updated.index(updated.startIndex, offsetBy: token.range.lowerBound + inserted.count)
        if usesExistingSeparator, caret < updated.endIndex {
            caret = updated.index(after: caret)
        }
        return (updated, TextSelection(insertionPoint: caret))
    }

    func encodedPrompt(_ text: String) -> String {
        reconcile(text)
        var encoded = text
        for mention in selected.sorted(by: { $0.range.location > $1.range.location }) {
            guard let range = Range(mention.range, in: encoded),
                  encoded[range] == mention.display else { continue }
            encoded.replaceSubrange(range, with: fileMentionLink(path: mention.path,
                                                                  isDirectory: mention.isDirectory))
        }
        return encoded
    }

    func reset() {
        dismiss()
        selected = []
        previousText = ""
    }

    private func isSelectedMentionToken(_ token: FileMentionToken, in text: String) -> Bool {
        let start = text.index(text.startIndex, offsetBy: token.range.lowerBound)
        let end = text.index(text.startIndex, offsetBy: token.range.upperBound)
        guard let utf16Start = start.samePosition(in: text.utf16),
              let utf16End = end.samePosition(in: text.utf16) else { return false }
        let range = NSRange(
            location: text.utf16.distance(from: text.utf16.startIndex, to: utf16Start),
            length: text.utf16.distance(from: utf16Start, to: utf16End)
        )
        return selected.contains { NSIntersectionRange($0.range, range).length > 0 }
    }

    /// Preserve selected mention ranges through ordinary edits. Any edit that
    /// touches a mention turns it back into ordinary text rather than carrying
    /// stale path metadata.
    private func reconcile(_ text: String) {
        guard text != previousText else { return }
        let old = Array(previousText.utf16)
        let new = Array(text.utf16)
        var prefix = 0
        while prefix < old.count, prefix < new.count, old[prefix] == new[prefix] { prefix += 1 }
        var suffix = 0
        while suffix < old.count - prefix, suffix < new.count - prefix,
              old[old.count - 1 - suffix] == new[new.count - 1 - suffix] { suffix += 1 }

        let oldEnd = old.count - suffix
        let delta = new.count - old.count
        selected = selected.compactMap { mention in
            let mentionEnd = NSMaxRange(mention.range)
            if mentionEnd <= prefix { return mention }
            if mention.range.location >= oldEnd {
                var shifted = mention
                shifted.range.location += delta
                return shifted
            }
            return nil
        }
        previousText = text
    }

    private static func message(for error: Error) -> String {
        if let relay = error as? RelayError {
            switch relay {
            case .hostOffline, .notConnected, .timeout:
                return "The session's device is unreachable."
            case .rpc(let message) where message.hasPrefix("unknown method"):
                return "Update Jolt on the session's device to search its files."
            case .rpc:
                return "File search failed."
            }
        }
        return "File search failed."
    }
}

struct FileMentionMenu: View {
    let draft: FileMentionDraft
    let select: (FileSearchMatch) -> Void

    var body: some View {
        if draft.isOpen {
            VStack(spacing: 0) {
                if draft.loading && draft.results.isEmpty {
                    HStack(spacing: 10) {
                        ProgressView().controlSize(.small).tint(Theme.textMuted)
                        Text("Searching files…")
                        Spacer()
                    }
                    .foregroundStyle(Theme.textMuted)
                    .padding(.horizontal, 14)
                    .frame(height: 42)
                } else if let error = draft.error {
                    Text(error)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.danger)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
                } else if draft.results.isEmpty {
                    Text(draft.token?.query.isEmpty == true ? "No files available" : "No matching files")
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textMuted)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
                } else {
                    ForEach(draft.results) { result in
                        Button { select(result) } label: {
                            HStack(spacing: 10) {
                                Image(systemName: result.isDir ? "folder" : "doc")
                                    .font(.system(size: 13))
                                    .foregroundStyle(Theme.textMuted)
                                    .frame(width: 16)
                                Text(result.path + (result.isDir ? "/" : ""))
                                    .font(Theme.mono(12))
                                    .foregroundStyle(Theme.text)
                                    .lineLimit(1)
                                Spacer(minLength: 0)
                            }
                            .padding(.horizontal, 14)
                            .frame(height: 40)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
            .background(Theme.surfaceRaised.opacity(0.96), in: RoundedRectangle(cornerRadius: 14))
            .overlay(RoundedRectangle(cornerRadius: 14).strokeBorder(whiteAlpha(0.06)))
            .padding(.horizontal, 16)
        }
    }
}

struct FileMentionProjection {
    var text: AttributedString
    var plainText: String
    /// Canonical prompt with valid file mentions converted to inline code so
    /// the transcript can parse the rest as normal Markdown.
    var markdownText: String
}

/// Project canonical sent-message links into compact read-only chips.
func projectFileMentions(_ raw: String) -> FileMentionProjection {
    let pattern = #"\[((?:\\.|[^\]])*)\]\(jolt-file:([^\)]+)\)"#
    guard let regex = try? NSRegularExpression(pattern: pattern) else {
        return FileMentionProjection(text: AttributedString(raw), plainText: raw, markdownText: raw)
    }
    let matches = regex.matches(in: raw, range: NSRange(raw.startIndex..., in: raw))
    guard !matches.isEmpty else {
        return FileMentionProjection(text: AttributedString(raw), plainText: raw, markdownText: raw)
    }

    struct ValidMention {
        var range: Range<String.Index>
        var path: String
        var isDirectory: Bool
    }
    let mentions = matches.compactMap { match -> ValidMention? in
        guard let whole = Range(match.range(at: 0), in: raw),
              let labelRange = Range(match.range(at: 1), in: raw),
              let targetRange = Range(match.range(at: 2), in: raw),
              let decoded = percentDecodeMentionPath(String(raw[targetRange])) else { return nil }
        let isDirectory = decoded.hasSuffix("/")
        let path = isDirectory ? String(decoded.dropLast()) : decoded
        let basename = path.split(separator: "/").last.map(String.init) ?? path
        guard fileMentionPathIsSafe(path),
              percentEncodeMentionPath(decoded) == raw[targetRange],
              escapeMentionLabel(basename) == raw[labelRange] else { return nil }
        return ValidMention(range: whole, path: path, isDirectory: isDirectory)
    }
    guard !mentions.isEmpty else {
        return FileMentionProjection(text: AttributedString(raw), plainText: raw, markdownText: raw)
    }

    var attributed = AttributedString()
    var plain = ""
    var markdown = ""
    var cursor = raw.startIndex
    for mention in mentions {
        let prefix = String(raw[cursor..<mention.range.lowerBound])
        attributed.append(AttributedString(prefix))
        plain += prefix
        markdown += prefix

        let label = "@" + (mention.path.split(separator: "/").last.map(String.init) ?? mention.path)
        var chip = AttributedString(label)
        chip.font = Theme.mono(MD.textSize)
        chip.foregroundColor = Theme.text
        chip.backgroundColor = whiteAlpha(0.08)
        attributed.append(chip)
        plain += label
        markdown += markdownCodeSpan(label)
        cursor = mention.range.upperBound
    }
    let suffix = String(raw[cursor...])
    attributed.append(AttributedString(suffix))
    plain += suffix
    markdown += suffix
    return FileMentionProjection(text: attributed, plainText: plain, markdownText: markdown)
}

private func markdownCodeSpan(_ text: String) -> String {
    var longest = 0
    var current = 0
    for character in text {
        if character == "`" {
            current += 1
            longest = max(longest, current)
        } else {
            current = 0
        }
    }
    let delimiter = String(repeating: "`", count: longest + 1)
    return delimiter + text + delimiter
}

// Line-by-line syntax tokenizer — a port of crates/ui/src/markdown/highlight.rs.
//
// Paint-only: tokens recolor text runs on the same mono font, so highlighting
// can never change layout. Lines tokenize independently with a small carry
// state for block comments / multiline strings, which lets code blocks render
// line-per-row and lets highlighting arrive asynchronously without reflow.

import Foundation

enum TokenClass: Equatable {
    case keyword
    case stringLit
    case comment
    case number
}

/// A classified span within a single line (character offsets).
struct TokenSpan {
    var range: Range<Int>
    var cls: TokenClass
}

struct StringSpec {
    let open: String
    let close: String
    let multiline: Bool
    let escapes: Bool
}

enum HighlightLanguage: String {
    case rust, javascript, python, go, json, bash, toml, markdown
    case c, cpp, java, cSharp, kotlin, swift, ruby, lua, php, sql, yaml, html, css, dockerfile

    static func forTag(_ tag: String?) -> HighlightLanguage? {
        guard let tag = tag?.lowercased() else { return nil }
        switch tag {
        case "rust", "rs": return .rust
        case "js", "jsx", "ts", "tsx", "javascript", "typescript", "mjs", "cjs": return .javascript
        case "py", "python": return .python
        case "go", "golang": return .go
        case "json", "jsonc": return .json
        case "sh", "bash", "zsh", "shell", "console": return .bash
        case "toml": return .toml
        case "md", "markdown": return .markdown
        case "c", "h": return .c
        case "cpp", "cxx", "cc", "hpp", "hxx", "hh", "c++", "cplusplus": return .cpp
        case "java": return .java
        case "cs", "csharp", "c#": return .cSharp
        case "kt", "kts", "kotlin": return .kotlin
        case "swift": return .swift
        case "rb", "ruby": return .ruby
        case "lua": return .lua
        case "php": return .php
        case "sql": return .sql
        case "yaml", "yml": return .yaml
        case "html", "htm": return .html
        case "css": return .css
        case "dockerfile", "docker": return .dockerfile
        default: return nil
        }
    }

    var keywords: Set<String> {
        switch self {
        case .rust:
            return ["as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                    "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
                    "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
                    "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while"]
        case .javascript:
            return ["abstract", "any", "as", "async", "await", "boolean", "break", "case", "catch",
                    "class", "const", "continue", "default", "delete", "do", "else", "enum", "export",
                    "extends", "false", "finally", "for", "from", "function", "if", "implements", "import",
                    "in", "instanceof", "interface", "let", "new", "null", "number", "of", "private",
                    "protected", "public", "readonly", "return", "static", "string", "super", "switch",
                    "this", "throw", "true", "try", "type", "typeof", "undefined", "var", "void", "while",
                    "yield"]
        case .python:
            return ["False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
                    "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
                    "if", "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise",
                    "return", "try", "while", "with", "yield"]
        case .go:
            return ["break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
                    "false", "for", "func", "go", "goto", "if", "import", "interface", "map", "nil", "package",
                    "range", "return", "select", "struct", "switch", "true", "type", "var"]
        case .json:
            return ["true", "false", "null"]
        case .bash:
            return ["case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for", "function",
                    "if", "in", "local", "return", "select", "then", "until", "while"]
        case .toml:
            return ["true", "false"]
        case .markdown:
            return []
        case .c:
            return ["_Alignas", "_Alignof", "_Atomic", "_Bool", "_Complex", "_Generic", "_Imaginary",
                    "_Noreturn", "_Static_assert", "_Thread_local", "auto", "break", "case", "char", "const",
                    "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto",
                    "if", "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof",
                    "static", "struct", "switch", "typedef", "union", "unsigned", "void", "volatile", "while"]
        case .cpp:
            return ["alignas", "alignof", "and", "and_eq", "asm", "auto", "bitand", "bitor", "bool", "break",
                    "case", "catch", "char", "char8_t", "char16_t", "char32_t", "class", "co_await", "co_return",
                    "co_yield", "compl", "concept", "const", "const_cast", "consteval", "constexpr", "constinit",
                    "continue", "decltype", "default", "delete", "do", "double", "dynamic_cast", "else", "enum",
                    "explicit", "export", "extern", "false", "float", "for", "friend", "goto", "if", "inline",
                    "int", "long", "mutable", "namespace", "new", "noexcept", "not", "not_eq", "nullptr",
                    "operator", "or", "or_eq", "private", "protected", "public", "register", "reinterpret_cast",
                    "requires", "return", "short", "signed", "sizeof", "static", "static_assert", "static_cast",
                    "struct", "switch", "template", "this", "thread_local", "throw", "true", "try", "typedef",
                    "typeid", "typename", "union", "unsigned", "using", "virtual", "void", "volatile", "wchar_t",
                    "while", "xor", "xor_eq"]
        case .java:
            return ["abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
                    "continue", "default", "do", "double", "else", "enum", "exports", "extends", "false", "final",
                    "finally", "float", "for", "goto", "if", "implements", "import", "instanceof", "int", "interface",
                    "long", "module", "native", "new", "null", "open", "opens", "package", "permits", "private",
                    "protected", "provides", "public", "record", "requires", "return", "sealed", "short", "static",
                    "strictfp", "super", "switch", "synchronized", "this", "throw", "throws", "to", "transient",
                    "true", "try", "uses", "var", "void", "volatile", "while", "with", "yield"]
        case .cSharp:
            return ["abstract", "add", "alias", "and", "as", "ascending", "async", "await", "base", "bool", "break",
                    "by", "byte", "case", "catch", "char", "checked", "class", "const", "continue", "decimal",
                    "default", "delegate", "descending", "do", "double", "dynamic", "else", "enum", "equals", "event",
                    "explicit", "extern", "false", "file", "finally", "fixed", "float", "for", "foreach", "from", "get",
                    "global", "goto", "group", "if", "implicit", "in", "init", "int", "interface", "internal", "into",
                    "is", "join", "let", "lock", "long", "managed", "nameof", "namespace", "new", "nint", "not",
                    "notnull", "nuint", "null", "object", "on", "operator", "or", "orderby", "out", "override", "params",
                    "partial", "private", "protected", "public", "readonly", "record", "ref", "remove", "required",
                    "return", "sbyte", "scoped", "sealed", "select", "set", "short", "sizeof", "stackalloc", "static",
                    "string", "struct", "switch", "this", "throw", "true", "try", "typeof", "uint", "ulong", "unchecked",
                    "unmanaged", "unsafe", "ushort", "using", "value", "var", "virtual", "void", "volatile", "when",
                    "where", "while", "with", "yield"]
        case .kotlin:
            return ["actual", "abstract", "annotation", "as", "break", "by", "catch", "class", "companion", "const",
                    "constructor", "continue", "crossinline", "data", "delegate", "do", "dynamic", "else", "enum", "expect",
                    "external", "false", "field", "file", "final", "finally", "for", "fun", "get", "if", "import", "in",
                    "infix", "init", "inline", "inner", "interface", "internal", "is", "lateinit", "noinline", "null", "object",
                    "open", "operator", "out", "override", "package", "private", "protected", "public", "reified", "return",
                    "sealed", "set", "suspend", "tailrec", "this", "throw", "true", "try", "typealias", "val", "var", "vararg",
                    "when", "where", "while"]
        case .swift:
            return ["Any", "Self", "actor", "as", "associatedtype", "async", "await", "break", "case", "catch", "class",
                    "continue", "convenience", "copy", "default", "defer", "deinit", "didSet", "distributed", "do", "dynamic",
                    "else", "enum", "extension", "fallthrough", "false", "fileprivate", "final", "for", "func", "get", "guard",
                    "if", "import", "in", "indirect", "infix", "init", "inout", "internal", "is", "isolated", "lazy", "let",
                    "macro", "mutating", "nil", "nonisolated", "nonmutating", "open", "operator", "optional", "override", "package",
                    "postfix", "precedencegroup", "prefix", "private", "protocol", "public", "repeat", "required", "rethrows",
                    "return", "self", "set", "some", "static", "struct", "subscript", "super", "switch", "throw", "throws", "true",
                    "try", "typealias", "unowned", "var", "weak", "where", "while", "willSet", "yield"]
        case .ruby:
            return ["BEGIN", "END", "__ENCODING__", "__FILE__", "__LINE__", "alias", "and", "begin", "break", "case",
                    "class", "def", "defined", "do", "else", "elsif", "end", "ensure", "false", "for", "if", "in", "module",
                    "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "true", "undef",
                    "unless", "until", "when", "while", "yield"]
        case .lua:
            return ["and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local",
                    "nil", "not", "or", "repeat", "return", "then", "true", "until", "while"]
        case .php:
            return ["__class__", "__dir__", "__file__", "__function__", "__line__", "__method__", "__namespace__", "__trait__",
                    "abstract", "and", "array", "as", "break", "callable", "case", "catch", "class", "clone", "const", "continue",
                    "declare", "default", "die", "do", "echo", "else", "elseif", "empty", "enddeclare", "endfor", "endforeach",
                    "endif", "endswitch", "endwhile", "enum", "eval", "exit", "extends", "false", "final", "finally", "fn", "for",
                    "foreach", "from", "function", "global", "goto", "if", "implements", "include", "include_once", "instanceof",
                    "insteadof", "interface", "isset", "list", "match", "namespace", "new", "null", "or", "print", "private",
                    "protected", "public", "readonly", "require", "require_once", "return", "static", "switch", "throw", "trait",
                    "true", "try", "unset", "use", "var", "while", "xor", "yield"]
        case .sql:
            return ["all", "alter", "and", "any", "as", "asc", "begin", "between", "by", "case", "check", "column", "commit",
                    "constraint", "create", "cross", "database", "default", "delete", "desc", "distinct", "do", "drop", "else", "end",
                    "escape", "except", "exists", "false", "fetch", "for", "foreign", "from", "full", "grant", "group", "having", "if",
                    "in", "index", "inner", "insert", "intersect", "into", "is", "join", "key", "left", "like", "limit", "merge",
                    "natural", "not", "null", "offset", "on", "or", "order", "outer", "over", "partition", "primary", "references",
                    "returning", "revoke", "right", "rollback", "row", "rows", "schema", "select", "set", "table", "then", "transaction",
                    "trigger", "true", "truncate", "union", "unique", "update", "using", "values", "view", "when", "where", "window", "with"]
        case .yaml:
            return ["false", "no", "null", "off", "on", "true", "yes"]
        case .html:
            return ["a", "article", "aside", "audio", "body", "button", "canvas", "code", "col", "data", "datalist", "details",
                    "dialog", "div", "em", "fieldset", "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "head",
                    "header", "html", "iframe", "img", "input", "label", "legend", "li", "link", "main", "meta", "nav", "ol",
                    "option", "picture", "pre", "script", "section", "select", "slot", "small", "source", "span", "strong", "style",
                    "summary", "table", "tbody", "td", "template", "textarea", "tfoot", "th", "thead", "time", "title", "tr", "track",
                    "ul", "video"]
        case .css:
            return ["align-content", "align-items", "align-self", "all", "animation", "appearance", "aspect-ratio", "auto", "background",
                    "background-color", "block", "border", "border-color", "border-radius", "bottom", "box-shadow", "box-sizing", "color",
                    "column", "content", "cursor", "display", "filter", "flex", "flex-basis", "flex-direction", "flex-grow", "flex-shrink",
                    "flex-wrap", "font", "font-family", "font-size", "font-style", "font-weight", "gap", "grid", "grid-area", "grid-column",
                    "grid-row", "grid-template", "height", "hidden", "inherit", "initial", "inline", "inline-block", "inset", "justify-content",
                    "left", "letter-spacing", "line-height", "list-style", "margin", "max-height", "max-width", "media", "min-height", "min-width",
                    "none", "object-fit", "opacity", "order", "outline", "overflow", "padding", "pointer-events", "position", "relative", "repeat",
                    "right", "rotate", "scale", "sticky", "text-align", "text-decoration", "text-overflow", "text-transform", "top", "transform",
                    "transition", "translate", "transparent", "unset", "user-select", "vertical-align", "visibility", "white-space", "width",
                    "word-break", "z-index"]
        case .dockerfile:
            return ["add", "arg", "cmd", "copy", "entrypoint", "env", "expose", "from", "healthcheck", "label", "maintainer", "onbuild",
                    "run", "shell", "stopsignal", "user", "volume", "workdir"]
        }
    }

    var lineComments: [String] {
        switch self {
        case .rust, .javascript, .go, .c, .cpp, .java, .cSharp, .kotlin, .swift: return ["//"]
        case .python, .bash, .toml, .ruby, .yaml, .dockerfile: return ["#"]
        case .lua, .sql: return ["--"]
        case .php: return ["//", "#"]
        case .json, .markdown, .html, .css: return []
        }
    }

    var commentNeedsBoundary: Bool {
        switch self {
        case .python, .bash, .toml, .yaml, .dockerfile: return true
        default: return false
        }
    }

    var blockComment: (open: String, close: String)? {
        switch self {
        case .rust, .javascript, .go, .c, .cpp, .java, .cSharp, .kotlin, .swift, .php, .sql, .css:
            return ("/*", "*/")
        case .lua: return ("--[[", "]]" )
        case .html: return ("<!--", "-->")
        default: return nil
        }
    }

    var strings: [StringSpec] {
        let double = StringSpec(open: "\"", close: "\"", multiline: false, escapes: true)
        let single = StringSpec(open: "'", close: "'", multiline: false, escapes: true)
        let literalSingle = StringSpec(open: "'", close: "'", multiline: false, escapes: false)
        let triple = StringSpec(open: "\"\"\"", close: "\"\"\"", multiline: true, escapes: true)
        switch self {
        case .javascript:
            return [StringSpec(open: "`", close: "`", multiline: true, escapes: true), double, single]
        case .python:
            return [triple, StringSpec(open: "'''", close: "'''", multiline: true, escapes: true), double, single]
        case .go:
            return [StringSpec(open: "`", close: "`", multiline: true, escapes: false), double, single]
        case .toml:
            return [triple, double, literalSingle]
        case .java, .kotlin, .swift:
            return [triple, double, single]
        case .cSharp:
            return [StringSpec(open: "\"\"\"", close: "\"\"\"", multiline: true, escapes: false),
                    StringSpec(open: "@\"", close: "\"", multiline: true, escapes: false), double, single]
        case .lua:
            return [StringSpec(open: "[[", close: "]]", multiline: true, escapes: false), double, single]
        case .php:
            return [double, single, StringSpec(open: "`", close: "`", multiline: false, escapes: true)]
        case .sql:
            return [literalSingle, StringSpec(open: "\"", close: "\"", multiline: false, escapes: false),
                    StringSpec(open: "`", close: "`", multiline: false, escapes: false)]
        case .yaml:
            return [double, literalSingle]
        case .markdown:
            return [StringSpec(open: "`", close: "`", multiline: false, escapes: false)]
        case .rust, .c, .cpp, .ruby, .html, .css, .dockerfile:
            return [double, single]
        case .json:
            return [double]
        case .bash:
            return [double, literalSingle]
        }
    }

    var caseInsensitiveKeywords: Bool {
        switch self {
        case .php, .sql, .yaml, .html, .css, .dockerfile: return true
        default: return false
        }
    }

    var allowsHyphenInIdentifier: Bool {
        self == .yaml || self == .html || self == .css
    }
}

struct StringCarry: Equatable {
    let close: String
    let escapes: Bool
}

/// Carry state across lines (block comments / multiline strings).
struct LineCarry: Equatable {
    var blockCommentClose: String?
    var string: StringCarry?
}

enum Highlighter {
    /// Tokenize all lines of a code block. Pure; run off the main actor.
    static func highlight(code: String, language: HighlightLanguage) -> [[TokenSpan]] {
        var carry = LineCarry()
        return code.components(separatedBy: "\n").map { line in
            tokenizeLine(Array(line), language: language, carry: &carry)
        }
    }

    static func tokenizeLine(_ chars: [Character], language lang: HighlightLanguage, carry: inout LineCarry) -> [TokenSpan] {
        var spans: [TokenSpan] = []
        var i = 0
        let n = chars.count
        let keywords = lang.keywords

        func matches(_ pattern: String, at index: Int) -> Bool {
            let patternChars = Array(pattern)
            guard index + patternChars.count <= n else { return false }
            for (offset, character) in patternChars.enumerated() where chars[index + offset] != character {
                return false
            }
            return true
        }

        func scanClose(_ close: String, escapes: Bool, from start: Int) -> Int? {
            var index = start
            while index < n {
                if escapes, chars[index] == "\\" {
                    index += min(2, n - index)
                    continue
                }
                if matches(close, at: index) { return index + close.count }
                index += 1
            }
            return nil
        }

        if lang == .markdown, String(chars).trimmingCharacters(in: .whitespaces).hasPrefix("#") {
            return chars.isEmpty ? [] : [TokenSpan(range: 0..<n, cls: .keyword)]
        }

        // Resume a multi-line construct.
        if let close = carry.blockCommentClose {
            let start = i
            if let end = scanClose(close, escapes: false, from: i) {
                i = end
                carry.blockCommentClose = nil
            } else {
                i = n
            }
            if start < i { spans.append(TokenSpan(range: start..<i, cls: .comment)) }
            if i == n { return spans }
        } else if let string = carry.string {
            let start = i
            if let end = scanClose(string.close, escapes: string.escapes, from: i) {
                i = end
                carry.string = nil
            } else {
                i = n
            }
            if start < i { spans.append(TokenSpan(range: start..<i, cls: .stringLit)) }
            if i == n { return spans }
        }

        while i < n {
            // Block comments are checked before line comments because Lua's
            // multi-line opener extends its line-comment prefix.
            if let block = lang.blockComment, matches(block.open, at: i) {
                let start = i
                i += block.open.count
                if let end = scanClose(block.close, escapes: false, from: i) {
                    i = end
                } else {
                    i = n
                    carry.blockCommentClose = block.close
                }
                spans.append(TokenSpan(range: start..<i, cls: .comment))
                continue
            }

            if let comment = lang.lineComments.first(where: { matches($0, at: i) }) {
                let boundaryOK = !lang.commentNeedsBoundary || i == 0 || chars[i - 1].isWhitespace
                if boundaryOK {
                    _ = comment
                    spans.append(TokenSpan(range: i..<n, cls: .comment))
                    break
                }
            }

            if let spec = lang.strings.first(where: { matches($0.open, at: i) }) {
                let start = i
                i += spec.open.count
                if let end = scanClose(spec.close, escapes: spec.escapes, from: i) {
                    i = end
                } else {
                    i = n
                    if spec.multiline {
                        carry.string = StringCarry(close: spec.close, escapes: spec.escapes)
                    }
                }
                spans.append(TokenSpan(range: start..<i, cls: .stringLit))
                continue
            }

            let character = chars[i]

            if character.isNumber {
                let start = i
                while i < n, chars[i].isHexDigit || chars[i] == "." || chars[i] == "_"
                    || chars[i] == "x" || chars[i] == "o" || chars[i] == "b" || chars[i] == "e" {
                    i += 1
                }
                if start == 0 || !(chars[start - 1].isLetter || chars[start - 1] == "_") {
                    spans.append(TokenSpan(range: start..<i, cls: .number))
                }
                continue
            }

            if character.isLetter || character == "_" {
                let start = i
                while i < n, chars[i].isLetter || chars[i].isNumber || chars[i] == "_"
                    || (lang.allowsHyphenInIdentifier && chars[i] == "-") {
                    i += 1
                }
                let originalWord = String(chars[start..<i])
                let lookupWord = lang.caseInsensitiveKeywords ? originalWord.lowercased() : originalWord
                let listed = keywords.contains(lookupWord)
                let contextual: Bool
                switch lang {
                case .html:
                    let prefix = String(chars[..<start]).trimmingCharacters(in: .whitespaces)
                    contextual = listed && (prefix.hasSuffix("<") || prefix.hasSuffix("</") || prefix.hasSuffix("<!"))
                case .yaml:
                    let suffix = String(chars[i...]).trimmingCharacters(in: .whitespaces)
                    contextual = listed || suffix.hasPrefix(":")
                case .dockerfile:
                    contextual = listed && chars[..<start].allSatisfy(\.isWhitespace)
                default:
                    contextual = listed
                }
                if contextual {
                    spans.append(TokenSpan(range: start..<i, cls: .keyword))
                }
                continue
            }

            i += 1
        }
        return spans
    }
}

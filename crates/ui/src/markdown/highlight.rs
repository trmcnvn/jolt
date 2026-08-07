//! Lightweight, paint-only syntax tokenizer.
//!
//! Keyword / string / comment / number classes only, line-by-line with a small
//! carry state for constructs that span lines (block comments, multi-line
//! strings). Results become `TextRun` colors on the same mono font, so layout is
//! identical whether or not highlighting has landed — "highlight is pure paint"
//! (docs/using-jolt.md).
//!
//! Tokenization runs off the render path (background executor, time-sliced by
//! the caller); this module is pure and synchronous.

use std::ops::Range;

/// Token paint class. `Plain` gaps are implicit (only non-plain spans are emitted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenClass {
    Keyword,
    StringLit,
    Comment,
    Number,
}

/// One highlighted span within a line (byte offsets into that line).
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub range: Range<usize>,
    pub class: TokenClass,
}

/// Cross-line scanner state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCarry {
    #[default]
    None,
    /// Inside a block comment.
    BlockComment,
    /// Inside a multi-line string; the value indexes the language's string specs.
    InString(u8),
}

/// Supported languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Js,
    Python,
    Go,
    Json,
    Bash,
    Toml,
    Markdown,
    C,
    Cpp,
    Java,
    CSharp,
    Kotlin,
    Swift,
    Ruby,
    Lua,
    Php,
    Sql,
    Yaml,
    Html,
    Css,
    Dockerfile,
}

/// Map a fenced-code info tag to a language.
pub fn lang_for_tag(tag: &str) -> Option<Lang> {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(Lang::Rust),
        "ts" | "tsx" | "typescript" | "js" | "jsx" | "javascript" | "mjs" | "cjs" => Some(Lang::Js),
        "python" | "py" => Some(Lang::Python),
        "go" | "golang" => Some(Lang::Go),
        "json" | "jsonc" => Some(Lang::Json),
        "bash" | "sh" | "shell" | "zsh" | "console" => Some(Lang::Bash),
        "toml" => Some(Lang::Toml),
        "md" | "markdown" => Some(Lang::Markdown),
        "c" | "h" => Some(Lang::C),
        "cpp" | "cxx" | "cc" | "hpp" | "hxx" | "hh" | "c++" | "cplusplus" => Some(Lang::Cpp),
        "java" => Some(Lang::Java),
        "cs" | "csharp" | "c#" => Some(Lang::CSharp),
        "kt" | "kts" | "kotlin" => Some(Lang::Kotlin),
        "swift" => Some(Lang::Swift),
        "rb" | "ruby" => Some(Lang::Ruby),
        "lua" => Some(Lang::Lua),
        "php" => Some(Lang::Php),
        "sql" => Some(Lang::Sql),
        "yaml" | "yml" => Some(Lang::Yaml),
        "html" | "htm" => Some(Lang::Html),
        "css" => Some(Lang::Css),
        "dockerfile" | "docker" => Some(Lang::Dockerfile),
        _ => None,
    }
}

struct StringSpec {
    open: &'static str,
    close: &'static str,
    multiline: bool,
    escapes: bool,
}

struct LangSpec {
    line_comments: &'static [&'static str],
    /// Line comments that must be at line start or after whitespace (`#` langs).
    comment_needs_boundary: bool,
    block_comment: Option<(&'static str, &'static str)>,
    strings: &'static [StringSpec],
    keywords: &'static [&'static str],
}

const DQ: StringSpec = StringSpec {
    open: "\"",
    close: "\"",
    multiline: false,
    escapes: true,
};
const SQ: StringSpec = StringSpec {
    open: "'",
    close: "'",
    multiline: false,
    escapes: true,
};

fn spec(lang: Lang) -> &'static LangSpec {
    match lang {
        Lang::Rust => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[DQ, SQ],
            keywords: &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
        },
        Lang::Js => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "`",
                    close: "`",
                    multiline: true,
                    escapes: true,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "abstract",
                "any",
                "as",
                "async",
                "await",
                "boolean",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "default",
                "delete",
                "do",
                "else",
                "enum",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "implements",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "number",
                "of",
                "private",
                "protected",
                "public",
                "readonly",
                "return",
                "static",
                "string",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "type",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
            ],
        },
        Lang::C => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[DQ, SQ],
            keywords: &[
                "_Alignas",
                "_Alignof",
                "_Atomic",
                "_Bool",
                "_Complex",
                "_Generic",
                "_Imaginary",
                "_Noreturn",
                "_Static_assert",
                "_Thread_local",
                "auto",
                "break",
                "case",
                "char",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extern",
                "float",
                "for",
                "goto",
                "if",
                "inline",
                "int",
                "long",
                "register",
                "restrict",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "typedef",
                "union",
                "unsigned",
                "void",
                "volatile",
                "while",
            ],
        },
        Lang::Cpp => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[DQ, SQ],
            keywords: &[
                "alignas",
                "alignof",
                "and",
                "and_eq",
                "asm",
                "atomic_cancel",
                "atomic_commit",
                "atomic_noexcept",
                "auto",
                "bitand",
                "bitor",
                "bool",
                "break",
                "case",
                "catch",
                "char",
                "char8_t",
                "char16_t",
                "char32_t",
                "class",
                "co_await",
                "co_return",
                "co_yield",
                "compl",
                "concept",
                "const",
                "const_cast",
                "consteval",
                "constexpr",
                "constinit",
                "continue",
                "decltype",
                "default",
                "delete",
                "do",
                "double",
                "dynamic_cast",
                "else",
                "enum",
                "explicit",
                "export",
                "extern",
                "false",
                "float",
                "for",
                "friend",
                "goto",
                "if",
                "inline",
                "int",
                "long",
                "mutable",
                "namespace",
                "new",
                "noexcept",
                "not",
                "not_eq",
                "nullptr",
                "operator",
                "or",
                "or_eq",
                "private",
                "protected",
                "public",
                "reflexpr",
                "register",
                "reinterpret_cast",
                "requires",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "static_assert",
                "static_cast",
                "struct",
                "switch",
                "template",
                "this",
                "thread_local",
                "throw",
                "true",
                "try",
                "typedef",
                "typeid",
                "typename",
                "union",
                "unsigned",
                "using",
                "virtual",
                "void",
                "volatile",
                "wchar_t",
                "while",
                "xor",
                "xor_eq",
            ],
        },
        Lang::Java => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: true,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "abstract",
                "assert",
                "boolean",
                "break",
                "byte",
                "case",
                "catch",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "exports",
                "extends",
                "false",
                "final",
                "finally",
                "float",
                "for",
                "goto",
                "if",
                "implements",
                "import",
                "instanceof",
                "int",
                "interface",
                "long",
                "module",
                "native",
                "new",
                "non-sealed",
                "null",
                "open",
                "opens",
                "package",
                "permits",
                "private",
                "protected",
                "provides",
                "public",
                "record",
                "requires",
                "return",
                "sealed",
                "short",
                "static",
                "strictfp",
                "super",
                "switch",
                "synchronized",
                "this",
                "throw",
                "throws",
                "to",
                "transient",
                "true",
                "try",
                "uses",
                "var",
                "void",
                "volatile",
                "while",
                "with",
                "yield",
            ],
        },
        Lang::CSharp => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: false,
                },
                StringSpec {
                    open: "@\"",
                    close: "\"",
                    multiline: true,
                    escapes: false,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "abstract",
                "add",
                "alias",
                "and",
                "as",
                "ascending",
                "async",
                "await",
                "base",
                "bool",
                "break",
                "by",
                "byte",
                "case",
                "catch",
                "char",
                "checked",
                "class",
                "const",
                "continue",
                "decimal",
                "default",
                "delegate",
                "descending",
                "do",
                "double",
                "dynamic",
                "else",
                "enum",
                "equals",
                "event",
                "explicit",
                "extern",
                "false",
                "file",
                "finally",
                "fixed",
                "float",
                "for",
                "foreach",
                "from",
                "get",
                "global",
                "goto",
                "group",
                "if",
                "implicit",
                "in",
                "init",
                "int",
                "interface",
                "internal",
                "into",
                "is",
                "join",
                "let",
                "lock",
                "long",
                "managed",
                "nameof",
                "namespace",
                "new",
                "nint",
                "not",
                "notnull",
                "nuint",
                "null",
                "object",
                "on",
                "operator",
                "or",
                "orderby",
                "out",
                "override",
                "params",
                "partial",
                "private",
                "protected",
                "public",
                "readonly",
                "record",
                "ref",
                "remove",
                "required",
                "return",
                "sbyte",
                "scoped",
                "sealed",
                "select",
                "set",
                "short",
                "sizeof",
                "stackalloc",
                "static",
                "string",
                "struct",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "uint",
                "ulong",
                "unchecked",
                "unmanaged",
                "unsafe",
                "ushort",
                "using",
                "value",
                "var",
                "virtual",
                "void",
                "volatile",
                "when",
                "where",
                "while",
                "with",
                "yield",
            ],
        },
        Lang::Kotlin => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: false,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "actual",
                "abstract",
                "annotation",
                "as",
                "break",
                "by",
                "catch",
                "class",
                "companion",
                "const",
                "constructor",
                "continue",
                "crossinline",
                "data",
                "delegate",
                "do",
                "dynamic",
                "else",
                "enum",
                "expect",
                "external",
                "false",
                "field",
                "file",
                "final",
                "finally",
                "for",
                "fun",
                "get",
                "if",
                "import",
                "in",
                "infix",
                "init",
                "inline",
                "inner",
                "interface",
                "internal",
                "is",
                "lateinit",
                "noinline",
                "null",
                "object",
                "open",
                "operator",
                "out",
                "override",
                "package",
                "private",
                "protected",
                "public",
                "reified",
                "return",
                "sealed",
                "set",
                "suspend",
                "tailrec",
                "this",
                "throw",
                "true",
                "try",
                "typealias",
                "val",
                "var",
                "vararg",
                "when",
                "where",
                "while",
            ],
        },
        Lang::Swift => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: true,
                },
                DQ,
            ],
            keywords: &[
                "Any",
                "Self",
                "actor",
                "as",
                "associatedtype",
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "continue",
                "convenience",
                "copy",
                "default",
                "defer",
                "deinit",
                "didSet",
                "distributed",
                "do",
                "dynamic",
                "else",
                "enum",
                "extension",
                "fallthrough",
                "false",
                "fileprivate",
                "final",
                "for",
                "func",
                "get",
                "guard",
                "if",
                "import",
                "in",
                "indirect",
                "infix",
                "init",
                "inout",
                "internal",
                "is",
                "isolated",
                "lazy",
                "let",
                "macro",
                "mutating",
                "nil",
                "nonisolated",
                "nonmutating",
                "open",
                "operator",
                "optional",
                "override",
                "package",
                "postfix",
                "precedencegroup",
                "prefix",
                "private",
                "protocol",
                "public",
                "repeat",
                "required",
                "rethrows",
                "return",
                "self",
                "set",
                "some",
                "static",
                "struct",
                "subscript",
                "super",
                "switch",
                "throws",
                "throw",
                "true",
                "try",
                "typealias",
                "unowned",
                "var",
                "weak",
                "where",
                "while",
                "willSet",
                "yield",
            ],
        },
        Lang::Python => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: true,
            block_comment: None,
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: true,
                },
                StringSpec {
                    open: "'''",
                    close: "'''",
                    multiline: true,
                    escapes: true,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
                "continue", "def", "del", "elif", "else", "except", "finally", "for", "from",
                "global", "if", "import", "in", "is", "lambda", "match", "nonlocal", "not", "or",
                "pass", "raise", "return", "try", "while", "with", "yield",
            ],
        },
        Lang::Go => &LangSpec {
            line_comments: &["//"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "`",
                    close: "`",
                    multiline: true,
                    escapes: false,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "false",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
        },
        Lang::Json => &LangSpec {
            line_comments: &[],
            comment_needs_boundary: false,
            block_comment: None,
            strings: &[DQ],
            keywords: &["true", "false", "null"],
        },
        Lang::Bash => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: true,
            block_comment: None,
            strings: &[
                DQ,
                StringSpec {
                    open: "'",
                    close: "'",
                    multiline: false,
                    escapes: false,
                },
            ],
            keywords: &[
                "case", "do", "done", "elif", "else", "esac", "exit", "export", "fi", "for",
                "function", "if", "in", "local", "return", "select", "then", "until", "while",
            ],
        },
        Lang::Toml => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: true,
            block_comment: None,
            strings: &[
                StringSpec {
                    open: "\"\"\"",
                    close: "\"\"\"",
                    multiline: true,
                    escapes: true,
                },
                DQ,
                StringSpec {
                    open: "'",
                    close: "'",
                    multiline: false,
                    escapes: false,
                },
            ],
            keywords: &["true", "false"],
        },
        Lang::Ruby => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: false,
            block_comment: None,
            strings: &[DQ, SQ],
            keywords: &[
                "BEGIN",
                "END",
                "__ENCODING__",
                "__FILE__",
                "__LINE__",
                "alias",
                "and",
                "begin",
                "break",
                "case",
                "class",
                "def",
                "defined",
                "do",
                "else",
                "elsif",
                "end",
                "ensure",
                "false",
                "for",
                "if",
                "in",
                "module",
                "next",
                "nil",
                "not",
                "or",
                "redo",
                "rescue",
                "retry",
                "return",
                "self",
                "super",
                "then",
                "true",
                "undef",
                "unless",
                "until",
                "when",
                "while",
                "yield",
            ],
        },
        Lang::Lua => &LangSpec {
            line_comments: &["--"],
            comment_needs_boundary: false,
            block_comment: Some(("--[[", "]]")),
            strings: &[
                StringSpec {
                    open: "[[",
                    close: "]]",
                    multiline: true,
                    escapes: false,
                },
                DQ,
                SQ,
            ],
            keywords: &[
                "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto",
                "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
                "until", "while",
            ],
        },
        Lang::Php => &LangSpec {
            line_comments: &["//", "#"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                DQ,
                StringSpec {
                    open: "'",
                    close: "'",
                    multiline: false,
                    escapes: true,
                },
                StringSpec {
                    open: "`",
                    close: "`",
                    multiline: false,
                    escapes: true,
                },
            ],
            keywords: &[
                "__CLASS__",
                "__DIR__",
                "__FILE__",
                "__FUNCTION__",
                "__LINE__",
                "__METHOD__",
                "__NAMESPACE__",
                "__TRAIT__",
                "abstract",
                "and",
                "array",
                "as",
                "break",
                "callable",
                "case",
                "catch",
                "class",
                "clone",
                "const",
                "continue",
                "declare",
                "default",
                "die",
                "do",
                "echo",
                "else",
                "elseif",
                "empty",
                "enddeclare",
                "endfor",
                "endforeach",
                "endif",
                "endswitch",
                "endwhile",
                "enum",
                "eval",
                "exit",
                "extends",
                "false",
                "final",
                "finally",
                "fn",
                "for",
                "foreach",
                "from",
                "function",
                "global",
                "goto",
                "if",
                "implements",
                "include",
                "include_once",
                "instanceof",
                "insteadof",
                "interface",
                "isset",
                "list",
                "match",
                "namespace",
                "new",
                "null",
                "or",
                "print",
                "private",
                "protected",
                "public",
                "readonly",
                "require",
                "require_once",
                "return",
                "static",
                "switch",
                "throw",
                "trait",
                "true",
                "try",
                "unset",
                "use",
                "var",
                "while",
                "xor",
                "yield",
            ],
        },
        Lang::Sql => &LangSpec {
            line_comments: &["--"],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[
                StringSpec {
                    open: "'",
                    close: "'",
                    multiline: false,
                    escapes: false,
                },
                StringSpec {
                    open: "\"",
                    close: "\"",
                    multiline: false,
                    escapes: false,
                },
                StringSpec {
                    open: "`",
                    close: "`",
                    multiline: false,
                    escapes: false,
                },
            ],
            keywords: &[
                "all",
                "alter",
                "and",
                "any",
                "as",
                "asc",
                "begin",
                "between",
                "by",
                "case",
                "check",
                "column",
                "commit",
                "constraint",
                "create",
                "cross",
                "database",
                "default",
                "delete",
                "desc",
                "distinct",
                "do",
                "drop",
                "else",
                "end",
                "escape",
                "except",
                "exists",
                "false",
                "fetch",
                "for",
                "foreign",
                "from",
                "full",
                "grant",
                "group",
                "having",
                "if",
                "in",
                "index",
                "inner",
                "insert",
                "intersect",
                "into",
                "is",
                "join",
                "key",
                "left",
                "like",
                "limit",
                "merge",
                "natural",
                "not",
                "null",
                "offset",
                "on",
                "or",
                "order",
                "outer",
                "over",
                "partition",
                "primary",
                "references",
                "returning",
                "revoke",
                "right",
                "rollback",
                "row",
                "rows",
                "schema",
                "select",
                "set",
                "table",
                "then",
                "transaction",
                "trigger",
                "true",
                "truncate",
                "union",
                "unique",
                "update",
                "using",
                "values",
                "view",
                "when",
                "where",
                "window",
                "with",
            ],
        },
        Lang::Yaml => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: true,
            block_comment: None,
            strings: &[
                DQ,
                StringSpec {
                    open: "'",
                    close: "'",
                    multiline: false,
                    escapes: false,
                },
            ],
            keywords: &["false", "no", "null", "off", "on", "true", "yes"],
        },
        Lang::Html => &LangSpec {
            line_comments: &[],
            comment_needs_boundary: false,
            block_comment: Some(("<!--", "-->")),
            strings: &[DQ, SQ],
            keywords: &[
                "a", "article", "aside", "audio", "body", "button", "canvas", "code", "col",
                "data", "datalist", "details", "dialog", "div", "em", "fieldset", "figure",
                "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "head", "header", "html",
                "iframe", "img", "input", "label", "legend", "li", "link", "main", "meta", "nav",
                "ol", "option", "picture", "pre", "script", "section", "select", "slot", "small",
                "source", "span", "strong", "style", "summary", "table", "tbody", "td", "template",
                "textarea", "tfoot", "th", "thead", "time", "title", "tr", "track", "ul", "video",
            ],
        },
        Lang::Css => &LangSpec {
            line_comments: &[],
            comment_needs_boundary: false,
            block_comment: Some(("/*", "*/")),
            strings: &[DQ, SQ],
            keywords: &[
                "align-content",
                "align-items",
                "align-self",
                "all",
                "animation",
                "appearance",
                "aspect-ratio",
                "auto",
                "background",
                "background-color",
                "block",
                "border",
                "border-color",
                "border-radius",
                "bottom",
                "box-shadow",
                "box-sizing",
                "color",
                "column",
                "content",
                "cursor",
                "display",
                "filter",
                "flex",
                "flex-basis",
                "flex-direction",
                "flex-grow",
                "flex-shrink",
                "flex-wrap",
                "font",
                "font-family",
                "font-size",
                "font-style",
                "font-weight",
                "gap",
                "grid",
                "grid-area",
                "grid-column",
                "grid-row",
                "grid-template",
                "height",
                "hidden",
                "inherit",
                "initial",
                "inline",
                "inline-block",
                "inset",
                "justify-content",
                "left",
                "letter-spacing",
                "line-height",
                "list-style",
                "margin",
                "max-height",
                "max-width",
                "media",
                "min-height",
                "min-width",
                "none",
                "object-fit",
                "opacity",
                "order",
                "outline",
                "overflow",
                "padding",
                "pointer-events",
                "position",
                "relative",
                "repeat",
                "right",
                "rotate",
                "scale",
                "sticky",
                "text-align",
                "text-decoration",
                "text-overflow",
                "text-transform",
                "top",
                "transform",
                "transition",
                "translate",
                "transparent",
                "unset",
                "user-select",
                "vertical-align",
                "visibility",
                "white-space",
                "width",
                "word-break",
                "z-index",
            ],
        },
        Lang::Dockerfile => &LangSpec {
            line_comments: &["#"],
            comment_needs_boundary: true,
            block_comment: None,
            strings: &[DQ, SQ],
            keywords: &[
                "add",
                "arg",
                "cmd",
                "copy",
                "entrypoint",
                "env",
                "expose",
                "from",
                "healthcheck",
                "label",
                "maintainer",
                "onbuild",
                "run",
                "shell",
                "stopsignal",
                "user",
                "volume",
                "workdir",
            ],
        },
        Lang::Markdown => &LangSpec {
            line_comments: &[],
            comment_needs_boundary: false,
            block_comment: None,
            strings: &[StringSpec {
                open: "`",
                close: "`",
                multiline: false,
                escapes: false,
            }],
            keywords: &[],
        },
    }
}

fn is_ident_char(lang: Lang, c: u8) -> bool {
    c.is_ascii_alphanumeric()
        || c == b'_'
        || (c == b'-' && matches!(lang, Lang::Yaml | Lang::Html | Lang::Css))
}

fn is_keyword(lang: Lang, line: &str, start: usize, end: usize, keywords: &[&str]) -> bool {
    let word = &line[start..end];
    let case_insensitive = matches!(
        lang,
        Lang::Php | Lang::Sql | Lang::Yaml | Lang::Html | Lang::Css | Lang::Dockerfile
    );
    let listed = keywords.iter().any(|keyword| {
        if case_insensitive {
            word.eq_ignore_ascii_case(keyword)
        } else {
            word == *keyword
        }
    });

    match lang {
        Lang::Html => {
            let prefix = line[..start].trim_end();
            listed && (prefix.ends_with('<') || prefix.ends_with("</") || prefix.ends_with("<!"))
        }
        Lang::Yaml => listed || line[end..].trim_start().starts_with(':'),
        Lang::Dockerfile => listed && line[..start].trim().is_empty(),
        _ => listed,
    }
}

/// Tokenize one line given the carry from the previous line. Stateless except
/// for the returned carry.
pub fn tokenize_line(lang: Lang, line: &str, carry: LineCarry) -> (Vec<Token>, LineCarry) {
    let spec = spec(lang);
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();

    // Markdown gets a bespoke, ultra-shallow pass: heading lines highlight whole.
    if lang == Lang::Markdown && line.trim_start().starts_with('#') {
        if !line.is_empty() {
            tokens.push(Token {
                range: 0..line.len(),
                class: TokenClass::Keyword,
            });
        }
        return (tokens, LineCarry::None);
    }

    let mut i = 0usize;

    // Resume a multi-line construct.
    match carry {
        LineCarry::BlockComment => {
            let close = spec.block_comment.map(|(_, c)| c).unwrap_or("*/");
            match line.find(close) {
                Some(at) => {
                    let end = at + close.len();
                    tokens.push(Token {
                        range: 0..end,
                        class: TokenClass::Comment,
                    });
                    i = end;
                }
                None => {
                    if !line.is_empty() {
                        tokens.push(Token {
                            range: 0..line.len(),
                            class: TokenClass::Comment,
                        });
                    }
                    return (tokens, LineCarry::BlockComment);
                }
            }
        }
        LineCarry::InString(spec_ix) => {
            let Some(string) = spec.strings.get(spec_ix as usize) else {
                return (tokens, LineCarry::None);
            };
            match find_string_close(line, 0, string) {
                Some(end) => {
                    tokens.push(Token {
                        range: 0..end,
                        class: TokenClass::StringLit,
                    });
                    i = end;
                }
                None => {
                    if !line.is_empty() {
                        tokens.push(Token {
                            range: 0..line.len(),
                            class: TokenClass::StringLit,
                        });
                    }
                    return (tokens, LineCarry::InString(spec_ix));
                }
            }
        }
        LineCarry::None => {}
    }

    while i < bytes.len() {
        let rest = &line[i..];

        // Block comments are checked before line comments because Lua's
        // multi-line opener (`--[[`) extends its line-comment prefix (`--`).
        if let Some((open, close)) = spec.block_comment
            && rest.starts_with(open)
        {
            match line[i + open.len()..].find(close) {
                Some(at) => {
                    let end = i + open.len() + at + close.len();
                    tokens.push(Token {
                        range: i..end,
                        class: TokenClass::Comment,
                    });
                    i = end;
                    continue;
                }
                None => {
                    tokens.push(Token {
                        range: i..line.len(),
                        class: TokenClass::Comment,
                    });
                    return (tokens, LineCarry::BlockComment);
                }
            }
        }

        // Line comment.
        if spec.line_comments.iter().any(|p| rest.starts_with(p)) {
            let boundary_ok =
                !spec.comment_needs_boundary || i == 0 || bytes[i - 1].is_ascii_whitespace();
            if boundary_ok {
                tokens.push(Token {
                    range: i..line.len(),
                    class: TokenClass::Comment,
                });
                return (tokens, LineCarry::None);
            }
        }

        // Strings (specs ordered longest-delimiter-first per language).
        if let Some((spec_ix, string)) = spec
            .strings
            .iter()
            .enumerate()
            .find(|(_, s)| rest.starts_with(s.open))
        {
            match find_string_close(line, i + string.open.len(), string) {
                Some(end) => {
                    tokens.push(Token {
                        range: i..end,
                        class: TokenClass::StringLit,
                    });
                    i = end;
                    continue;
                }
                None => {
                    tokens.push(Token {
                        range: i..line.len(),
                        class: TokenClass::StringLit,
                    });
                    let carry = if string.multiline {
                        LineCarry::InString(spec_ix as u8)
                    } else {
                        LineCarry::None
                    };
                    return (tokens, carry);
                }
            }
        }

        let c = bytes[i];

        // Number (not inside an identifier).
        if c.is_ascii_digit() && (i == 0 || !is_ident_char(lang, bytes[i - 1])) {
            let mut end = i + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
            {
                end += 1;
            }
            tokens.push(Token {
                range: i..end,
                class: TokenClass::Number,
            });
            i = end;
            continue;
        }

        // Identifier / keyword.
        if c.is_ascii_alphabetic() || c == b'_' {
            let mut end = i + 1;
            while end < bytes.len() && is_ident_char(lang, bytes[end]) {
                end += 1;
            }
            if is_keyword(lang, line, i, end, spec.keywords) {
                tokens.push(Token {
                    range: i..end,
                    class: TokenClass::Keyword,
                });
            }
            i = end;
            continue;
        }

        // Skip one char (respect UTF-8 boundaries).
        i += 1;
        while i < bytes.len() && !line.is_char_boundary(i) {
            i += 1;
        }
    }

    (tokens, LineCarry::None)
}

/// Find the end (exclusive, including the delimiter) of a string opened before
/// `from`, honoring backslash escapes when the spec wants them.
fn find_string_close(line: &str, from: usize, string: &StringSpec) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        if string.escapes && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if line[i..].starts_with(string.close) {
            return Some(i + string.close.len());
        }
        i += 1;
        while i < bytes.len() && !line.is_char_boundary(i) {
            i += 1;
        }
    }
    None
}

/// Tokenize a whole code block, threading carry across lines. One `Vec<Token>`
/// per line, in order.
pub fn tokenize_block(lang: Lang, code: &str) -> Vec<Vec<Token>> {
    let mut carry = LineCarry::None;
    code.split('\n')
        .map(|line| {
            let (tokens, next) = tokenize_line(lang, line, carry);
            carry = next;
            tokens
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(lang: Lang, line: &str) -> Vec<(String, TokenClass)> {
        tokenize_line(lang, line, LineCarry::None)
            .0
            .into_iter()
            .map(|t| (line[t.range.clone()].to_string(), t.class))
            .collect()
    }

    #[test]
    fn rust_keywords_strings_comments_numbers() {
        let toks = classes(Lang::Rust, r#"let x = 42; // the "answer""#);
        assert!(toks.contains(&("let".into(), TokenClass::Keyword)));
        assert!(toks.contains(&("42".into(), TokenClass::Number)));
        assert!(
            toks.iter()
                .any(|(t, c)| t.starts_with("//") && *c == TokenClass::Comment)
        );
        // Nothing after a line comment is tokenized separately.
        assert!(
            toks.iter()
                .filter(|(_, c)| *c == TokenClass::Comment)
                .count()
                == 1
        );
    }

    #[test]
    fn string_with_escapes_and_embedded_comment_marker() {
        let toks = classes(Lang::Rust, r#"print("a \" // not comment") // real"#);
        let strings: Vec<_> = toks
            .iter()
            .filter(|(_, c)| *c == TokenClass::StringLit)
            .collect();
        assert_eq!(strings.len(), 1);
        assert!(strings[0].0.contains("not comment"));
        assert!(
            toks.iter()
                .any(|(t, c)| t == "// real" && *c == TokenClass::Comment)
        );
    }

    #[test]
    fn block_comment_carries_across_lines() {
        let (t1, c1) = tokenize_line(Lang::Rust, "let a = 1; /* start", LineCarry::None);
        assert_eq!(c1, LineCarry::BlockComment);
        assert!(t1.iter().any(|t| t.class == TokenClass::Comment));
        let (t2, c2) = tokenize_line(Lang::Rust, "middle of comment", c1);
        assert_eq!(c2, LineCarry::BlockComment);
        assert_eq!(
            t2,
            vec![Token {
                range: 0..17,
                class: TokenClass::Comment
            }]
        );
        let (t3, c3) = tokenize_line(Lang::Rust, "end */ let b = 2;", c2);
        assert_eq!(c3, LineCarry::None);
        assert_eq!(
            t3[0],
            Token {
                range: 0..6,
                class: TokenClass::Comment
            }
        );
        assert!(t3.iter().any(|t| t.class == TokenClass::Keyword));
    }

    #[test]
    fn python_triple_string_carries() {
        let lines = tokenize_block(Lang::Python, "x = \"\"\"doc\nstill here\n\"\"\"\ny = 1");
        assert_eq!(lines.len(), 4);
        assert!(lines[1].iter().any(|t| t.class == TokenClass::StringLit));
        assert!(lines[3].iter().any(|t| t.class == TokenClass::Number));
    }

    #[test]
    fn bash_hash_needs_word_boundary() {
        let toks = classes(Lang::Bash, "echo $#ARGS # trailing");
        let comments: Vec<_> = toks
            .iter()
            .filter(|(_, c)| *c == TokenClass::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].0, "# trailing");
    }

    #[test]
    fn json_literals() {
        let toks = classes(Lang::Json, r#"{"a": true, "b": null, "n": 3.14}"#);
        assert!(toks.contains(&("true".into(), TokenClass::Keyword)));
        assert!(toks.contains(&("null".into(), TokenClass::Keyword)));
        assert!(toks.contains(&("3.14".into(), TokenClass::Number)));
        assert!(
            toks.iter()
                .any(|(t, c)| t == "\"a\"" && *c == TokenClass::StringLit)
        );
    }

    #[test]
    fn js_template_string_carries() {
        let lines = tokenize_block(Lang::Js, "const s = `multi\nline`; let n = 5");
        assert!(lines[0].iter().any(|t| t.class == TokenClass::StringLit));
        assert!(lines[1].iter().any(|t| t.class == TokenClass::StringLit));
        assert!(lines[1].iter().any(|t| t.class == TokenClass::Number));
    }

    #[test]
    fn keywords_do_not_match_inside_identifiers() {
        let toks = classes(Lang::Rust, "letter formation");
        assert!(toks.is_empty());
        let toks = classes(Lang::Go, "x1 = 2");
        // "1" is inside identifier x1 — only the standalone 2 is a number.
        assert_eq!(toks, vec![("2".to_string(), TokenClass::Number)]);
    }

    #[test]
    fn markdown_headings_highlight_whole_line() {
        let toks = classes(Lang::Markdown, "## Heading here");
        assert_eq!(toks.len(), 1);
        assert_eq!(toks[0].1, TokenClass::Keyword);
    }

    #[test]
    fn lang_tags_resolve() {
        assert_eq!(lang_for_tag("RS"), Some(Lang::Rust));
        assert_eq!(lang_for_tag("tsx"), Some(Lang::Js));
        assert_eq!(lang_for_tag("shell"), Some(Lang::Bash));
        assert_eq!(lang_for_tag("unknown-lang"), None);
    }

    #[test]
    fn common_language_tags_resolve() {
        for (tag, expected) in [
            ("c", Lang::C),
            ("h", Lang::C),
            ("c++", Lang::Cpp),
            ("hpp", Lang::Cpp),
            ("java", Lang::Java),
            ("c#", Lang::CSharp),
            ("kts", Lang::Kotlin),
            ("swift", Lang::Swift),
            ("rb", Lang::Ruby),
            ("lua", Lang::Lua),
            ("php", Lang::Php),
            ("sql", Lang::Sql),
            ("yml", Lang::Yaml),
            ("htm", Lang::Html),
            ("css", Lang::Css),
            ("Dockerfile", Lang::Dockerfile),
        ] {
            assert_eq!(lang_for_tag(tag), Some(expected), "tag {tag}");
        }
    }

    #[test]
    fn common_language_profiles_highlight_representative_syntax() {
        for (lang, line, keyword) in [
            (Lang::C, "int main(void) { return 0; }", "return"),
            (Lang::Cpp, "class Widget { public: bool ready; };", "class"),
            (Lang::Java, "public record User(String name) {}", "record"),
            (Lang::CSharp, "public record User(string Name);", "record"),
            (Lang::Kotlin, "data class User(val name: String)", "data"),
            (Lang::Swift, "struct User { let name: String }", "struct"),
            (Lang::Ruby, "class User; def name; nil; end; end", "def"),
            (Lang::Lua, "local function load() return nil end", "local"),
            (
                Lang::Php,
                "public function load(): ?string { return null; }",
                "function",
            ),
            (Lang::Sql, "SELECT * FROM users WHERE id = 42", "SELECT"),
            (Lang::Yaml, "runs-on: macos-latest", "runs-on"),
            (
                Lang::Html,
                "<section class=\"hero\">text</section>",
                "section",
            ),
            (
                Lang::Css,
                ".hero { background-color: transparent; }",
                "background-color",
            ),
            (Lang::Dockerfile, "FROM rust:1.90 AS builder", "FROM"),
        ] {
            let tokens = classes(lang, line);
            assert!(
                tokens
                    .iter()
                    .any(|(text, class)| text == keyword && *class == TokenClass::Keyword),
                "missing {keyword:?} for {lang:?}: {tokens:?}"
            );
        }
    }

    #[test]
    fn new_multiline_constructs_carry_between_lines() {
        for (lang, code) in [
            (Lang::Java, "var text = \"\"\"hello\nworld\"\"\";"),
            (Lang::Kotlin, "val text = \"\"\"hello\nworld\"\"\""),
            (Lang::Swift, "let text = \"\"\"hello\nworld\"\"\""),
            (Lang::Lua, "--[[ hello\nworld ]] local done = true"),
            (Lang::Html, "<!-- hello\nworld --> <main>done</main>"),
        ] {
            let lines = tokenize_block(lang, code);
            assert_eq!(lines.len(), 2);
            assert!(
                lines[1].iter().any(|token| matches!(
                    token.class,
                    TokenClass::StringLit | TokenClass::Comment
                )),
                "missing carried token for {lang:?}: {lines:?}"
            );
        }
    }

    #[test]
    fn contextual_keywords_avoid_obvious_false_positives() {
        assert!(classes(Lang::Html, "section text").is_empty());
        assert!(
            classes(Lang::Dockerfile, "RUN echo run")
                .iter()
                .any(|(text, class)| text == "RUN" && *class == TokenClass::Keyword)
        );
        assert!(
            !classes(Lang::Dockerfile, "RUN echo run")
                .iter()
                .any(|(text, class)| text == "run" && *class == TokenClass::Keyword)
        );
    }

    #[test]
    fn tokens_never_overlap_and_stay_in_bounds() {
        for (lang, line) in [
            (
                Lang::Rust,
                "fn f(x: &str) -> u32 { x.len() as u32 } // done",
            ),
            (Lang::Python, "def f(x): return f\"{x}\" # end"),
            (Lang::Toml, "key = \"value\" # note"),
        ] {
            let (tokens, _) = tokenize_line(lang, line, LineCarry::None);
            let mut last_end = 0;
            for t in &tokens {
                assert!(t.range.start >= last_end, "overlap in {line}");
                assert!(t.range.end <= line.len());
                assert!(line.is_char_boundary(t.range.start) && line.is_char_boundary(t.range.end));
                last_end = t.range.end;
            }
        }
    }
}

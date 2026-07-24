//! 内置文本编辑器的语言识别、语法着色与本地补全。
//!
//! 这里刻意不依赖远端语言服务：SSH/远程文件尚未保存时也能即时着色，
//! 补全候选则由语言关键字、常用内建符号和当前文档内标识符共同组成。

use std::{
    collections::BTreeMap,
    hash::{Hash as _, Hasher as _},
    sync::Arc,
};

use super::theme::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Language {
    PlainText,
    Json,
    Yaml,
    Toml,
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Shell,
    PowerShell,
    Go,
    Java,
    CLike,
    Html,
    Xml,
    Css,
    Sql,
    Markdown,
    Dockerfile,
}

impl Language {
    pub(super) fn from_path(path: &str) -> Self {
        let name = path
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .unwrap_or(path)
            .to_ascii_lowercase();
        let extension = name.rsplit_once('.').map(|(_, extension)| extension);
        match (name.as_str(), extension) {
            ("dockerfile" | "containerfile", _) => Self::Dockerfile,
            (".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".bash_login", _) => Self::Shell,
            (_, Some("json" | "jsonc")) => Self::Json,
            (_, Some("yaml" | "yml")) => Self::Yaml,
            (_, Some("toml")) => Self::Toml,
            (_, Some("rs")) => Self::Rust,
            (_, Some("py" | "pyw")) => Self::Python,
            (_, Some("js" | "jsx" | "mjs" | "cjs")) => Self::JavaScript,
            (_, Some("ts" | "tsx" | "mts" | "cts")) => Self::TypeScript,
            (_, Some("sh" | "bash" | "zsh" | "fish")) => Self::Shell,
            (_, Some("ps1" | "psm1" | "psd1")) => Self::PowerShell,
            (_, Some("go")) => Self::Go,
            (_, Some("java" | "kt" | "kts")) => Self::Java,
            (_, Some("c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "cs")) => Self::CLike,
            (_, Some("html" | "htm" | "vue" | "svelte")) => Self::Html,
            (_, Some("xml" | "svg")) => Self::Xml,
            (_, Some("css" | "scss" | "sass" | "less")) => Self::Css,
            (_, Some("sql")) => Self::Sql,
            (_, Some("md" | "markdown")) => Self::Markdown,
            _ => Self::PlainText,
        }
    }

    pub(super) fn from_path_and_text(path: &str, text: &str) -> Self {
        let detected = Self::from_path(path);
        if detected != Self::PlainText {
            return detected;
        }
        let first_line = text.lines().next().unwrap_or_default();
        if !first_line.starts_with("#!") {
            return detected;
        }
        let lower = first_line.to_ascii_lowercase();
        if lower.contains("python") {
            Self::Python
        } else if lower.contains("pwsh") || lower.contains("powershell") {
            Self::PowerShell
        } else if ["bash", "zsh", "fish", "/sh"]
            .iter()
            .any(|shell| lower.contains(shell))
        {
            Self::Shell
        } else {
            detected
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::PlainText => "Plain Text",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Shell => "Shell",
            Self::PowerShell => "PowerShell",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::CLike => "C/C++",
            Self::Html => "HTML",
            Self::Xml => "XML",
            Self::Css => "CSS",
            Self::Sql => "SQL",
            Self::Markdown => "Markdown",
            Self::Dockerfile => "Dockerfile",
        }
    }

    const fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            Self::JavaScript | Self::TypeScript => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
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
                "get",
                "if",
                "implements",
                "import",
                "in",
                "instanceof",
                "interface",
                "let",
                "new",
                "null",
                "of",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "set",
                "static",
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
                "with",
                "yield",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
                "select", "then", "time", "until", "while",
            ],
            Self::PowerShell => &[
                "begin",
                "break",
                "catch",
                "class",
                "continue",
                "data",
                "do",
                "dynamicparam",
                "else",
                "elseif",
                "end",
                "enum",
                "exit",
                "filter",
                "finally",
                "for",
                "foreach",
                "from",
                "function",
                "hidden",
                "if",
                "in",
                "param",
                "process",
                "return",
                "static",
                "switch",
                "throw",
                "trap",
                "try",
                "until",
                "using",
                "while",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "map",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "type",
                "var",
            ],
            Self::Java | Self::CLike => &[
                "abstract",
                "auto",
                "bool",
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
                "delete",
                "do",
                "double",
                "else",
                "enum",
                "extends",
                "false",
                "final",
                "finally",
                "float",
                "for",
                "friend",
                "if",
                "implements",
                "import",
                "in",
                "inline",
                "instanceof",
                "int",
                "interface",
                "long",
                "namespace",
                "new",
                "null",
                "nullptr",
                "operator",
                "package",
                "private",
                "protected",
                "public",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "super",
                "switch",
                "template",
                "this",
                "throw",
                "throws",
                "true",
                "try",
                "typedef",
                "typename",
                "union",
                "unsigned",
                "using",
                "virtual",
                "void",
                "volatile",
                "while",
            ],
            Self::Sql => &[
                "alter",
                "and",
                "as",
                "asc",
                "begin",
                "between",
                "by",
                "case",
                "commit",
                "create",
                "delete",
                "desc",
                "distinct",
                "drop",
                "else",
                "end",
                "exists",
                "from",
                "full",
                "group",
                "having",
                "in",
                "index",
                "inner",
                "insert",
                "into",
                "is",
                "join",
                "left",
                "like",
                "limit",
                "not",
                "null",
                "offset",
                "on",
                "or",
                "order",
                "outer",
                "primary",
                "references",
                "right",
                "rollback",
                "select",
                "set",
                "table",
                "then",
                "union",
                "unique",
                "update",
                "values",
                "view",
                "when",
                "where",
                "with",
            ],
            Self::Json => &["false", "null", "true"],
            Self::Yaml | Self::Toml => &["false", "null", "true"],
            Self::Dockerfile => &[
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
            Self::PlainText | Self::Html | Self::Xml | Self::Css | Self::Markdown => &[],
        }
    }

    const fn builtins(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "Box", "Err", "None", "Ok", "Option", "Result", "Self", "Some", "String", "Vec",
                "format", "println", "todo", "vec",
            ],
            Self::Python => &[
                "dict",
                "enumerate",
                "filter",
                "float",
                "int",
                "len",
                "list",
                "map",
                "open",
                "print",
                "range",
                "set",
                "str",
                "sum",
                "super",
                "tuple",
                "type",
                "zip",
            ],
            Self::JavaScript | Self::TypeScript => &[
                "Array", "Boolean", "Date", "Error", "JSON", "Map", "Math", "Number", "Object",
                "Promise", "RegExp", "Set", "String", "console", "document", "window",
            ],
            Self::Shell => &[
                "cd", "echo", "export", "printf", "pwd", "read", "set", "shift", "source", "test",
                "trap", "unset",
            ],
            Self::PowerShell => &[
                "ForEach-Object",
                "Get-ChildItem",
                "Get-Content",
                "Get-Item",
                "Select-Object",
                "Set-Content",
                "Set-Location",
                "Where-Object",
                "Write-Host",
            ],
            Self::Go => &[
                "append", "cap", "close", "complex", "copy", "delete", "len", "make", "new",
                "panic", "print", "println", "recover",
            ],
            Self::Java | Self::CLike => &[
                "ArrayList",
                "Console",
                "List",
                "Map",
                "Math",
                "String",
                "System",
                "Vector",
                "printf",
                "sizeof",
            ],
            Self::Sql => &[
                "avg", "coalesce", "count", "lower", "max", "min", "now", "round", "sum", "upper",
            ],
            _ => &[],
        }
    }

    const fn line_comment(self) -> Option<&'static str> {
        match self {
            Self::Json => Some("//"),
            Self::Python
            | Self::Shell
            | Self::PowerShell
            | Self::Yaml
            | Self::Toml
            | Self::Dockerfile => Some("#"),
            Self::Sql => Some("--"),
            Self::Rust
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::Java
            | Self::CLike => Some("//"),
            _ => None,
        }
    }

    const fn supports_block_comments(self) -> bool {
        matches!(
            self,
            Self::Rust
                | Self::JavaScript
                | Self::TypeScript
                | Self::Go
                | Self::Java
                | Self::CLike
                | Self::Json
                | Self::Css
        )
    }

    const fn property_separator(self) -> Option<char> {
        match self {
            Self::Json | Self::Yaml | Self::Css => Some(':'),
            Self::Toml => Some('='),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum TokenStyle {
    Plain,
    Comment,
    Keyword,
    String,
    Number,
    Function,
    Property,
    Type,
    Operator,
}

#[derive(Clone, Copy)]
struct SyntaxColors {
    font_size: f32,
    plain: egui::Color32,
    comment: egui::Color32,
    keyword: egui::Color32,
    string: egui::Color32,
    number: egui::Color32,
    function: egui::Color32,
    property: egui::Color32,
    ty: egui::Color32,
    operator: egui::Color32,
}

impl SyntaxColors {
    fn from_palette(pal: &Palette, font_size: f32) -> Self {
        if pal.light {
            Self {
                font_size,
                plain: pal.fg,
                comment: egui::Color32::from_rgb(0x00, 0x80, 0x00),
                keyword: egui::Color32::from_rgb(0xAF, 0x00, 0xDB),
                string: egui::Color32::from_rgb(0xA3, 0x15, 0x15),
                number: egui::Color32::from_rgb(0x09, 0x86, 0x58),
                function: egui::Color32::from_rgb(0x79, 0x5E, 0x26),
                property: egui::Color32::from_rgb(0x00, 0x10, 0x80),
                ty: egui::Color32::from_rgb(0x26, 0x7F, 0x99),
                operator: pal.fg_dim,
            }
        } else {
            Self {
                font_size,
                plain: pal.fg,
                comment: egui::Color32::from_rgb(0x6A, 0x99, 0x55),
                keyword: egui::Color32::from_rgb(0xC5, 0x86, 0xC0),
                string: egui::Color32::from_rgb(0xCE, 0x91, 0x78),
                number: egui::Color32::from_rgb(0xB5, 0xCE, 0xA8),
                function: egui::Color32::from_rgb(0xDC, 0xDC, 0xAA),
                property: egui::Color32::from_rgb(0x9C, 0xDC, 0xFE),
                ty: egui::Color32::from_rgb(0x4E, 0xC9, 0xB0),
                operator: pal.fg_dim,
            }
        }
    }

    const fn color(self, style: TokenStyle) -> egui::Color32 {
        match style {
            TokenStyle::Plain => self.plain,
            TokenStyle::Comment => self.comment,
            TokenStyle::Keyword => self.keyword,
            TokenStyle::String => self.string,
            TokenStyle::Number => self.number,
            TokenStyle::Function => self.function,
            TokenStyle::Property => self.property,
            TokenStyle::Type => self.ty,
            TokenStyle::Operator => self.operator,
        }
    }
}

pub(super) fn highlight(
    text: &str,
    language: Language,
    pal: &Palette,
    wrap_width: f32,
    font_size: f32,
) -> egui::text::LayoutJob {
    let colors = SyntaxColors::from_palette(pal, font_size);
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap_width;
    let mut in_block_comment = false;
    for segment in text.split_inclusive('\n') {
        highlight_segment(&mut job, segment, language, colors, &mut in_block_comment);
    }
    if text.is_empty() {
        append(&mut job, "", TokenStyle::Plain, colors);
    }
    job
}

fn highlight_segment(
    job: &mut egui::text::LayoutJob,
    text: &str,
    language: Language,
    colors: SyntaxColors,
    in_block_comment: &mut bool,
) {
    if language == Language::PlainText {
        append(job, text, TokenStyle::Plain, colors);
        return;
    }
    if language == Language::Markdown {
        highlight_markdown(job, text, colors, in_block_comment);
        return;
    }
    if matches!(language, Language::Html | Language::Xml) {
        highlight_markup(job, text, colors, in_block_comment);
        return;
    }

    let mut index = 0;
    while index < text.len() {
        if *in_block_comment {
            let end = text[index..]
                .find("*/")
                .map_or(text.len(), |offset| index + offset + 2);
            append(job, &text[index..end], TokenStyle::Comment, colors);
            index = end;
            *in_block_comment = index == text.len() && !text[..index].ends_with("*/");
            continue;
        }

        if language.supports_block_comments() && text[index..].starts_with("/*") {
            if let Some(offset) = text[index + 2..].find("*/") {
                let end = index + 2 + offset + 2;
                append(job, &text[index..end], TokenStyle::Comment, colors);
                index = end;
            } else {
                append(job, &text[index..], TokenStyle::Comment, colors);
                *in_block_comment = true;
                break;
            }
            continue;
        }
        if let Some(marker) = language.line_comment() {
            if text[index..].starts_with(marker) {
                append(job, &text[index..], TokenStyle::Comment, colors);
                break;
            }
        }

        let ch = text[index..].chars().next().unwrap_or_default();
        if language == Language::Rust && ch == '\'' {
            let name_start = index + ch.len_utf8();
            let Some(first) = text[name_start..].chars().next() else {
                append(job, "'", TokenStyle::Operator, colors);
                break;
            };
            if is_identifier_start(language, first) {
                let end = take_while(text, name_start, |candidate| {
                    is_identifier_continue(language, candidate)
                });
                if !text[end..].starts_with('\'') {
                    append(job, &text[index..end], TokenStyle::Type, colors);
                    index = end;
                    continue;
                }
            }
        }
        if matches!(ch, '"' | '\'' | '`') {
            let end = quoted_end(text, index, ch);
            let next = text[end..]
                .chars()
                .find(|candidate| !candidate.is_whitespace());
            let style = if language
                .property_separator()
                .is_some_and(|separator| next == Some(separator))
            {
                TokenStyle::Property
            } else {
                TokenStyle::String
            };
            append(job, &text[index..end], style, colors);
            index = end;
            continue;
        }
        if ch.is_ascii_digit() {
            let end = number_end(text, index);
            append(job, &text[index..end], TokenStyle::Number, colors);
            index = end;
            continue;
        }
        if is_identifier_start(language, ch) {
            let end = take_while(text, index, |candidate| {
                is_identifier_continue(language, candidate)
            });
            let word = &text[index..end];
            let next = text[end..]
                .chars()
                .find(|candidate| !candidate.is_whitespace());
            let style = if language
                .keywords()
                .iter()
                .any(|keyword| keyword.eq_ignore_ascii_case(word))
            {
                TokenStyle::Keyword
            } else if language
                .builtins()
                .iter()
                .any(|builtin| builtin.eq_ignore_ascii_case(word))
                || next == Some('(')
            {
                TokenStyle::Function
            } else if word.chars().next().is_some_and(char::is_uppercase) {
                TokenStyle::Type
            } else if language
                .property_separator()
                .is_some_and(|separator| next == Some(separator))
            {
                TokenStyle::Property
            } else {
                TokenStyle::Plain
            };
            append(job, word, style, colors);
            index = end;
            continue;
        }
        let end = index + ch.len_utf8();
        let style = if ch.is_ascii_punctuation() {
            TokenStyle::Operator
        } else {
            TokenStyle::Plain
        };
        append(job, &text[index..end], style, colors);
        index = end;
    }
}

fn highlight_markdown(
    job: &mut egui::text::LayoutJob,
    text: &str,
    colors: SyntaxColors,
    in_fence: &mut bool,
) {
    let trimmed = text.trim_start();
    let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
    if *in_fence {
        append(job, text, TokenStyle::String, colors);
        if is_fence {
            *in_fence = false;
        }
    } else if is_fence {
        append(job, text, TokenStyle::String, colors);
        *in_fence = true;
    } else if trimmed.starts_with('#') {
        append(job, text, TokenStyle::Keyword, colors);
    } else if trimmed.starts_with('>') {
        append(job, text, TokenStyle::Comment, colors);
    } else {
        append(job, text, TokenStyle::Plain, colors);
    }
}

fn highlight_markup(
    job: &mut egui::text::LayoutJob,
    text: &str,
    colors: SyntaxColors,
    in_comment: &mut bool,
) {
    let mut index = 0;
    while index < text.len() {
        if *in_comment {
            let end = text[index..]
                .find("-->")
                .map_or(text.len(), |offset| index + offset + 3);
            append(job, &text[index..end], TokenStyle::Comment, colors);
            *in_comment = end == text.len() && !text[..end].ends_with("-->");
            index = end;
        } else if text[index..].starts_with("<!--") {
            let end = text[index + 4..]
                .find("-->")
                .map_or(text.len(), |offset| index + 4 + offset + 3);
            append(job, &text[index..end], TokenStyle::Comment, colors);
            *in_comment = end == text.len() && !text[..end].ends_with("-->");
            index = end;
        } else if text[index..].starts_with('<') {
            let end = text[index..]
                .find('>')
                .map_or(text.len(), |offset| index + offset + 1);
            append(job, &text[index..end], TokenStyle::Keyword, colors);
            index = end;
        } else {
            let end = text[index..]
                .find('<')
                .map_or(text.len(), |offset| index + offset);
            append(job, &text[index..end], TokenStyle::Plain, colors);
            index = end;
        }
    }
}

fn append(job: &mut egui::text::LayoutJob, text: &str, style: TokenStyle, colors: SyntaxColors) {
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::monospace(colors.font_size),
            color: colors.color(style),
            ..Default::default()
        },
    );
}

fn quoted_end(text: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (offset, ch) in text[start + quote.len_utf8()..].char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == quote {
            return start + quote.len_utf8() + offset + ch.len_utf8();
        }
    }
    text.len()
}

fn take_while(text: &str, start: usize, predicate: impl Fn(char) -> bool) -> usize {
    for (offset, ch) in text[start..].char_indices() {
        if !predicate(ch) {
            return start + offset;
        }
    }
    text.len()
}

fn number_end(text: &str, start: usize) -> usize {
    let tail = &text[start..];
    if tail.starts_with("0x") || tail.starts_with("0X") {
        return take_while(text, start, |ch| {
            ch.is_ascii_hexdigit() || matches!(ch, 'x' | 'X' | '_')
        });
    }
    if tail.starts_with("0b") || tail.starts_with("0B") {
        return take_while(text, start, |ch| matches!(ch, '0' | '1' | 'b' | 'B' | '_'));
    }
    if tail.starts_with("0o") || tail.starts_with("0O") {
        return take_while(text, start, |ch| matches!(ch, '0'..='7' | 'o' | 'O' | '_'));
    }

    let mut end = start;
    let mut seen_dot = false;
    let mut seen_exponent = false;
    let mut allow_exponent_sign = false;
    for (offset, ch) in tail.char_indices() {
        let accepted = if ch.is_ascii_digit() || ch == '_' {
            allow_exponent_sign = false;
            true
        } else if ch == '.' && !seen_dot && !seen_exponent {
            seen_dot = true;
            true
        } else if matches!(ch, 'e' | 'E') && !seen_exponent {
            seen_exponent = true;
            allow_exponent_sign = true;
            true
        } else if matches!(ch, '+' | '-') && allow_exponent_sign {
            allow_exponent_sign = false;
            true
        } else {
            false
        };
        if !accepted {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    end.max(start + 1)
}

fn is_identifier_start(language: Language, ch: char) -> bool {
    ch == '_'
        || ch.is_alphabetic()
        || (ch == '$'
            && matches!(
                language,
                Language::JavaScript
                    | Language::TypeScript
                    | Language::Shell
                    | Language::PowerShell
            ))
}

fn is_identifier_continue(language: Language, ch: char) -> bool {
    is_identifier_start(language, ch)
        || ch.is_ascii_digit()
        || (ch == '-'
            && matches!(
                language,
                Language::Shell
                    | Language::PowerShell
                    | Language::Css
                    | Language::Yaml
                    | Language::Dockerfile
            ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompletionItem {
    pub label: String,
    pub kind: CompletionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CompletionKind {
    Keyword,
    Builtin,
    Document,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CompletionSet {
    pub replace_chars: std::ops::Range<usize>,
    pub items: Vec<CompletionItem>,
}

#[derive(Default)]
pub(super) struct LanguageCache {
    layout_key: Option<LayoutCacheKey>,
    layout: Option<Arc<egui::Galley>>,
    identifiers_key: Option<(u64, usize, Language)>,
    candidates: Vec<CompletionItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LayoutCacheKey {
    fingerprint: u64,
    text_len: usize,
    language: Language,
    wrap_width: u32,
    font_size: u32,
    light: bool,
    foreground: [u8; 4],
    foreground_dim: [u8; 4],
}

impl LanguageCache {
    pub(super) fn layout(
        &mut self,
        ui: &egui::Ui,
        text: &str,
        language: Language,
        pal: &Palette,
        wrap_width: f32,
    ) -> Arc<egui::Galley> {
        let fingerprint = text_fingerprint(text);
        let font_size = ui
            .style()
            .text_styles
            .get(&egui::TextStyle::Monospace)
            .map_or(13.0, |font| font.size);
        let key = LayoutCacheKey {
            fingerprint,
            text_len: text.len(),
            language,
            wrap_width: wrap_width.to_bits(),
            font_size: font_size.to_bits(),
            light: pal.light,
            foreground: pal.fg.to_array(),
            foreground_dim: pal.fg_dim.to_array(),
        };
        if self.layout_key == Some(key) {
            if let Some(layout) = &self.layout {
                return Arc::clone(layout);
            }
        }
        let job = highlight(text, language, pal, wrap_width, font_size);
        let layout = ui.fonts_mut(|fonts| fonts.layout_job(job));
        self.layout_key = Some(key);
        self.layout = Some(Arc::clone(&layout));
        layout
    }

    pub(super) fn completions(
        &mut self,
        text: &str,
        cursor_char: usize,
        language: Language,
        explicit: bool,
    ) -> Option<CompletionSet> {
        let fingerprint = text_fingerprint(text);
        let key = (fingerprint, text.len(), language);
        if self.identifiers_key != Some(key) {
            self.candidates = completion_candidates(text, language);
            self.identifiers_key = Some(key);
        }
        completions_from_candidates(text, cursor_char, language, explicit, &self.candidates)
    }
}

fn completions_from_candidates(
    text: &str,
    cursor_char: usize,
    language: Language,
    explicit: bool,
    candidates: &[CompletionItem],
) -> Option<CompletionSet> {
    let cursor_byte = char_to_byte(text, cursor_char);
    let cursor_char = text[..cursor_byte].chars().count();
    let mut start_byte = cursor_byte;
    for (byte, ch) in text[..cursor_byte].char_indices().rev() {
        if !is_identifier_continue(language, ch) {
            break;
        }
        start_byte = byte;
    }
    let prefix = &text[start_byte..cursor_byte];
    let prefix_chars = prefix.chars().count();
    let start = cursor_char.saturating_sub(prefix_chars);
    if (!explicit && prefix.chars().count() < 2)
        || prefix.chars().next().is_some_and(char::is_numeric)
    {
        return None;
    }
    let prefix_lower = prefix.to_lowercase();
    let mut ranked: Vec<(u8, &CompletionItem)> = candidates
        .iter()
        .filter_map(|candidate| {
            if candidate.label == prefix {
                return None;
            }
            let lower = candidate.label.to_lowercase();
            let score = if candidate.label.starts_with(prefix) {
                0
            } else if lower.starts_with(&prefix_lower) {
                1
            } else if explicit && lower.contains(&prefix_lower) {
                2
            } else {
                return None;
            };
            Some((score, candidate))
        })
        .collect();
    ranked.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.label.len().cmp(&right.1.label.len()))
            .then_with(|| {
                left.1
                    .label
                    .to_lowercase()
                    .cmp(&right.1.label.to_lowercase())
            })
    });
    let items: Vec<CompletionItem> = ranked
        .into_iter()
        .take(12)
        .map(|(_, item)| item.clone())
        .collect();
    (!items.is_empty()).then_some(CompletionSet {
        replace_chars: start..cursor_char,
        items,
    })
}

fn completion_candidates(text: &str, language: Language) -> Vec<CompletionItem> {
    let mut candidates = BTreeMap::<String, CompletionKind>::new();
    for keyword in language.keywords() {
        candidates.insert((*keyword).to_owned(), CompletionKind::Keyword);
    }
    for builtin in language.builtins() {
        candidates.insert((*builtin).to_owned(), CompletionKind::Builtin);
    }
    for word in identifiers(text, language) {
        candidates.entry(word).or_insert(CompletionKind::Document);
    }
    candidates
        .into_iter()
        .map(|(label, kind)| CompletionItem { label, kind })
        .collect()
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn identifiers(text: &str, language: Language) -> Vec<String> {
    let mut words = BTreeMap::<String, usize>::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match (start, is_identifier_continue(language, ch)) {
            (None, true) if is_identifier_start(language, ch) => start = Some(index),
            (Some(begin), false) => {
                add_identifier(&mut words, &text[begin..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        add_identifier(&mut words, &text[begin..]);
    }
    words.into_keys().collect()
}

fn add_identifier(words: &mut BTreeMap<String, usize>, word: &str) {
    if word.chars().count() >= 3 {
        *words.entry(word.to_owned()).or_default() += 1;
    }
}

fn text_fingerprint(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_remote_file_languages() {
        assert_eq!(Language::from_path("/etc/app/config.yaml"), Language::Yaml);
        assert_eq!(
            Language::from_path(r"C:\repo\src\main.tsx"),
            Language::TypeScript
        );
        assert_eq!(
            Language::from_path("/srv/api/Dockerfile"),
            Language::Dockerfile
        );
        assert_eq!(Language::from_path("/tmp/README"), Language::PlainText);
        assert_eq!(Language::from_path("/home/user/.bashrc"), Language::Shell);
        assert_eq!(
            Language::from_path_and_text("/usr/local/bin/tool", "#!/usr/bin/env python3\n"),
            Language::Python
        );
    }

    #[test]
    fn completion_combines_keywords_and_document_words() {
        let text = "function calculateTotal() {\n  const cal\n}\n";
        let cursor = text.find("cal\n").unwrap_or_default() + 3;
        let cursor_char = text[..cursor].chars().count();
        let candidates = completion_candidates(text, Language::JavaScript);
        let set = completions_from_candidates(
            text,
            cursor_char,
            Language::JavaScript,
            false,
            &candidates,
        )
        .unwrap();
        assert!(set.items.iter().any(|item| item.label == "calculateTotal"));
        assert_eq!(set.replace_chars.end, cursor_char);
    }

    #[test]
    fn identifier_rules_do_not_swallow_code_operators() {
        let text = "foo-bar";
        let cursor = text.chars().count();
        let candidates = vec![CompletionItem {
            label: "barValue".to_owned(),
            kind: CompletionKind::Document,
        }];
        let set =
            completions_from_candidates(text, cursor, Language::Rust, false, &candidates).unwrap();
        assert_eq!(set.replace_chars, 4..7);
    }

    #[test]
    fn number_scanner_stops_before_binary_operator() {
        assert_eq!(number_end("1+foo", 0), 1);
        assert_eq!(number_end("1e-3 + value", 0), 4);
        assert_eq!(number_end("0xff + value", 0), 4);
    }

    #[test]
    fn highlighter_keeps_the_original_text() {
        let source = "fn borrow<'a>(value: &'a str) {\n    println!(\"hi\"); // note\n}\n";
        let job = highlight(
            source,
            Language::Rust,
            &super::super::theme::DARK,
            640.0,
            13.0,
        );
        assert_eq!(job.text, source);
        assert!(job.sections.len() > 4);
    }

    #[test]
    fn language_layout_cache_reuses_unchanged_galley() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 500.0),
            )),
            ..Default::default()
        };
        let mut cache = LanguageCache::default();
        let mut reused = false;
        let _ = ctx.run_ui(input, |ui| {
            let first = cache.layout(
                ui,
                "const answer = 42;",
                Language::Rust,
                &super::super::theme::DARK,
                500.0,
            );
            let second = cache.layout(
                ui,
                "const answer = 42;",
                Language::Rust,
                &super::super::theme::DARK,
                500.0,
            );
            reused = Arc::ptr_eq(&first, &second);
        });
        assert!(reused, "未变化文本应复用已排版的语法高亮 Galley");
    }
}

//! Syntax colours for the editor tile, in the token colours of screen 2.
//!
//! A small scanner per language instead of syntect: it covers the formats
//! agent directories hold (Rust, JavaScript, TOML, Markdown, JSON, Python,
//! shell), TOML included, which syntect's default set lacks, and it keeps
//! the colours of the mockup.

use std::ops::Range;
use std::path::Path;

use egui::text::{ByteIndex, LayoutJob, TextFormat};
use egui::{Color32, FontId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    JavaScript,
    Python,
    Toml,
    Markdown,
    Json,
    Shell,
    Plain,
}

impl Lang {
    pub fn of(path: &Path) -> Lang {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        match ext.as_str() {
            "rs" => Lang::Rust,
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => Lang::JavaScript,
            "py" | "pyw" => Lang::Python,
            "toml" => Lang::Toml,
            "md" | "markdown" => Lang::Markdown,
            "json" | "jsonl" => Lang::Json,
            "sh" | "bash" | "zsh" => Lang::Shell,
            _ if matches!(name, ".bashrc" | ".profile" | "Makefile") => Lang::Shell,
            _ if name == "Cargo.lock" => Lang::Toml,
            _ => Lang::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::JavaScript => "JavaScript",
            Lang::Python => "Python",
            Lang::Toml => "TOML",
            Lang::Markdown => "Markdown",
            Lang::Json => "JSON",
            Lang::Shell => "Shell",
            Lang::Plain => "Plain text",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Keyword,
    String,
    /// Function names, calls, keys of TOML and JSON.
    Function,
    /// Numbers and literals such as `true`.
    Number,
    Comment,
}

/// Colours of screen 2 (`.k`, `.s`, `.f`, `.n`, `.c` and the code text).
pub fn color(kind: Kind) -> Color32 {
    let rgb = match kind {
        Kind::Text => [0xC9, 0xCE, 0xD9],
        Kind::Keyword => [0xC3, 0xA6, 0xFF],
        Kind::String => [0xA8, 0xD5, 0x8A],
        Kind::Function => [0xF0, 0xD5, 0x8A],
        Kind::Number => [0xF2, 0xB4, 0x5A],
        Kind::Comment => [0x6B, 0x73, 0x85],
    };
    Color32::from_rgb(rgb[0], rgb[1], rgb[2])
}

/// The coloured layout of `text`, without wrapping.
pub fn layout_job(text: &str, lang: Lang, font: FontId, line_height: f32) -> LayoutJob {
    let mut job = LayoutJob { text: text.to_string(), ..Default::default() };
    job.wrap.max_width = f32::INFINITY;
    let format =
        |kind: Kind| TextFormat { font_id: font.clone(), color: color(kind), line_height: Some(line_height), ..Default::default() };
    let mut push = |range: Range<usize>, kind: Kind| {
        let byte_range = ByteIndex(range.start)..ByteIndex(range.end);
        job.sections.push(egui::text::LayoutSection { leading_space: 0.0, byte_range, format: format(kind) });
    };
    let mut at = 0;
    let mut any = false;
    for (range, kind) in tokens(text, lang) {
        if range.start > at {
            push(at..range.start, Kind::Text);
        }
        at = range.end;
        push(range, kind);
        any = true;
    }
    if at < text.len() || !any {
        push(at..text.len(), Kind::Text);
    }
    job
}

struct Spec {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    /// Strings may run over line ends.
    multiline_strings: bool,
    /// `"""` and `'''` strings.
    triple_quotes: bool,
    keywords: &'static [&'static str],
    literals: &'static [&'static str],
    /// The word after one of these is a function or type name.
    definers: &'static [&'static str],
}

const RUST: Spec = Spec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"'],
    multiline_strings: true,
    triple_quotes: false,
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "fn", "for", "if", "impl", "in",
        "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "type",
        "unsafe", "use", "where", "while", "yield",
    ],
    literals: &["true", "false"],
    definers: &["fn", "struct", "enum", "trait", "type", "mod"],
};

const JAVASCRIPT: Spec = Spec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: &['"', '\'', '`'],
    multiline_strings: false,
    triple_quotes: false,
    keywords: &[
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
        "export",
        "extends",
        "finally",
        "for",
        "from",
        "function",
        "if",
        "import",
        "in",
        "instanceof",
        "let",
        "new",
        "of",
        "return",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "try",
        "typeof",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ],
    literals: &["true", "false", "null", "undefined", "NaN", "Infinity"],
    definers: &["function", "class"],
};

const PYTHON: Spec = Spec {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    multiline_strings: false,
    triple_quotes: true,
    keywords: &[
        "and", "as", "assert", "async", "await", "break", "case", "class", "continue", "def", "del", "elif", "else", "except", "finally",
        "for", "from", "global", "if", "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return", "try",
        "while", "with", "yield",
    ],
    literals: &["True", "False", "None"],
    definers: &["def", "class"],
};

const SHELL: Spec = Spec {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    multiline_strings: true,
    triple_quotes: false,
    keywords: &[
        "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "function", "in", "select", "return",
        "local", "export", "readonly", "declare", "unset", "shift", "exit", "break", "continue", "source", "alias", "set",
    ],
    literals: &["true", "false"],
    definers: &["function"],
};

const TOML: Spec = Spec {
    line_comments: &["#"],
    block_comment: None,
    quotes: &['"', '\''],
    multiline_strings: false,
    triple_quotes: true,
    keywords: &[],
    literals: &["true", "false"],
    definers: &[],
};

const JSON: Spec = Spec {
    line_comments: &[],
    block_comment: None,
    quotes: &['"'],
    multiline_strings: false,
    triple_quotes: false,
    keywords: &[],
    literals: &["true", "false", "null"],
    definers: &[],
};

/// Coloured byte ranges of `text`, in order and without overlap. Plain
/// text between them is left out.
pub fn tokens(text: &str, lang: Lang) -> Vec<(Range<usize>, Kind)> {
    match lang {
        Lang::Rust => scan(text, &RUST, lang),
        Lang::JavaScript => scan(text, &JAVASCRIPT, lang),
        Lang::Python => scan(text, &PYTHON, lang),
        Lang::Shell => scan(text, &SHELL, lang),
        Lang::Toml => scan(text, &TOML, lang),
        Lang::Json => scan(text, &JSON, lang),
        Lang::Markdown => markdown(text),
        Lang::Plain => Vec::new(),
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn scan(text: &str, spec: &Spec, lang: Lang) -> Vec<(Range<usize>, Kind)> {
    let b = text.as_bytes();
    let mut out: Vec<(Range<usize>, Kind)> = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    let mut define_next = false;
    let char_at = |i: usize| text[i..].chars().next().unwrap_or('\0');
    while i < b.len() {
        let c = char_at(i);
        if c == '\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let rest = &text[i..];
        let first_on_line = line_start;
        line_start = false;
        // Comments.
        if let Some(lc) = spec.line_comments.iter().find(|lc| rest.starts_with(**lc)) {
            // A `#` inside a shell word (`a#b`, `$#`) is no comment.
            let word_char = i > 0 && !text[..i].ends_with(char::is_whitespace) && lc.starts_with('#') && lang == Lang::Shell;
            if !word_char {
                let end = rest.find('\n').map_or(text.len(), |n| i + n);
                out.push((i..end, Kind::Comment));
                i = end;
                continue;
            }
        }
        if let Some((open, close)) = spec.block_comment
            && rest.starts_with(open)
        {
            let end = rest[open.len()..].find(close).map_or(text.len(), |n| i + open.len() + n + close.len());
            out.push((i..end, Kind::Comment));
            i = end;
            continue;
        }
        // TOML tables at the start of a line.
        if lang == Lang::Toml && first_on_line && c == '[' {
            let end = rest.find('\n').map_or(text.len(), |n| i + n);
            let close = rest[..end - i].rfind(']').map_or(end, |n| i + n + 1);
            out.push((i..close, Kind::Keyword));
            i = close;
            continue;
        }
        // Rust raw strings, r"..." and r#"..."#.
        if lang == Lang::Rust && (rest.starts_with("r\"") || rest.starts_with("r#")) {
            let hashes = rest[1..].bytes().take_while(|h| *h == b'#').count();
            if rest[1 + hashes..].starts_with('"') {
                let close = format!("\"{}", "#".repeat(hashes));
                let body = i + 2 + hashes;
                let end = text[body..].find(&close).map_or(text.len(), |n| body + n + close.len());
                out.push((i..end, Kind::String));
                i = end;
                continue;
            }
        }
        // Rust character literals. A lifetime stays plain text.
        if lang == Lang::Rust && c == '\'' {
            let lit = if let Some(escaped) = rest.strip_prefix("'\\") {
                escaped.find('\'').filter(|n| *n <= 8).map(|n| n + 3)
            } else {
                let mut chars = rest[1..].chars();
                match (chars.next(), chars.next()) {
                    (Some(ch), Some('\'')) => Some(1 + ch.len_utf8() + 1),
                    _ => None,
                }
            };
            if let Some(len) = lit {
                out.push((i..i + len, Kind::String));
                i += len;
            } else {
                i += 1;
            }
            continue;
        }
        // Strings.
        if spec.quotes.contains(&c) {
            let triple = spec.triple_quotes && (rest.starts_with("\"\"\"") || rest.starts_with("'''"));
            let end = if triple {
                let close = &rest[..3];
                rest[3..].find(close).map_or(text.len(), |n| i + 3 + n + 3)
            } else {
                string_end(text, i, c, spec.multiline_strings || c == '`', lang != Lang::Shell || c == '"')
            };
            let kind = if (lang == Lang::Json || lang == Lang::Toml) && is_key(text, end) { Kind::Function } else { Kind::String };
            out.push((i..end, kind));
            i = end;
            continue;
        }
        // Numbers.
        if c.is_ascii_digit() {
            let end = i + rest.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')).unwrap_or(rest.len());
            out.push((i..end, Kind::Number));
            i = end;
            continue;
        }
        // Shell variables.
        if lang == Lang::Shell && c == '$' {
            let after = &rest[1..];
            let len = if after.starts_with('{') {
                after.find('}').map_or(after.len(), |n| n + 1)
            } else {
                after.find(|ch: char| !is_ident(ch)).unwrap_or(after.len()).max(after.chars().next().map_or(0, |ch| ch.len_utf8()))
            };
            out.push((i..i + 1 + len, Kind::Number));
            i += 1 + len;
            continue;
        }
        // Python decorators.
        if lang == Lang::Python && c == '@' && first_on_line {
            let end = i + 1 + rest[1..].find(|ch: char| !(is_ident(ch) || ch == '.')).unwrap_or(rest.len() - 1);
            out.push((i..end, Kind::Function));
            i = end;
            continue;
        }
        // Words.
        if is_ident_start(c) {
            let len = rest.find(|ch: char| !is_ident(ch) && !(lang == Lang::Toml && ch == '-')).unwrap_or(rest.len());
            let word = &rest[..len];
            let end = i + len;
            let next = text[end..].trim_start_matches([' ', '\t']).chars().next();
            let kind = if define_next {
                define_next = false;
                Some(Kind::Function)
            } else if spec.keywords.contains(&word) {
                define_next = spec.definers.contains(&word);
                Some(Kind::Keyword)
            } else if spec.literals.contains(&word) {
                Some(Kind::Number)
            } else if (lang == Lang::Toml && first_on_line && next.is_some_and(|n| n == '=' || n == '.'))
                || (lang == Lang::Rust && next == Some('!'))
                || (next == Some('(') && lang != Lang::Toml && lang != Lang::Json)
            {
                // TOML keys, Rust macros and calls.
                Some(Kind::Function)
            } else {
                None
            };
            if let Some(kind) = kind {
                out.push((i..end, kind));
            }
            i = end;
            continue;
        }
        i += c.len_utf8();
    }
    out
}

/// End of the string that starts with `quote` at `start`.
fn string_end(text: &str, start: usize, quote: char, multiline: bool, escapes: bool) -> usize {
    let mut chars = text[start + quote.len_utf8()..].char_indices();
    while let Some((n, ch)) = chars.next() {
        let at = start + quote.len_utf8() + n;
        match ch {
            '\\' if escapes => {
                chars.next();
            }
            '\n' if !multiline => return at,
            ch if ch == quote => return at + ch.len_utf8(),
            _ => {}
        }
    }
    text.len()
}

/// A JSON or TOML string at `end` is a key when `:` or `=` follows.
fn is_key(text: &str, end: usize) -> bool {
    matches!(text[end..].trim_start_matches([' ', '\t']).chars().next(), Some(':') | Some('='))
}

fn markdown(text: &str) -> Vec<(Range<usize>, Kind)> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut fence = false;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();
        let range = at..at + body.len();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            out.push((range, Kind::Comment));
        } else if fence {
            out.push((range, Kind::String));
        } else if trimmed.starts_with('#') {
            out.push((range, Kind::Keyword));
        } else if trimmed.starts_with('>') {
            out.push((range, Kind::Comment));
        } else {
            let mut from = 0;
            for marker in ["- ", "* ", "+ "] {
                if trimmed.starts_with(marker) {
                    out.push((at + indent..at + indent + 1, Kind::Number));
                    from = indent + 1;
                }
            }
            if from == 0 {
                let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
                if digits > 0 && trimmed[digits..].starts_with(". ") {
                    out.push((at + indent..at + indent + digits + 1, Kind::Number));
                    from = indent + digits + 1;
                }
            }
            // Inline code spans.
            let mut rest = &body[from..];
            let mut offset = at + from;
            while let Some(open) = rest.find('`') {
                let Some(close) = rest[open + 1..].find('`') else { break };
                let end = open + 1 + close + 1;
                out.push((offset + open..offset + end, Kind::String));
                offset += end;
                rest = &rest[end..];
            }
        }
        at += line.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str, lang: Lang) -> Vec<(&str, Kind)> {
        tokens(text, lang).into_iter().map(|(r, k)| (&text[r], k)).collect()
    }

    #[test]
    fn python_like_the_mockup() {
        use Kind::*;
        assert_eq!(
            kinds("def count_timeouts(results):\n    return sum(r == \"timeout\")  # n\n", Lang::Python),
            vec![
                ("def", Keyword),
                ("count_timeouts", Function),
                ("return", Keyword),
                ("sum", Function),
                ("\"timeout\"", String),
                ("# n", Comment)
            ]
        );
        assert_eq!(kinds("x = '''a\nb'''\n@dataclass\n", Lang::Python), vec![("'''a\nb'''", String), ("@dataclass", Function)]);
    }

    #[test]
    fn rust_strings_lifetimes_and_macros() {
        use Kind::*;
        assert_eq!(
            kinds("fn f<'a>(x: &'a str) -> char { println!(\"{x}\"); 'c' } // done", Lang::Rust),
            vec![("fn", Keyword), ("f", Function), ("println", Function), ("\"{x}\"", String), ("'c'", String), ("// done", Comment)]
        );
        assert_eq!(
            kinds("let s = r#\"a \"q\" b\"#; /* c\nd */", Lang::Rust),
            vec![("let", Keyword), ("r#\"a \"q\" b\"#", String), ("/* c\nd */", Comment)]
        );
    }

    #[test]
    fn toml_json_shell_and_markdown() {
        use Kind::*;
        assert_eq!(
            kinds("[runtime]\nkind = \"mock\" # x\nquiet-hours = 5\nok = true\n", Lang::Toml),
            vec![
                ("[runtime]", Keyword),
                ("kind", Function),
                ("\"mock\"", String),
                ("# x", Comment),
                ("quiet-hours", Function),
                ("5", Number),
                ("ok", Function),
                ("true", Number)
            ]
        );
        assert_eq!(
            kinds("{\"a\": [1, \"b\", null]}", Lang::Json),
            vec![("\"a\"", Function), ("1", Number), ("\"b\"", String), ("null", Number)]
        );
        assert_eq!(
            kinds("if [ -n \"$X\" ]; then echo ${HOME} $1; fi # end\n", Lang::Shell),
            vec![
                ("if", Keyword),
                ("\"$X\"", String),
                ("then", Keyword),
                ("${HOME}", Number),
                ("$1", Number),
                ("fi", Keyword),
                ("# end", Comment)
            ]
        );
        assert_eq!(
            kinds("# Title\n- item with `code`\n```\nlet x\n```\n", Lang::Markdown),
            vec![("# Title", Keyword), ("-", Number), ("`code`", String), ("```", Comment), ("let x", String), ("```", Comment)]
        );
    }

    #[test]
    fn languages_by_extension() {
        assert_eq!(Lang::of(Path::new("src/main.rs")), Lang::Rust);
        assert_eq!(Lang::of(Path::new("figure.js")), Lang::JavaScript);
        assert_eq!(Lang::of(Path::new("agent.toml")), Lang::Toml);
        assert_eq!(Lang::of(Path::new("memory/notes.md")), Lang::Markdown);
        assert_eq!(Lang::of(Path::new("state.json")), Lang::Json);
        assert_eq!(Lang::of(Path::new("run.py")), Lang::Python);
        assert_eq!(Lang::of(Path::new("build.sh")), Lang::Shell);
        assert_eq!(Lang::of(Path::new("README")), Lang::Plain);
    }

    #[test]
    fn sections_cover_the_whole_text() {
        let text = "def f():\n    return \"x\"\n";
        let job = layout_job(text, Lang::Python, FontId::monospace(13.0), 20.0);
        let mut at = 0;
        for s in &job.sections {
            assert_eq!(s.byte_range.start.0, at);
            at = s.byte_range.end.0;
        }
        assert_eq!(at, text.len());
        assert!(layout_job("", Lang::Plain, FontId::monospace(13.0), 20.0).sections.len() == 1);
    }
}

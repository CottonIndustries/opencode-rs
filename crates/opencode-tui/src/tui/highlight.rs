//! Small lexical highlighter mapping code to the theme's syntax tokens
//! (comment, keyword, function, variable, string, number, type, operator, punctuation).

use super::theme::{Rgba, Syntax};

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Plain,
    Comment,
    Keyword,
    Function,
    Str,
    Number,
    Type,
    Operator,
    Punct,
}

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "case", "catch", "class", "const", "continue", "crate", "def", "default", "defer", "del", "do", "elif", "else", "enum", "except",
    "export", "extends", "extern", "false", "finally", "fn", "for", "from", "func", "function", "go", "if", "impl", "import", "in", "interface", "is", "lambda", "let",
    "loop", "match", "mod", "move", "mut", "new", "nil", "None", "not", "null", "of", "or", "and", "package", "pass", "pub", "raise", "ref", "return", "self", "Self",
    "static", "struct", "super", "switch", "this", "throw", "trait", "true", "True", "False", "try", "type", "typeof", "undefined", "unsafe", "use", "var", "void",
    "where", "while", "with", "yield", "then", "fi", "esac", "done", "local", "echo", "private", "protected", "public", "readonly", "declare", "namespace", "instanceof",
];

fn color(k: Kind, s: &Syntax, plain: Rgba) -> Rgba {
    match k {
        Kind::Plain => plain,
        Kind::Comment => s.comment,
        Kind::Keyword => s.keyword,
        Kind::Function => s.function,
        Kind::Str => s.string,
        Kind::Number => s.number,
        Kind::Type => s.ty,
        Kind::Operator => s.operator,
        Kind::Punct => s.punctuation,
    }
}

fn comment_prefix(lang: &str) -> &'static [&'static str] {
    match lang {
        "py" | "python" | "sh" | "bash" | "zsh" | "shell" | "toml" | "yaml" | "yml" | "ruby" | "rb" | "ps1" | "powershell" | "r" | "dockerfile" | "make" | "makefile" => &["#"],
        "sql" | "lua" | "haskell" | "hs" => &["--"],
        "json" | "markdown" | "md" | "text" | "txt" | "" => &[],
        _ => &["//"],
    }
}

/// Highlight a full source block. Tree-sitter handles multi-line strings,
/// comments and nested constructs; the lexical scanner is the fallback.
pub fn text(src: &str, lang: &str, s: &Syntax, plain: Rgba) -> Vec<Vec<(String, Rgba)>> {
    if let Some(lines) = super::ts_highlight::highlight(src, lang, s, plain) {
        return lines;
    }
    src.lines().map(|source| self::line(source, lang, s, plain)).collect()
}

/// Highlight one line of `lang` source into (text, color) runs.
pub fn line(src: &str, lang: &str, s: &Syntax, plain: Rgba) -> Vec<(String, Rgba)> {
    if let Some(runs) = super::ts_highlight::line(src, lang, s, plain) {
        return runs;
    }
    let lang = lang.to_ascii_lowercase();
    if matches!(lang.as_str(), "text" | "txt" | "plain" | "markdown" | "md" | "diff") {
        return vec![(src.to_string(), plain)];
    }
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<(String, Kind)> = Vec::new();
    let push = |out: &mut Vec<(String, Kind)>, t: String, k: Kind| match out.last_mut() {
        Some((last, lk)) if *lk == k => last.push_str(&t),
        _ => out.push((t, k)),
    };
    let comments = comment_prefix(&lang);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let rest: String = chars[i..].iter().collect();
        if comments.iter().any(|p| rest.starts_with(p)) || rest.starts_with("/*") && comments.contains(&"//") {
            push(&mut out, rest, Kind::Comment);
            break;
        }
        if c == '"' || c == '\'' || c == '`' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != c {
                if chars[j] == '\\' {
                    j += 1;
                }
                j += 1;
            }
            let end = (j + 1).min(chars.len());
            // a lone apostrophe in Rust lifetimes or prose should not swallow the line
            if c == '\'' && j >= chars.len() && lang == "rust" {
                push(&mut out, c.to_string(), Kind::Punct);
                i += 1;
                continue;
            }
            push(&mut out, chars[i..end].iter().collect(), Kind::Str);
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_') {
                j += 1;
            }
            push(&mut out, chars[i..j].iter().collect(), Kind::Number);
            i = j;
            continue;
        }
        if c.is_alphabetic() || c == '_' || c == '$' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '$') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let next = chars[j..].iter().find(|c| !c.is_whitespace());
            let kind = if KEYWORDS.contains(&word.as_str()) {
                Kind::Keyword
            } else if next == Some(&'(') || next == Some(&'!') && lang == "rust" {
                Kind::Function
            } else if word.chars().next().is_some_and(|c| c.is_uppercase()) {
                Kind::Type
            } else {
                Kind::Plain
            };
            push(&mut out, word, kind);
            i = j;
            continue;
        }
        let kind = if "+-*/%=<>!&|^~?:".contains(c) {
            Kind::Operator
        } else if "(){}[];,.".contains(c) {
            Kind::Punct
        } else {
            Kind::Plain
        };
        push(&mut out, c.to_string(), kind);
        i += 1;
    }
    out.into_iter().map(|(t, k)| (t, color(k, s, plain))).collect()
}

/// Language name from a file path, for code views.
pub fn lang_for_path(path: &str) -> String {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" => "powershell",
        "md" => "markdown",
        "txt" | "log" => "text",
        other => other,
    }
    .to_string()
}

//! Tree-sitter syntax highlighting for transcript and plugin code blocks.
//!
//! The lexical highlighter remains the fallback for unsupported languages and
//! for code that cannot be parsed. Tree-sitter provides token boundaries that
//! regex highlighting cannot, including strings with escapes, nested comments,
//! raw strings, and multi-line constructs.

use std::collections::HashMap;
use std::sync::OnceLock;

use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

use super::theme::{Rgba, Syntax};

const HIGHLIGHT_NAMES: &[&str] = &[
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constructor",
    "embedded",
    "function",
    "function.builtin",
    "keyword",
    "label",
    "module",
    "number",
    "operator",
    "property",
    "property.builtin",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "string",
    "string.escape",
    "string.special",
    "tag",
    "type",
    "type.builtin",
    "variable",
    "variable.builtin",
    "variable.parameter",
];

struct Language {
    config: HighlightConfiguration,
}

fn build(
    language: tree_sitter::Language,
    name: &str,
    highlights: &str,
) -> Option<Language> {
    let mut config = HighlightConfiguration::new(language, name, highlights, "", "").ok()?;
    config.configure(HIGHLIGHT_NAMES);
    Some(Language { config })
}

fn languages() -> &'static HashMap<&'static str, Language> {
    static LANGUAGES: OnceLock<HashMap<&'static str, Language>> = OnceLock::new();
    LANGUAGES.get_or_init(|| {
        let mut map = HashMap::new();
        let mut add = |id: &'static str, language: tree_sitter::Language, name: &str, query: &str| {
            if let Some(language) = build(language, name, query) {
                map.insert(id, language);
            }
        };
        add(
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
        );
        add(
            "javascript",
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
        );
        add(
            "typescript",
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        );
        add(
            "tsx",
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tsx",
            tree_sitter_typescript::HIGHLIGHTS_QUERY,
        );
        add(
            "python",
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
        );
        add(
            "json",
            tree_sitter_json::LANGUAGE.into(),
            "json",
            tree_sitter_json::HIGHLIGHTS_QUERY,
        );
        add(
            "bash",
            tree_sitter_bash::LANGUAGE.into(),
            "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY,
        );
        add(
            "go",
            tree_sitter_go::LANGUAGE.into(),
            "go",
            tree_sitter_go::HIGHLIGHTS_QUERY,
        );
        add(
            "html",
            tree_sitter_html::LANGUAGE.into(),
            "html",
            tree_sitter_html::HIGHLIGHTS_QUERY,
        );
        add(
            "css",
            tree_sitter_css::LANGUAGE.into(),
            "css",
            tree_sitter_css::HIGHLIGHTS_QUERY,
        );
        map
    })
}

fn language_id(lang: &str) -> &'static str {
    match lang.to_ascii_lowercase().as_str() {
        "rs" | "rust" => "rust",
        "js" | "jsx" | "javascript" | "mjs" | "cjs" => "javascript",
        "ts" | "typescript" => "typescript",
        "tsx" => "tsx",
        "py" | "python" => "python",
        "json" | "jsonc" => "json",
        "sh" | "bash" | "zsh" | "shell" => "bash",
        "go" | "golang" => "go",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        _ => "",
    }
}

fn style(name: &str, syntax: &Syntax, plain: Rgba) -> Rgba {
    if name.starts_with("comment") {
        syntax.comment
    } else if name.starts_with("keyword") {
        syntax.keyword
    } else if name.starts_with("string") {
        syntax.string
    } else if name.starts_with("number") || name.starts_with("constant") {
        syntax.number
    } else if name.starts_with("function") || name.starts_with("constructor") {
        syntax.function
    } else if name.starts_with("type") {
        syntax.ty
    } else if name.starts_with("operator") {
        syntax.operator
    } else if name.starts_with("punctuation") {
        syntax.punctuation
    } else if name.starts_with("tag") || name.starts_with("attribute") || name.starts_with("property") {
        syntax.function
    } else {
        plain
    }
}

fn current(stack: &[Highlight], names: &[&str], syntax: &Syntax, plain: Rgba) -> Rgba {
    stack
        .last()
        .and_then(|highlight| names.get(highlight.0))
        .map(|name| style(name, syntax, plain))
        .unwrap_or(plain)
}

/// Highlight `source` into styled runs per line. Returns `None` when the
/// language is unsupported or parsing fails, so callers can use the lexical
/// highlighter instead.
pub fn highlight(
    source: &str,
    lang: &str,
    syntax: &Syntax,
    plain: Rgba,
) -> Option<Vec<Vec<(String, Rgba)>>> {
    let id = language_id(lang);
    if id.is_empty() {
        return None;
    }
    let language = languages().get(id)?;
    let mut highlighter = Highlighter::new();
    let events = highlighter
        .highlight(&language.config, source.as_bytes(), None, None, |_| None)
        .ok()?;
    let mut lines: Vec<Vec<(String, Rgba)>> = vec![Vec::new()];
    let mut stack: Vec<Highlight> = Vec::new();
    for event in events {
        match event.ok()? {
            HighlightEvent::Source { start, end } => {
                let text = source.get(start..end)?;
                let color = current(&stack, HIGHLIGHT_NAMES, syntax, plain);
                for (index, part) in text.split('\n').enumerate() {
                    if index > 0 {
                        lines.push(Vec::new());
                    }
                    if part.is_empty() {
                        continue;
                    }
                    let line = lines.last_mut()?;
                    if let Some((last, last_color)) = line.last_mut() {
                        if *last_color == color {
                            last.push_str(part);
                            continue;
                        }
                    }
                    line.push((part.to_string(), color));
                }
            }
            HighlightEvent::HighlightStart(highlight) => stack.push(highlight),
            HighlightEvent::HighlightEnd => {
                stack.pop();
            }
        }
    }
    Some(lines)
}

/// Highlight `source` as one line, for callers that render a single line.
pub fn line(source: &str, lang: &str, syntax: &Syntax, plain: Rgba) -> Option<Vec<(String, Rgba)>> {
    let lines = highlight(source, lang, syntax, plain)?;
    Some(lines.into_iter().next().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn syntax() -> Syntax {
        Syntax {
            comment: Rgba::from_ints(1, 2, 3, 255),
            keyword: Rgba::from_ints(4, 5, 6, 255),
            function: Rgba::from_ints(7, 8, 9, 255),
            variable: Rgba::from_ints(10, 11, 12, 255),
            string: Rgba::from_ints(13, 14, 15, 255),
            number: Rgba::from_ints(16, 17, 18, 255),
            ty: Rgba::from_ints(19, 20, 21, 255),
            operator: Rgba::from_ints(22, 23, 24, 255),
            punctuation: Rgba::from_ints(25, 26, 27, 255),
        }
    }

    #[test]
    fn rust_keywords_and_strings_are_colored() {
        let syntax = syntax();
        let lines = highlight("fn main() { let s = \"x\"; }", "rust", &syntax, Rgba::BLACK).unwrap();
        let colors: Vec<Rgba> = lines[0].iter().map(|(_, color)| *color).collect();
        assert!(colors.contains(&syntax.keyword));
        assert!(colors.contains(&syntax.string));
    }

    #[test]
    fn unsupported_language_returns_none() {
        assert!(highlight("x", "brainfuck", &syntax(), Rgba::BLACK).is_none());
    }
}

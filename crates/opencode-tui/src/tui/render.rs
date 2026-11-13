//! One-line tool summaries, used by `opencode run` output.

use serde_json::Value;

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// One-line summary of a tool call's input.
pub fn tool_summary(name: &str, input: &Value) -> String {
    let path = || {
        let p = s(input, "path");
        if p.is_empty() {
            s(input, "filePath")
        } else {
            p
        }
    };
    match name {
        "shell" => {
            let c = s(input, "command");
            let mut out = crate::util::first_line(&c).to_string();
            if c.lines().count() > 1 {
                out.push_str(" …");
            }
            if input.get("background").and_then(|b| b.as_bool()) == Some(true) {
                out.push_str("  (background)");
            }
            out
        }
        "read" | "write" | "edit" => {
            let mut p = path();
            if let Some(o) = input.get("offset").and_then(|x| x.as_u64()) {
                p.push_str(&format!(" [offset={o}]"));
            }
            p
        }
        "glob" => format!("{}{}", s(input, "pattern"), if s(input, "path").is_empty() { String::new() } else { format!(" in {}", s(input, "path")) }),
        "grep" => format!("\"{}\"{}", s(input, "pattern"), if s(input, "include").is_empty() { String::new() } else { format!(" ({})", s(input, "include")) }),
        "webfetch" => s(input, "url"),
        "websearch" => s(input, "query"),
        "subagent" => format!("@{} {}", s(input, "agent"), s(input, "description")),
        "skill" => s(input, "id"),
        "todowrite" => format!("{} todos", input.get("todos").and_then(|t| t.as_array()).map(|a| a.len()).unwrap_or(0)),
        "patch" => {
            let t = s(input, "patchText");
            let files: Vec<&str> = t
                .lines()
                .filter_map(|l| l.strip_prefix("*** Add File: ").or_else(|| l.strip_prefix("*** Update File: ")).or_else(|| l.strip_prefix("*** Delete File: ")))
                .collect();
            files.join(", ")
        }
        _ => crate::util::truncate_chars(&input.to_string(), 120),
    }
}

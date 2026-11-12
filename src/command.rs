//! Slash commands: built-ins (`/init`, `/review`), config `command`, and `command/*.md` files.

use crate::config::Config;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub template: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    /// MCP prompt backing this command, with its declared argument names.
    pub mcp_prompt: Option<(String, String, Vec<String>)>,
}

pub fn load(config: &Config, dirs: &[PathBuf]) -> BTreeMap<String, Command> {
    let mut out = BTreeMap::new();
    out.insert(
        "init".into(),
        Command { name: "init".into(), description: "guided AGENTS.md setup".into(), template: crate::prompt::INITIALIZE.into(), agent: None, model: None, mcp_prompt: None },
    );
    out.insert(
        "review".into(),
        Command {
            name: "review".into(),
            description: "review changes [commit|branch|pr], defaults to uncommitted".into(),
            template: crate::prompt::REVIEW.into(),
            agent: None,
            model: None,
            mcp_prompt: None,
        },
    );
    for dir in dirs {
        for sub in ["command", "commands"] {
            let Ok(rd) = std::fs::read_dir(dir.join(sub)) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else { continue };
                let (fm, body) = crate::config::frontmatter(&text);
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                let s = |k: &str| fm.get(k).and_then(|v| v.as_str()).map(String::from);
                out.insert(
                    name.clone(),
                    Command {
                        name,
                        description: s("description").unwrap_or_default(),
                        template: body.trim().to_string(),
                        agent: s("agent"),
                        model: s("model"),
                        mcp_prompt: None,
                    },
                );
            }
        }
    }
    for (name, c) in &config.command {
        out.insert(
            name.clone(),
            Command {
                name: name.clone(),
                description: c.description.clone().unwrap_or_default(),
                template: c.template.clone(),
                agent: c.agent.clone(),
                model: c.model.clone(),
                mcp_prompt: None,
            },
        );
    }
    out
}

fn shell_output(cmd: &str, cwd: &Path) -> String {
    let shell = crate::tools::shell::detect();
    match std::process::Command::new(&shell.program).args(&shell.args).arg(cmd).current_dir(cwd).output() {
        Ok(o) => format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim_end().to_string(),
        Err(e) => format!("(failed to run `{cmd}`: {e})"),
    }
}

/// Expand an MCP prompt command into the text sent as the prompt.
pub async fn render_mcp(app: &crate::app::App, cmd: &Command, args: &str) -> String {
    let Some((server, prompt, arguments)) = &cmd.mcp_prompt else {
        return render(cmd, args, &app.directory, &app.worktree);
    };
    let tokens = regex::Regex::new(r#"(?:\[Image\s+\d+\]|"[^"]*"|'[^']*'|[^\s"']+)"#)
        .unwrap()
        .find_iter(args)
        .map(|token| token.as_str().trim_matches(['"', '\'']).to_string())
        .collect::<Vec<_>>();
    let values = arguments
        .iter()
        .enumerate()
        .map(|(index, name)| (name.clone(), serde_json::json!(tokens.get(index).cloned().unwrap_or_default())))
        .collect::<serde_json::Map<_, _>>();
    let result = match app.mcp.prompt(server, prompt, serde_json::Value::Object(values)).await {
        Ok(Some(result)) => result,
        Ok(None) => return format!("MCP prompt not found: {server}:{prompt}"),
        Err(error) => return format!("MCP prompt {server}:{prompt} failed: {error:#}"),
    };
    result
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(|message| message.get("content"))
                .map(|content| match content {
                    serde_json::Value::String(text) => text.clone(),
                    value if value.get("type").and_then(serde_json::Value::as_str) == Some("text") => {
                        value.get("text").and_then(serde_json::Value::as_str).unwrap_or_default().to_string()
                    }
                    _ => String::new(),
                })
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

fn sanitize(value: &str) -> String {
    value.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// `server:prompt`, as MCP prompt commands are addressed.
pub fn mcp_command_name(server: &str, prompt: &str) -> String {
    format!("{}:{}", sanitize(server), sanitize(prompt))
}

/// Expand `$ARGUMENTS`, `$1..$9`, `${path}` and `` !`cmd` `` in a command template.
pub fn render(cmd: &Command, args: &str, directory: &Path, worktree: &Path) -> String {
    let mut t = cmd.template.clone();
    let positional: Vec<&str> = args.split_whitespace().collect();
    for i in (1..=9).rev() {
        t = t.replace(&format!("${i}"), positional.get(i - 1).copied().unwrap_or(""));
    }
    let had_args = t.contains("$ARGUMENTS");
    t = t.replace("$ARGUMENTS", args);
    t = t.replace("${path}", &worktree.join("AGENTS.md").display().to_string());
    let re = regex::Regex::new(r"!`([^`]+)`").unwrap();
    t = re.replace_all(&t, |c: &regex::Captures| shell_output(&c[1], directory)).into_owned();
    if !had_args && !args.trim().is_empty() {
        t.push_str("\n\n");
        t.push_str(args);
    }
    t
}

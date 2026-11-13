use super::*;
use crate::session::ToolState;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn dump(t: &Terminal<TestBackend>) -> String {
    let buf = t.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn rows(screen: &str) -> Vec<Vec<char>> {
    screen.lines().map(|l| l.chars().collect()).collect()
}

async fn ui() -> Ui {
    let dir = std::env::temp_dir().join("opencode-tui-test");
    std::fs::create_dir_all(&dir).unwrap();
    let app = App::init(dir).await.unwrap();
    let opts = StartOptions { model: Some("anthropic/claude-sonnet-5-5".into()), agent: None, continue_last: false, session: None, fork: false, prompt: None };
    let mut ui = Ui::new(app, &opts).unwrap();
    ui.theme = theme::fallback(Mode::Dark);
    ui
}

/// Positions measured from the TypeScript client at 120x40.
#[tokio::test]
async fn home_matches_upstream_layout() {
    let mut ui = ui().await;
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    println!("{screen}");
    let r = rows(&screen);
    assert_eq!(r[14][74], '▄', "logo top row");
    assert_eq!(r[15][41], '█', "logo left edge");
    assert_eq!(r[20][23], '┃', "prompt stripe");
    assert!(r[21].iter().collect::<String>().contains("Ask anything…"));
    assert_eq!(r[24][23], '╹');
    assert!(r[25].iter().collect::<String>().contains("ctrl+p commands"));
}

#[tokio::test]
async fn tabs_close_to_previous_and_reopen_in_place() {
    let mut ui = ui().await;
    ui.tabs.clear();
    let ids: Vec<String> = (0..3)
        .map(|_| {
            let h = ui.app.new_session(None);
            h.save();
            h.id()
        })
        .collect();
    for id in &ids {
        ui.open_session(id);
    }
    // visit order a, b, c, then back to a: closing a returns to c (last visited), not its neighbour b
    ui.open_session(&ids[0]);
    ui.exec("session.tab.close", "");
    assert_eq!(ui.tabs, [ids[1].clone(), ids[2].clone()]);
    assert_eq!(ui.sid(), ids[2]);
    ui.exec("session.tab.reopen", "");
    assert_eq!(ui.tabs, ids);
    assert_eq!(ui.sid(), ids[0]);
    ui.exec("session.tab.next", "");
    assert_eq!(ui.sid(), ids[1]);
    ui.exec("session.tab.select.3", "");
    assert_eq!(ui.sid(), ids[2]);
    // closing the last tab goes home
    for _ in 0..3 {
        ui.exec("session.tab.close", "");
    }
    assert!(ui.tabs.is_empty());
    assert!(ui.route == Route::Home);
    for id in &ids {
        let _ = crate::session::delete(&ui.app.project, id);
    }
}

#[tokio::test]
async fn draws_session_and_dialogs() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    ui.handle.with(|s| {
        s.title = "Fix notes file".into();
        let mut u = Message::user_text("Please fix notes.txt and explain");
        u.model = Some("anthropic/claude-sonnet-5-5".into());
        s.messages.push(u);
        let mut a = Message::new(Role::Assistant);
        a.agent = Some("build".into());
        a.model = Some("anthropic/claude-sonnet-5-5".into());
        a.parts.push(Part::Reasoning { text: "I should read the file first.".into(), signature: None, redacted: None, item_id: None, started: Some(a.time), ended: Some(a.time + 591) });
        a.parts.push(Part::Tool { id: "0".into(), name: "read".into(), input: serde_json::json!({ "path": "notes.txt" }), state: ToolState::Completed { output: "hello".into(), title: String::new(), metadata: serde_json::json!({}), attachments: vec![] }, provider_meta: None });
        a.parts.push(Part::Tool { id: "1".into(), name: "shell".into(), input: serde_json::json!({ "command": "git status" }), state: ToolState::Completed { output: "On branch main\nnothing to commit".into(), title: "git status".into(), metadata: serde_json::json!({}), attachments: vec![] }, provider_meta: None });
        let diff = crate::util::unified_diff("notes.txt", "hello\nsecond line\n", "hello\nline two\n");
        a.parts.push(Part::Tool { id: "2".into(), name: "edit".into(), input: serde_json::json!({ "path": "notes.txt" }), state: ToolState::Completed { output: "Edited".into(), title: "notes.txt".into(), metadata: serde_json::json!({ "files": [{ "file": "notes.txt", "diff": diff, "additions": 1, "deletions": 1 }] }), attachments: vec![] }, provider_meta: None });
        a.parts.push(Part::Tool { id: "3".into(), name: "grep".into(), input: serde_json::json!({ "pattern": "TODO" }), state: ToolState::Error { error: "File not found: missing.txt".into() }, provider_meta: None });
        a.parts.push(Part::Text { text: "## Summary\n\nI changed **one line**:\n\n- `second line` → `line two`\n- kept the rest\n\n```rust\nfn main() {}\n```".into(), synthetic: false });
        a.finish = Some("stop".into());
        a.completed = Some(a.time + 2500);
        a.usage.output = 100;
        s.messages.push(a);
    });
    let mut t = Terminal::new(TestBackend::new(100, 60)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    println!("{screen}");
    for needle in ["Please fix notes.txt", "Thought: 591ms", "I should read the file first.", "$ git status", "nothing to commit", "← Edit notes.txt", "second line", "line two", "Summary", "- second line → line two", "fn main() {}", "Build", "Claude Sonnet"] {
        assert!(screen.contains(needle), "missing {needle:?}");
    }
    // exploration tools collapse into summary rows; the read and grep are not adjacent
    assert!(screen.contains("→ Explored: 1 read") && screen.contains("→ Explored: 1 search"), "exploration groups");

    ui.permissions.push_back(permission::Request {
        id: "per_1".into(),
        session_id: ui.sid(),
        tool_call_id: None,
        action: "shell".into(),
        resources: vec!["rm -rf build".into()],
        title: "Run: rm -rf build".into(),
        metadata: serde_json::json!({}),
        save: vec!["rm *".into()],
    });
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    println!("{screen}");
    assert!(screen.contains("Permission required") && screen.contains("- rm -rf build") && screen.contains("Always allow"));

    ui.permissions.clear();
    ui.open_palette();
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    println!("{screen}");
    assert!(screen.contains("Commands") && screen.contains("Suggested") && screen.contains("Switch model"));

    ui.dialog = None;
    ui.editor.set("/mo");
    ui.update_auto();
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    println!("{screen}");
    assert!(screen.contains("/models"));
}

#[tokio::test]
async fn skills_command_opens_dialog_and_inserts_mention() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    assert!(ui.slash_commands().iter().any(|(display, _, _)| display == "/skills"));
    ui.exec("prompt.skills", "");
    let Some(Dialog::Select(select)) = ui.dialog.as_ref() else {
        panic!("skills dialog did not open");
    };
    assert_eq!(select.kind, Kind::Skills);
    assert!(select.options.iter().any(|option| option.value == "opencode"));
    ui.dialog_select(Kind::Skills, "opencode".into());
    assert_eq!(ui.editor.text, "@opencode ");
    ui.exec("prompt.skills", "");
    ui.dialog_select(Kind::Skills, "opencode".into());
    assert_eq!(ui.editor.text, "@opencode ");
    let parts = crate::run::expand_mentions(&ui.app, "@opencode explain plugins");
    assert!(parts.iter().any(|part| matches!(part, Part::Text { text, .. } if text.contains("<skill_content"))));
    let skills = crate::run::skill_mentions(&ui.app, "@opencode explain plugins").unwrap();
    assert_eq!(skills[0]["id"], "opencode");
}

#[test]
fn plugin_layout_flattens_rows_and_columns() {
    let layout = serde_json::json!({
        "type": "box",
        "direction": "column",
        "children": [
            { "type": "text", "text": "one" },
            {
                "type": "box",
                "direction": "row",
                "children": [
                    { "type": "text", "text": "two" },
                    { "type": "text", "text": "three" }
                ]
            }
        ]
    });
    assert_eq!(super::layout_plain_lines(&layout, 0), vec!["one", "two  three"]);
}


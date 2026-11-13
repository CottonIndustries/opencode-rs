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
async fn slash_commands_cover_upstream_set() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    let commands: Vec<String> = ui.slash_commands().into_iter().map(|(name, _, _)| name).collect();
    for expected in [
        "/sessions", "/new", "/clear", "/open", "/models", "/agents", "/usage", "/mcps",
        "/variants", "/connect", "/highlights", "/settings", "/status", "/update", "/pair",
        "/restart", "/reload", "/debug", "/themes", "/help", "/exit", "/rename", "/timeline",
        "/fork", "/compact", "/undo", "/redo", "/copy", "/export", "/cd", "/editor", "/skills",
        "/worktrees", "/terminal", "/plugins", "/diff", "/stats", "/btw",
    ] {
        assert!(commands.iter().any(|name| name == expected), "missing {expected}");
    }
}

#[tokio::test]
async fn terminal_command_opens_persistent_terminal() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    ui.exec("session.terminal", "");
    let Some(Dialog::Terminal(term)) = ui.dialog.as_ref() else {
        panic!("terminal dialog did not open");
    };
    let _ = term.write("exit
");
    term.close();
}

#[tokio::test]
async fn tab_close_button_renders_on_hover_and_closes() {
    let mut ui = ui().await;
    ui.tabs.clear();
    let a = ui.app.new_session(None);
    a.save();
    let b = ui.app.new_session(None);
    b.save();
    let (aid, bid) = (a.id(), b.id());
    ui.open_session(&aid);
    ui.open_session(&bid);
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    let top = dump(&t).lines().next().unwrap_or_default().to_string();
    assert!(!top.contains('✕'), "close mark should be hidden without hover: {top:?}");
    let (column, _, id) = ui.hits.tab_close.iter().find(|(_, _, id)| *id == aid).cloned().expect("close button hit box");
    ui.on_mouse(event::MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    let hovered = dump(&t).lines().next().unwrap_or_default().to_string();
    assert!(hovered.contains('✕'), "close mark missing on hover");
    assert_eq!(hovered.chars().position(|c| c == '✕'), Some(column as usize), "close mark is not at the right edge: {hovered:?}");
    ui.on_mouse(event::MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(!ui.tabs.contains(&id));
    assert_eq!(ui.sid(), bid);
}

#[tokio::test]
async fn report_slash_commands_open_dialogs() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    for (command, title) in [
        ("plugins.list", "Plugins"),
        ("opencode.debug", "Debug"),
        ("opencode.highlights", "Release highlights"),
        ("opencode.update", "Update"),
    ] {
        ui.exec(command, "");
        match ui.dialog.as_ref() {
            Some(Dialog::Text { title: actual, .. }) => assert_eq!(actual, title),
            _ => panic!("{command} did not open a text dialog"),
        }
    }
    ui.open_connect();
    assert!(matches!(ui.dialog, Some(Dialog::Select(_))));
    ui.exec("session.cd", "");
    assert!(matches!(ui.dialog, Some(Dialog::Rename(_))));
    ui.exec("session.aside", "");
    assert!(matches!(ui.dialog, Some(Dialog::Rename(_))));
    ui.exec("session.worktrees", "");
    assert!(matches!(ui.dialog, Some(Dialog::Select(_))));
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
    let parts = crate::mentions::expand_mentions(&ui.app, "@opencode explain plugins");
    assert!(parts.iter().any(|part| matches!(part, Part::Text { text, .. } if text.contains("<skill_content"))));
    let skills = crate::mentions::skill_mentions(&ui.app, "@opencode explain plugins").unwrap();
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

#[tokio::test]
async fn plugin_page_edits_input_and_cycles_select() {
    let mut ui = ui().await;
    let layout = serde_json::json!({
        "type": "box",
        "direction": "column",
        "children": [
            { "type": "input", "value": "", "placeholder": "name" },
            { "type": "select", "options": ["one", "two"], "selected": 0 }
        ]
    });
    ui.on_app_event(AppEvent::PluginPage {
        page: String::new(),
        title: "Interactive".into(),
        text: String::new(),
        layout,
    });
    let event = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    let key = keymap::from_event(&event).unwrap();
    ui.dialog_key(&key, &event);
    let Some(Dialog::Plugin { layout, .. }) = ui.dialog.as_ref() else {
        panic!("plugin dialog did not open");
    };
    let controls = plugin_ui::controls(layout);
    assert_eq!(controls[0].value, "x");
    let event = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let key = keymap::from_event(&event).unwrap();
    ui.dialog_key(&key, &event);
    let event = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
    let key = keymap::from_event(&event).unwrap();
    ui.dialog_key(&key, &event);
    let Some(Dialog::Plugin { layout, .. }) = ui.dialog.as_ref() else {
        panic!("plugin dialog disappeared");
    };
    assert_eq!(plugin_ui::controls(layout)[1].selected, 1);
}

#[tokio::test]
async fn mcp_auth_dialog_shows_url_and_closes() {
    let mut ui = ui().await;
    ui.on_app_event(AppEvent::McpAuth {
        name: "context7".into(),
        url: "https://clerk.context7.com/oauth/authorize?client_id=test&state=abc".into(),
        instructions: "Authorize context7 in your browser. This window will close automatically.".into(),
        handle: None,
    });
    let mut t = Terminal::new(TestBackend::new(120, 40)).unwrap();
    t.draw(|f| ui.draw(f)).unwrap();
    let screen = dump(&t);
    assert!(screen.contains("Authorize context7 in your browser"), "missing instructions in {screen:?}");
    assert!(screen.contains("clerk.context7.com"), "missing sign-in URL in {screen:?}");
    assert!(screen.contains("Redirect URL"), "missing redirect URL field in {screen:?}");
    assert!(screen.contains("Waiting for authorization"), "missing waiting text in {screen:?}");
    ui.on_app_event(AppEvent::McpAuthDone { name: "context7".into(), error: Some("access denied".into()) });
    assert!(ui.dialog.is_none(), "sign-in dialog stayed open after failure");
}


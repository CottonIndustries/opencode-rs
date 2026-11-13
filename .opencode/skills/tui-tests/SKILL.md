---
name: tui-tests
description: Use when writing, fixing, or reviewing ratatui TUI tests in opencode-rs. Covers src/tui/tests.rs, TestBackend rendering, the ui() fixture, screen assertions, dialogs and key handling, plugin layout trees, temp session cleanup, and Windows text differences. For CLI/server end-to-end tests, use app-e2e-tests.
---

# TUI tests

The TUI suite is ordinary `cargo test` code. It draws into a `ratatui::backend::TestBackend` and asserts on the resulting cell buffer; there is no browser, OpenTUI renderer or frame clock to wait on.

## Harness

`src/tui/tests.rs` already provides:

- `dump(&Terminal<TestBackend>)` - the screen as text, one line per row.
- `rows(&str)` - the screen as `Vec<Vec<char>>` for position assertions.
- `ui()` - a `Ui` with a temp directory, a configured model and the fallback dark theme.

```rust
#[tokio::test]
async fn draws_session_and_dialogs() {
    let mut ui = ui().await;
    ui.route = Route::Session;
    let mut terminal = Terminal::new(TestBackend::new(100, 60)).unwrap();
    terminal.draw(|frame| ui.draw(frame)).unwrap();
    let screen = dump(&terminal);
    assert!(screen.contains("Please fix notes.txt"));
}
```

Call `ui.draw` synchronously through `terminal.draw`; do not add sleeps for rendering.

## What to assert

- Prefer stable substrings: titles, labels, tool names, file names, status text.
- Use `rows` and coordinates only for layout tests that intentionally pin upstream geometry, such as `home_matches_upstream_layout`.
- Avoid asserting exact padding, ANSI color sequences or the full frame unless the test is a visual-parity test.
- For hidden states, assert the absence of a label and the presence of the replacement state.
- For scrolling, assert the visible window, not all transcript lines.

## Dialogs and input

Drive the same entry points as the key handler:

```rust
ui.exec("prompt.skills", "");
let Some(Dialog::Select(select)) = ui.dialog.as_ref() else { panic!("no dialog") };
assert!(select.options.iter().any(|option| option.value == "opencode"));
ui.dialog_select(Kind::Skills, "opencode".into());
assert_eq!(ui.editor.text, "@opencode ");
```

Use `ui.on_key` only when the key mapping itself is the behavior under test. `exec`/`dialog_select` keep tests independent of terminal event encoding.

## Sizes

Test at least one realistic size. Use 120x40 for home/layout parity and 100x60 for a transcript with tool output. When a change is width-sensitive, add a narrow case (for example 60x24) rather than shrinking the only test.

## Sessions and cleanup

`ui()` uses a shared temp path. Tests that create sessions should delete them (`crate::session::delete`) or open their own unique directory:

```rust
let dir = std::env::temp_dir().join(format!("opencode-tui-{}", crate::util::id("")));
```

Do not rely on execution order between tests. If a test sets environment variables or global config, serialize it with a lock or restore the previous value in a guard.

## Plugin layout trees

The host reduces plugin JSX to `box`/`text` nodes. Test the reducer separately:

```rust
#[test]
fn plugin_layout_flattens_rows_and_columns() {
    let layout = serde_json::json!({
        "type": "box",
        "direction": "row",
        "children": [
            { "type": "text", "text": "one" },
            { "type": "text", "text": "two" }
        ]
    });
    assert_eq!(super::layout_plain_lines(&layout, 0), vec!["one  two"]);
}
```

For rendered pages and slots, assert the visible text through `Dialog::Text` or the relevant slot surface rather than reimplementing the layout.

## Windows notes

- Unicode symbols and box-drawing characters can render differently in a real terminal, but `TestBackend` stores the exact symbols, so assertions are stable.
- Paths may contain `\\?\` or short names; compare normalized or suffix text.
- Avoid platform-specific shell output in fixtures unless the tool is the subject.

## Budget

Run a focused test while iterating:

```powershell
cargo test tui::tests::draws_session_and_dialogs -- --nocapture
```

Then run the full suite. A TUI test that needs a real terminal, a real provider or a spawned server belongs in an e2e test, not here.

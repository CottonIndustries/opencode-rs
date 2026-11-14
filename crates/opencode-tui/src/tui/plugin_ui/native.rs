use super::*;
use crate::tui::plugin_native::{self, Scene};
use std::cell::RefCell;

thread_local! {
    static SCENE: RefCell<Option<Scene>> = const { RefCell::new(None) };
}

pub(super) fn render(
    layout: &Value,
    width: usize,
    theme: &Theme,
    background: Rgba,
) -> Option<Vec<Line<'static>>> {
    let node = parse(layout, theme, 0, Some(theme.base.text))?;
    if !supported(&node) {
        return None;
    }
    let width = width.max(1) as u32;
    SCENE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.is_none() {
            *cache = Some(Scene::new(width, MAX_LINES as u32)?);
        }
        let scene = cache.as_mut().unwrap();
        if !scene.reset(width, MAX_LINES as u32) {
            *cache = None;
            return None;
        }
        let root = scene.root();
        let mut specials = Vec::new();
        if !build(scene, &node, root, 0, theme, &mut specials) {
            return None;
        }
        if std::env::var_os("OPENCODE_NATIVE_DEBUG").is_some() {
            scene.measure();
            eprintln!("[opentui-native] rects {:?}", scene.debug_rects());
        }
        if !specials.is_empty() {
            scene.measure();
            for (id, kind) in specials {
                let width = scene
                    .layout_rect(id)
                    .map(|(_, _, width, _)| width as usize)
                    .filter(|width| *width > 0)
                    .unwrap_or(80);
                let lines = special_lines(&kind, width, theme, background);
                scene.layout(id, 2, 0, 0, 1, width as f32, 0);
                scene.set_lines(id, &lines, Some(theme.base.text), Some(background));
            }
        }
        scene.render(background)
    })
}

fn supported(node: &Node) -> bool {
    if !matches!(
        node.kind,
        Kind::Box(_)
            | Kind::Text(_)
            | Kind::Input(_)
            | Kind::Select(_)
            | Kind::Markdown(_)
            | Kind::Code { .. }
            | Kind::Diff(_)
            | Kind::Image(_)
    ) {
        return false;
    }
    if node.scroll_top > 0 || node.scroll_left > 0 {
        return false;
    }
    match &node.kind {
        Kind::Box(children) => children.iter().all(supported),
        _ => true,
    }
}

fn build(
    scene: &mut Scene,
    node: &Node,
    parent: plugin_native::SceneHandle,
    index: u32,
    theme: &Theme,
    specials: &mut Vec<(plugin_native::SceneHandle, Kind)>,
) -> bool {
    let tag = match node.kind {
        Kind::Box(_) => plugin_native::NODE_BOX,
        Kind::Text(_) | Kind::Input(_) | Kind::Select(_) => plugin_native::NODE_TEXT,
        Kind::Markdown(_) | Kind::Code { .. } | Kind::Diff(_) | Kind::Image(_) => {
            plugin_native::NODE_TEXT
        }
    };
    let Some(id) = scene.create(tag) else {
        return false;
    };
    apply_layout(scene, id, node);
    apply_paint(scene, id, node, theme);
    match &node.kind {
        Kind::Box(children) => {
            for (child_index, child) in children.iter().enumerate() {
                if !build(scene, child, id, child_index as u32, theme, specials) {
                    return false;
                }
            }
        }
        Kind::Text(text) => {
            if !set_text(scene, id, node, text, theme) {
                return false;
            }
        }
        Kind::Input(input) => {
            if !set_input(scene, id, node, input, theme) {
                return false;
            }
        }
        Kind::Select(select) => {
            if !set_select(scene, id, node, select, theme) {
                return false;
            }
        }
        Kind::Markdown(_) | Kind::Code { .. } | Kind::Diff(_) | Kind::Image(_) => {
            specials.push((id, node.kind.clone()));
        }
    }
    scene.insert(parent, id, index)
}

fn apply_layout(scene: &mut Scene, id: plugin_native::SceneHandle, node: &Node) {
    if let Some(width) = node.width {
        set_dim(scene, id, 0, width, false);
    }
    if let Some(height) = node.height {
        set_dim(scene, id, 1, height, false);
    }
    if let Some(value) = node.min_width {
        set_dim(scene, id, 2, value, true);
    }
    if let Some(value) = node.min_height {
        set_dim(scene, id, 3, value, true);
    }
    if let Some(value) = node.max_width {
        set_dim(scene, id, 4, value, true);
    }
    if let Some(value) = node.max_height {
        set_dim(scene, id, 5, value, true);
    }
    if let Some(value) = node.basis {
        set_dim(scene, id, 6, value, false);
    }
    for (edge, value) in [
        (1, node.padding.top),
        (2, node.padding.right),
        (3, node.padding.bottom),
        (0, node.padding.left),
    ] {
        if value > 0 {
            scene.layout(id, 2, 8, edge, 1, value as f32, 0);
        }
    }
    for (edge, value) in [
        (1, node.margin.top),
        (2, node.margin.right),
        (3, node.margin.bottom),
        (0, node.margin.left),
    ] {
        if value > 0 {
            scene.layout(id, 2, 7, edge, 1, value as f32, 0);
        }
    }
    if node.gap > 0 {
        scene.layout(id, 2, 10, 2, 1, node.gap as f32, 0);
    }
    if node.row_gap > 0 {
        scene.layout(id, 2, 10, 1, 1, node.row_gap as f32, 0);
    }
    if node.column_gap > 0 {
        scene.layout(id, 2, 10, 0, 1, node.column_gap as f32, 0);
    }
    let direction = match node.direction {
        Direction::Row => 2,
        Direction::RowReverse => 3,
        Direction::ColumnReverse => 1,
        Direction::Column => 0,
    };
    scene.layout(id, 0, 1, 0, 0, direction as f32, 0);
    let justify = match node.justify {
        Justify::Start => 0,
        Justify::Center => 1,
        Justify::End => 2,
        Justify::SpaceBetween => 3,
        Justify::SpaceAround => 4,
        Justify::SpaceEvenly => 5,
    };
    scene.layout(id, 0, 2, 0, 0, justify as f32, 0);
    if matches!(node.kind, Kind::Box(_)) {
        let align = match node.align {
            Align::Start => 1,
            Align::Center => 2,
            Align::End => 3,
            Align::Stretch => 4,
        };
        scene.layout(id, 0, 4, 0, 0, align as f32, 0);
    }
    if let Some(align) = node.align_self {
        let value = match align {
            Align::Start => 1,
            Align::Center => 2,
            Align::End => 3,
            Align::Stretch => 4,
        };
        scene.layout(id, 0, 5, 0, 0, value as f32, 0);
    }
    if matches!(node.position, Position::Absolute) {
        scene.layout(id, 0, 6, 0, 0, 2.0, 0);
    }
    if node.wrap {
        scene.layout(id, 0, 7, 0, 0, 1.0, 0);
    }
    if node.overflow {
        scene.layout(id, 0, 8, 0, 0, 1.0, 0);
    }
    if node.grow != 0.0 {
        scene.layout(id, 1, 1, 0, 0, node.grow, 0);
    }
    if node.shrink != 1.0 {
        scene.layout(id, 1, 2, 0, 0, node.shrink, 0);
    }
    if let Some(ratio) = node.aspect_ratio {
        scene.layout(id, 1, 3, 0, 0, ratio, 0);
    }
    if let Some(value) = node.left {
        set_position(scene, id, 0, value);
    }
    if let Some(value) = node.top {
        set_position(scene, id, 1, value);
    }
    if let Some(value) = node.right {
        set_position(scene, id, 2, value);
    }
    if let Some(value) = node.bottom {
        set_position(scene, id, 3, value);
    }
    if node.hidden {
        scene.layout(id, 0, 9, 0, 0, 1.0, 0);
    }
}

fn set_dim(
    scene: &mut Scene,
    id: plugin_native::SceneHandle,
    kind: u32,
    value: Dim,
    undefined_auto: bool,
) {
    let (unit, number) = match value {
        Dim::Auto if undefined_auto => return,
        Dim::Auto => (3, 0.0),
        Dim::Cells(value) => (1, value as f32),
        Dim::Percent(value) => (2, value),
    };
    scene.layout(id, 2, kind, 0, unit, number, 0);
}

fn set_position(scene: &mut Scene, id: plugin_native::SceneHandle, edge: u32, value: Dim) {
    let (unit, number) = match value {
        Dim::Auto => (3, 0.0),
        Dim::Cells(value) => (1, value as f32),
        Dim::Percent(value) => (2, value),
    };
    scene.layout(id, 2, 9, edge, unit, number, 0);
}

fn apply_paint(scene: &mut Scene, id: plugin_native::SceneHandle, node: &Node, theme: &Theme) {
    let background = node
        .background
        .filter(|color| color.a > 0.0)
        .or_else(|| matches!(node.kind, Kind::Box(_)).then_some(Rgba::TRANSPARENT));
    let border = node.border.as_deref().and_then(border_style);
    let border_color = node.border_color.or(Some(theme.base.border));
    let paint = plugin_native::paint_options(background, border.map(|style| (style, border_color)));
    scene.paint(id, &paint);
    if let Some(title) = node.title.as_deref() {
        if !title.is_empty() {
            let align = match node.align {
                Align::Center => plugin_native::TEXT_ALIGN_CENTER,
                Align::End => plugin_native::TEXT_ALIGN_RIGHT,
                _ => plugin_native::TEXT_ALIGN_LEFT,
            };
            scene.box_details(id, title, align, border_color);
        }
    }
}

fn border_style(value: &str) -> Option<u32> {
    match value.to_ascii_lowercase().as_str() {
        "single" | "plain" => Some(plugin_native::BORDER_SINGLE),
        "double" => Some(plugin_native::BORDER_DOUBLE),
        "rounded" => Some(plugin_native::BORDER_ROUNDED),
        "heavy" | "thick" => Some(plugin_native::BORDER_HEAVY),
        _ => None,
    }
}

fn special_lines(kind: &Kind, width: usize, theme: &Theme, background: Rgba) -> Vec<Line<'static>> {
    match kind {
        Kind::Markdown(text) => markdown::render(text, width, theme, background),
        Kind::Code { text, lang } => highlight::text(text, lang, &theme.syntax, theme.base.text)
            .into_iter()
            .map(|runs| {
                let spans = runs
                    .into_iter()
                    .map(|(text, color)| draw::span(text, color, background))
                    .collect::<Vec<_>>();
                draw::line(spans, background)
            })
            .collect(),
        Kind::Diff(text) => text
            .lines()
            .map(|source| {
                let color = if source.starts_with('+') {
                    theme.base.success.base
                } else if source.starts_with('-') {
                    theme.base.error.base
                } else if source.starts_with("@@") {
                    theme.base.info.base
                } else {
                    theme.base.text
                };
                draw::line(
                    vec![draw::span(source.to_string(), color, background)],
                    background,
                )
            })
            .collect(),
        Kind::Image(image) => super::image_lines(image, width, 20, theme, background),
        _ => Vec::new(),
    }
}

fn set_text(
    scene: &mut Scene,
    id: plugin_native::SceneHandle,
    node: &Node,
    text: &Text,
    theme: &Theme,
) -> bool {
    let chunks = text
        .segments
        .iter()
        .map(|segment| plugin_native::StyledChunk {
            text: segment.text.as_str(),
            fg: segment.fg,
            bg: segment.bg,
            attributes: attributes(segment),
        })
        .collect::<Vec<_>>();
    let fg = text
        .segments
        .iter()
        .find_map(|segment| segment.fg)
        .or(Some(theme.base.text));
    let wrap = match text.wrap {
        draw::Wrap::None => plugin_native::WRAP_NONE,
        draw::Wrap::Char => plugin_native::WRAP_CHAR,
        draw::Wrap::Word => plugin_native::WRAP_WORD,
    };
    let align = match node.align {
        Align::Center => plugin_native::TEXT_ALIGN_CENTER,
        Align::End => plugin_native::TEXT_ALIGN_RIGHT,
        _ => plugin_native::TEXT_ALIGN_LEFT,
    };
    let options = plugin_native::text_options(fg, node.background, wrap, align);
    scene.text_options(id, &options) && scene.styled_text(id, &chunks)
}

fn set_input(
    scene: &mut Scene,
    id: plugin_native::SceneHandle,
    node: &Node,
    input: &Input,
    theme: &Theme,
) -> bool {
    let raw = if input.value.is_empty() {
        input.placeholder.clone()
    } else {
        input.value.clone()
    };
    let fg = if input.value.is_empty() {
        theme.base.muted
    } else {
        theme.base.text
    };
    let options = plugin_native::text_options_with(
        Some(fg),
        node.background,
        if input.multiline {
            plugin_native::WRAP_WORD
        } else {
            plugin_native::WRAP_NONE
        },
        plugin_native::TEXT_ALIGN_LEFT,
        !input.multiline,
    );
    let mut before = String::new();
    let mut after = String::new();
    if input.focused {
        let cursor = input.cursor.min(raw.chars().count());
        before = raw.chars().take(cursor).collect();
        after = raw.chars().skip(cursor).collect();
    }
    let mut chunks = Vec::new();
    if input.focused {
        if !before.is_empty() {
            chunks.push(plugin_native::StyledChunk {
                text: before.as_str(),
                fg: Some(fg),
                bg: None,
                attributes: 0,
            });
        }
        chunks.push(plugin_native::StyledChunk {
            text: "▌",
            fg: Some(theme.base.bg),
            bg: Some(theme.base.text),
            attributes: 0,
        });
        if !after.is_empty() {
            chunks.push(plugin_native::StyledChunk {
                text: after.as_str(),
                fg: Some(fg),
                bg: None,
                attributes: 0,
            });
        }
    } else if !raw.is_empty() {
        chunks.push(plugin_native::StyledChunk {
            text: raw.as_str(),
            fg: Some(fg),
            bg: None,
            attributes: 0,
        });
    }
    scene.text_options(id, &options) && scene.styled_text(id, &chunks)
}

fn set_select(
    scene: &mut Scene,
    id: plugin_native::SceneHandle,
    node: &Node,
    select: &Select,
    theme: &Theme,
) -> bool {
    let options = plugin_native::text_options_with(
        Some(theme.base.text),
        node.background,
        plugin_native::WRAP_NONE,
        plugin_native::TEXT_ALIGN_LEFT,
        true,
    );
    let mut owned: Vec<(String, Option<Rgba>, u32)> = Vec::new();
    for (index, option) in select.options.iter().enumerate() {
        if index > 0 {
            owned.push(("\n".to_string(), None, 0));
        }
        let selected = index == select.selected;
        let text = if selected {
            format!("▸ {option}")
        } else {
            format!("  {option}")
        };
        owned.push((
            text,
            Some(if selected {
                theme.base.text
            } else {
                theme.base.muted
            }),
            if selected {
                plugin_native::ATTR_BOLD
            } else {
                0
            },
        ));
    }
    if select.options.is_empty() {
        owned.push(("  ".to_string(), Some(theme.base.muted), 0));
    }
    let chunks = owned
        .iter()
        .map(|(text, fg, attributes)| plugin_native::StyledChunk {
            text: text.as_str(),
            fg: *fg,
            bg: None,
            attributes: *attributes,
        })
        .collect::<Vec<_>>();
    scene.layout(id, 2, 1, 0, 1, select.options.len().max(1) as f32, 0);
    scene.text_options(id, &options) && scene.styled_text(id, &chunks)
}

fn attributes(segment: &Segment) -> u32 {
    let mut value = 0;
    if segment.bold {
        value |= plugin_native::ATTR_BOLD;
    }
    if segment.dim {
        value |= plugin_native::ATTR_DIM;
    }
    if segment.italic {
        value |= plugin_native::ATTR_ITALIC;
    }
    if segment.underline {
        value |= plugin_native::ATTR_UNDERLINE;
    }
    if segment.strike {
        value |= plugin_native::ATTR_STRIKETHROUGH;
    }
    if segment.inverse {
        value |= plugin_native::ATTR_INVERSE;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme;

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_box_and_text() {
        let theme = theme::fallback(theme::Mode::Dark);
        let lines = render(
            &serde_json::json!({
                "type": "box",
                "direction": "column",
                "padding": 1,
                "gap": 1,
                "border": "single",
                "children": [
                    { "type": "text", "text": "Context", "segments": [{ "text": "Context", "color": "#ff0000", "bold": true }] },
                    { "type": "text", "text": "72%" }
                ]
            }),
            30,
            &theme,
            theme.base.bg,
        )
        .expect("native renderer");
        let text = text(&lines);
        assert!(text.contains("Context"), "{text:?}");
        assert!(text.contains("72%"), "{text:?}");
        assert!(text.contains('┌') || text.contains('─'), "{text:?}");
    }

    #[test]
    fn places_row_children() {
        let theme = theme::fallback(theme::Mode::Dark);
        let lines = render(
            &serde_json::json!({
                "type": "box",
                "direction": "row",
                "justify": "end",
                "width": "100%",
                "children": [
                    { "type": "text", "text": "left", "width": 4 },
                    { "type": "text", "text": "right", "width": 5 }
                ]
            }),
            20,
            &theme,
            theme.base.bg,
        )
        .expect("native renderer");
        let text = text(&lines);
        assert!(text.contains("left"), "{text:?}");
        assert!(text.contains("right"), "{text:?}");
    }

    #[test]
    fn repeated_render_reuses_scene() {
        let theme = theme::fallback(theme::Mode::Dark);
        let layout = serde_json::json!({
            "type": "box",
            "direction": "row",
            "justify": "space-between",
            "children": [
                { "type": "text", "text": "left" },
                { "type": "text", "text": "right" }
            ]
        });
        for _ in 0..20 {
            let lines = render(&layout, 30, &theme, theme.base.bg).expect("native renderer");
            let text = text(&lines);
            assert!(text.contains("left"), "{text:?}");
            assert!(text.contains("right"), "{text:?}");
        }
    }

    #[test]
    fn renders_markdown_code_and_diff() {
        let theme = theme::fallback(theme::Mode::Dark);
        let layout = serde_json::json!({
            "type": "box",
            "direction": "column",
            "children": [
                { "type": "markdown", "text": "# Title\n\nBody" },
                { "type": "code", "lang": "rust", "text": "fn main() {\n    println!(\"hi\");\n}" },
                { "type": "diff", "text": "+ added\n- removed" }
            ]
        });
        let lines = render(&layout, 40, &theme, theme.base.bg).expect("native renderer");
        let text = text(&lines);
        assert!(text.contains("Title"), "{text:?}");
        assert!(text.contains("fn main"), "{text:?}");
        assert!(text.contains("+ added"), "{text:?}");
        assert!(text.contains("- removed"), "{text:?}");
    }

    #[test]
    fn renders_input_cursor_and_select_options() {
        let theme = theme::fallback(theme::Mode::Dark);
        let layout = serde_json::json!({
            "type": "box",
            "direction": "column",
            "children": [
                { "type": "input", "value": "hello", "cursor": 2, "focused": true },
                { "type": "select", "options": ["one", "two", "three"], "selected": 1, "focused": true }
            ]
        });
        let lines = render(&layout, 30, &theme, theme.base.bg).expect("native renderer");
        let text = text(&lines);
        assert!(text.contains("he▌llo"), "{text:?}");
        assert!(text.contains("▸ two"), "{text:?}");
        assert!(text.contains("  one"), "{text:?}");
        assert!(text.contains("  three"), "{text:?}");
    }

    #[test]
    fn renders_image_data_url() {
        let theme = theme::fallback(theme::Mode::Dark);
        let layout = serde_json::json!({
            "type": "box",
            "direction": "column",
            "children": [{
                "type": "image",
                "src": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl2ZQAAAABJRU5ErkJggg==",
                "alt": "[image]",
                "width": 10,
                "height": 5
            }]
        });
        let lines = render(&layout, 20, &theme, theme.base.bg).expect("native renderer");
        assert!(!lines.is_empty());
        let text = text(&lines);
        assert!(!text.trim().is_empty(), "{text:?}");
    }

    #[test]
    fn handles_magic_context_sidebar() {
        let theme = theme::fallback(theme::Mode::Dark);
        let layout: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/magic-context-sidebar.json"
        ))
        .unwrap();
        let lines = render(&layout, 38, &theme, theme.base.bg).expect("native renderer");
        let text = text(&lines);
        for expected in ["Magic Context", "Historian", "Compartments", "Memories"] {
            assert!(text.contains(expected), "missing {expected}: {text:?}");
        }
        let title = lines.get(1).expect("title row");
        assert!(
            title
                .spans
                .iter()
                .any(|span| span.style.bg == Some(ratatui::style::Color::Rgb(0x5f, 0x87, 0xff))),
            "missing title background: {title:?}"
        );
    }
}

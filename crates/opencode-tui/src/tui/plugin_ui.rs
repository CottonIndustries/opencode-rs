//! Plugin JSX layout. OpenTUI plugins hand the host a declarative element tree;
//! this module turns the common subset into a cell buffer with flex-like row
//! and column placement, padding, borders, wrapping and styled text runs.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::draw;
use super::theme::{Rgba, Theme};
use super::{highlight, markdown};

const MAX_DEPTH: usize = 32;
const MAX_LINES: usize = 400;

#[derive(Clone, Copy, Debug, Default)]
struct Edges {
    top: usize,
    right: usize,
    bottom: usize,
    left: usize,
}

#[derive(Clone, Copy, Debug)]
enum Dim {
    Auto,
    Cells(usize),
    Percent(f32),
}

impl Dim {
    fn is_auto(self) -> bool {
        matches!(self, Dim::Auto)
    }

    fn resolve(self, parent: usize) -> usize {
        match self {
            Dim::Auto => parent,
            Dim::Cells(value) => value,
            Dim::Percent(value) => ((parent as f32) * (value / 100.0)).round().max(0.0) as usize,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Row,
    Column,
    RowReverse,
    ColumnReverse,
}

impl Direction {
    fn is_row(self) -> bool {
        matches!(self, Direction::Row | Direction::RowReverse)
    }

    fn is_column(self) -> bool {
        matches!(self, Direction::Column | Direction::ColumnReverse)
    }

    fn reversed(self) -> bool {
        matches!(self, Direction::RowReverse | Direction::ColumnReverse)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Align {
    Start,
    Center,
    End,
    Stretch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Justify {
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Position {
    Relative,
    Absolute,
}

#[derive(Clone, Debug)]
struct Segment {
    text: String,
    fg: Option<Rgba>,
    bg: Option<Rgba>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    inverse: bool,
}

#[derive(Clone)]
struct Text {
    segments: Vec<Segment>,
    wrap: draw::Wrap,
}

#[derive(Clone)]
enum Kind {
    Text(Text),
    Box(Vec<Node>),
    Markdown(String),
    Code { text: String, lang: String },
    Diff(String),
    Input(Input),
    Select(Select),
}

#[derive(Clone)]
struct Input {
    value: String,
    placeholder: String,
    cursor: usize,
    focused: bool,
    multiline: bool,
}

#[derive(Clone)]
struct Select {
    options: Vec<String>,
    selected: usize,
    focused: bool,
}

#[derive(Clone)]
struct Node {
    kind: Kind,
    direction: Direction,
    width: Option<Dim>,
    height: Option<Dim>,
    min_width: Option<Dim>,
    max_width: Option<Dim>,
    min_height: Option<Dim>,
    max_height: Option<Dim>,
    basis: Option<Dim>,
    padding: Edges,
    margin: Edges,
    gap: usize,
    row_gap: usize,
    column_gap: usize,
    aspect_ratio: Option<f32>,
    grow: f32,
    shrink: f32,
    align_self: Option<Align>,
    wrap: bool,
    overflow: bool,
    scroll_top: usize,
    scroll_left: usize,
    border: Option<String>,
    border_color: Option<Rgba>,
    title: Option<String>,
    background: Option<Rgba>,
    align: Align,
    justify: Justify,
    position: Position,
    left: Option<Dim>,
    top: Option<Dim>,
    right: Option<Dim>,
    bottom: Option<Dim>,
    hidden: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct Size {
    width: usize,
    height: usize,
}

fn dim(value: Option<&Value>) -> Option<Dim> {
    let value = value?;
    if let Some(number) = value.as_u64() {
        return Some(Dim::Cells(number as usize));
    }
    if let Some(number) = value.as_f64() {
        return Some(Dim::Cells(number.max(0.0) as usize));
    }
    let text = value.as_str()?.trim().to_ascii_lowercase();
    if text.is_empty() || text == "auto" {
        return Some(Dim::Auto);
    }
    if let Some(percent) = text
        .strip_suffix('%')
        .and_then(|value| value.trim().parse::<f32>().ok())
    {
        return Some(Dim::Percent(percent));
    }
    text.parse::<usize>().ok().map(Dim::Cells)
}

fn number(value: Option<&Value>) -> Option<usize> {
    let value = value?;
    value
        .as_u64()
        .map(|value| value as usize)
        .or_else(|| value.as_f64().map(|value| value.max(0.0) as usize))
}

fn color(value: Option<&Value>, theme: &Theme) -> Option<Rgba> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return color_name(text, theme);
    }
    if let Some(number) = value.as_u64() {
        return Some(Rgba::from_ints(
            (number >> 16) as u8,
            (number >> 8) as u8,
            number as u8,
            255,
        ));
    }
    let object = value.as_object()?;
    if let Some(hex) = object.get("hex").and_then(Value::as_str) {
        return color_name(hex, theme);
    }
    let channel = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_f64)
            .map(|value| value.clamp(0.0, 1.0) as f32)
    };
    if let (Some(r), Some(g), Some(b)) = (channel("r"), channel("g"), channel("b")) {
        return Some(Rgba {
            r,
            g,
            b,
            a: channel("a").unwrap_or(1.0),
        });
    }
    None
}

fn color_name(value: &str, theme: &Theme) -> Option<Rgba> {
    if let Some(color) = Rgba::from_hex(value) {
        return Some(color);
    }
    match value.to_ascii_lowercase().as_str() {
        "text" => Some(theme.base.text),
        "muted" | "dim" => Some(theme.base.muted),
        "faint" => Some(theme.base.faint),
        "border" => Some(theme.base.border),
        "bg" | "background" => Some(theme.base.bg),
        "raised" => Some(theme.base.raised),
        "success" => Some(theme.base.success.base),
        "warning" => Some(theme.base.warning.base),
        "error" => Some(theme.base.error.base),
        "info" => Some(theme.base.info.base),
        "black" => Some(Rgba::BLACK),
        "white" => Some(Rgba::from_ints(255, 255, 255, 255)),
        "transparent" => Some(Rgba::TRANSPARENT),
        _ => None,
    }
}

fn bool_of(value: Option<&Value>) -> bool {
    value.and_then(Value::as_bool).unwrap_or(false)
}

fn segments(value: &Value, inherited: &Option<Rgba>, theme: &Theme) -> Vec<Segment> {
    let mut out = Vec::new();
    let Some(items) = value.as_array() else {
        if let Some(text) = value.as_str() {
            if !text.is_empty() {
                out.push(Segment {
                    text: text.to_string(),
                    fg: *inherited,
                    bg: None,
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    strike: false,
                    inverse: false,
                });
            }
        }
        return out;
    };
    for item in items {
        let text = item.get("text").and_then(Value::as_str).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        out.push(Segment {
            text: text.to_string(),
            fg: color(item.get("color"), theme).or(*inherited),
            bg: color(item.get("background"), theme),
            bold: bool_of(item.get("bold")),
            dim: bool_of(item.get("dim")),
            italic: bool_of(item.get("italic")),
            underline: bool_of(item.get("underline")),
            strike: bool_of(item.get("strikethrough")),
            inverse: bool_of(item.get("inverse")),
        });
    }
    out
}

fn parse(value: &Value, theme: &Theme, depth: usize, inherited: Option<Rgba>) -> Option<Node> {
    if depth > MAX_DEPTH {
        return None;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("text") => {
            let mut items = segments(
                value.get("segments").unwrap_or(&Value::Null),
                &inherited,
                theme,
            );
            if items.is_empty() {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    items = segments(&Value::String(text.to_string()), &inherited, theme);
                }
            }
            if items.is_empty() {
                return None;
            }
            let node_color = color(value.get("color"), theme).or(inherited);
            if items.iter().all(|item| item.fg.is_none()) {
                for item in &mut items {
                    item.fg = node_color;
                }
            }
            let background = color(value.get("background"), theme);
            if background.is_some() {
                for item in &mut items {
                    if item.bg.is_none() {
                        item.bg = background;
                    }
                }
            }
            let wrap = match value.get("wrap").and_then(Value::as_str).unwrap_or("word") {
                "none" => draw::Wrap::None,
                "char" => draw::Wrap::Char,
                _ => draw::Wrap::Word,
            };
            Some(Node {
                kind: Kind::Text(Text {
                    segments: items,
                    wrap,
                }),
                direction: Direction::Column,
                width: dim(value.get("width")),
                height: dim(value.get("height")),
                min_width: dim(value.get("minWidth")),
                max_width: dim(value.get("maxWidth")),
                min_height: dim(value.get("minHeight")),
                max_height: dim(value.get("maxHeight")),
                basis: dim(value.get("basis")),
                padding: Edges::default(),
                margin: Edges::default(),
                gap: 0,
                row_gap: 0,
                column_gap: 0,
                aspect_ratio: None,
                grow: value.get("grow").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                shrink: value.get("shrink").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                align_self: match value.get("alignSelf").and_then(Value::as_str) {
                    Some("center") => Some(Align::Center),
                    Some("end") | Some("flex-end") => Some(Align::End),
                    Some("stretch") => Some(Align::Stretch),
                    Some("start") | Some("flex-start") => Some(Align::Start),
                    _ => None,
                },
                wrap: false,
                overflow: false,
                scroll_top: 0,
                scroll_left: 0,
                border: None,
                border_color: None,
                title: None,
                background,
                align: match value.get("align").and_then(Value::as_str) {
                    Some("center") => Align::Center,
                    Some("end") | Some("right") => Align::End,
                    _ => Align::Start,
                },
                justify: Justify::Start,
                position: match value.get("position").and_then(Value::as_str) {
                    Some("absolute") => Position::Absolute,
                    _ => Position::Relative,
                },
                left: dim(value.get("left")),
                top: dim(value.get("top")),
                right: dim(value.get("right")),
                bottom: dim(value.get("bottom")),
                hidden: false,
            })
        }
        Some("markdown") => {
            let text = value.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
            if text.trim().is_empty() {
                return None;
            }
            Some(leaf(Kind::Markdown(text), color(value.get("background"), theme)))
        }
        Some("code") | Some("diff") => {
            let text = value.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
            if text.is_empty() {
                return None;
            }
            let kind = if value.get("type").and_then(Value::as_str) == Some("diff") {
                Kind::Diff(text)
            } else {
                Kind::Code {
                    text,
                    lang: value.get("lang").and_then(Value::as_str).unwrap_or_default().to_string(),
                }
            };
            Some(leaf(kind, color(value.get("background"), theme)))
        }
        Some("input") => {
            let value_text = value.get("value").and_then(Value::as_str).unwrap_or_default().to_string();
            let placeholder = value.get("placeholder").and_then(Value::as_str).unwrap_or_default().to_string();
            Some(leaf(
                Kind::Input(Input {
                    value: value_text,
                    placeholder,
                    cursor: number(value.get("cursor")).unwrap_or(0),
                    focused: bool_of(value.get("focused")),
                    multiline: bool_of(value.get("multiline")),
                }),
                color(value.get("background"), theme),
            ))
        }
        Some("select") => {
            let options = value
                .get("options")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(leaf(
                Kind::Select(Select {
                    options,
                    selected: number(value.get("selected")).unwrap_or(0),
                    focused: bool_of(value.get("focused")),
                }),
                color(value.get("background"), theme),
            ))
        }
        Some("box") => {
            let background = color(value.get("background"), theme);
            let node_color = color(value.get("color"), theme).or(inherited);
            let padding_base = number(value.get("padding")).unwrap_or(0);
            let margin_base = number(value.get("margin")).unwrap_or(0);
            let padding = Edges {
                top: number(value.get("paddingTop")).unwrap_or(padding_base),
                right: number(value.get("paddingRight")).unwrap_or(padding_base),
                bottom: number(value.get("paddingBottom")).unwrap_or(padding_base),
                left: number(value.get("paddingLeft")).unwrap_or(padding_base),
            };
            let margin = Edges {
                top: number(value.get("marginTop")).unwrap_or(margin_base),
                right: number(value.get("marginRight")).unwrap_or(margin_base),
                bottom: number(value.get("marginBottom")).unwrap_or(margin_base),
                left: number(value.get("marginLeft")).unwrap_or(margin_base),
            };
            let direction = match value.get("direction").and_then(Value::as_str) {
                Some("row") => Direction::Row,
                Some("row-reverse") => Direction::RowReverse,
                Some("column-reverse") => Direction::ColumnReverse,
                _ => Direction::Column,
            };
            let align = match value.get("align").and_then(Value::as_str) {
                Some("center") => Align::Center,
                Some("end") => Align::End,
                Some("stretch") => Align::Stretch,
                _ => Align::Stretch,
            };
            let justify = match value.get("justify").and_then(Value::as_str) {
                Some("center") => Justify::Center,
                Some("end") => Justify::End,
                Some("space-between") => Justify::SpaceBetween,
                Some("space-around") => Justify::SpaceAround,
                Some("space-evenly") => Justify::SpaceEvenly,
                _ => Justify::Start,
            };
            let children = value
                .get("children")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| parse(item, theme, depth + 1, node_color))
                        .collect()
                })
                .unwrap_or_default();
            Some(Node {
                kind: Kind::Box(children),
                direction,
                width: dim(value.get("width")),
                height: dim(value.get("height")),
                min_width: dim(value.get("minWidth")),
                max_width: dim(value.get("maxWidth")),
                min_height: dim(value.get("minHeight")),
                max_height: dim(value.get("maxHeight")),
                basis: dim(value.get("basis")),
                padding,
                margin,
                gap: number(value.get("gap")).unwrap_or(0).min(16),
                row_gap: number(value.get("rowGap")).unwrap_or(0).min(16),
                column_gap: number(value.get("columnGap")).unwrap_or(0).min(16),
                aspect_ratio: value.get("aspectRatio").and_then(Value::as_f64).map(|ratio| ratio as f32).filter(|ratio| *ratio > 0.0),
                grow: value.get("grow").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                shrink: value.get("shrink").and_then(Value::as_f64).unwrap_or(1.0) as f32,
                align_self: match value.get("alignSelf").and_then(Value::as_str) {
                    Some("center") => Some(Align::Center),
                    Some("end") | Some("flex-end") => Some(Align::End),
                    Some("stretch") => Some(Align::Stretch),
                    Some("start") | Some("flex-start") => Some(Align::Start),
                    _ => None,
                },
                wrap: value.get("wrap").and_then(Value::as_str) == Some("wrap"),
                overflow: value
                    .get("overflow")
                    .and_then(Value::as_str)
                    .is_some_and(|value| value != "visible"),
                scroll_top: number(value.get("scrollTop")).unwrap_or(0),
                scroll_left: number(value.get("scrollLeft")).unwrap_or(0),
                border: value
                    .get("border")
                    .and_then(Value::as_str)
                    .map(String::from),
                border_color: color(value.get("borderColor"), theme),
                title: value.get("title").and_then(Value::as_str).map(String::from),
                background,
                align,
                justify,
                position: match value.get("position").and_then(Value::as_str) {
                    Some("absolute") => Position::Absolute,
                    _ => Position::Relative,
                },
                left: dim(value.get("left")),
                top: dim(value.get("top")),
                right: dim(value.get("right")),
                bottom: dim(value.get("bottom")),
                hidden: bool_of(value.get("hidden")),
            })
        }
        _ => None,
    }
}

fn leaf(kind: Kind, background: Option<Rgba>) -> Node {
    Node {
        kind,
        direction: Direction::Column,
        width: None,
        height: None,
        min_width: None,
        max_width: None,
        min_height: None,
        max_height: None,
        basis: None,
        padding: Edges::default(),
        margin: Edges::default(),
        gap: 0,
        row_gap: 0,
        column_gap: 0,
        aspect_ratio: None,
        grow: 0.0,
        shrink: 1.0,
        align_self: None,
        wrap: false,
        overflow: false,
        scroll_top: 0,
        scroll_left: 0,
        border: None,
        border_color: None,
        title: None,
        background,
        align: Align::Start,
        justify: Justify::Start,
        position: Position::Relative,
        left: None,
        top: None,
        right: None,
        bottom: None,
        hidden: false,
    }
}

impl Node {
    fn gap_x(&self) -> usize {
        if self.column_gap > 0 { self.column_gap } else { self.gap }
    }

    fn gap_y(&self) -> usize {
        if self.row_gap > 0 { self.row_gap } else { self.gap }
    }

    fn outer_width(&self) -> usize {
        self.margin.left + self.margin.right
    }

    fn outer_height(&self) -> usize {
        self.margin.top + self.margin.bottom
    }

    fn intrinsic_width(&self, parent: usize) -> usize {
        let padding = self.padding.left + self.padding.right;
        let border = if self.border.is_some() { 2 } else { 0 };
        match &self.kind {
            Kind::Text(text) => {
                let mut widths = vec![0usize];
                for segment in &text.segments {
                    for (index, part) in segment.text.split('\n').enumerate() {
                        if index > 0 {
                            widths.push(0);
                        }
                        if let Some(width) = widths.last_mut() {
                            *width += draw::width(part);
                        }
                    }
                }
                widths
                    .into_iter()
                    .max()
                    .unwrap_or(0)
                    .min(parent.saturating_sub(padding + border).max(1))
            }
            Kind::Markdown(text) => text
                .lines()
                .map(draw::width)
                .max()
                .unwrap_or(0)
                .min(parent.saturating_sub(padding + border).max(1)),
            Kind::Code { text, .. } | Kind::Diff(text) => text
                .lines()
                .map(draw::width)
                .max()
                .unwrap_or(0)
                .min(parent.saturating_sub(padding + border).max(1)),
            Kind::Input(input) => draw::width(&input.value)
                .max(draw::width(&input.placeholder))
                .min(parent.saturating_sub(padding + border).max(1)),
            Kind::Select(select) => select
                .options
                .iter()
                .map(|option| draw::width(option) + 2)
                .max()
                .unwrap_or(0)
                .min(parent.saturating_sub(padding + border).max(1)),
            Kind::Box(children) => {
                let inner = parent
                    .saturating_sub(self.outer_width() + padding + border)
                    .max(1);
                let flow: Vec<&Node> = children
                    .iter()
                    .filter(|child| child.position == Position::Relative)
                    .collect();
                let mut width = 0usize;
                for child in &flow {
                    let child_width = child
                        .width
                        .filter(|value| !value.is_auto())
                        .map(|value| value.resolve(inner))
                        .unwrap_or_else(|| child.intrinsic_width(inner));
                    if self.direction.is_row() {
                        width = width.saturating_add(child_width + child.outer_width());
                    } else {
                        width = width.max(child_width + child.outer_width());
                    }
                }
                if self.direction.is_row() && flow.len() > 1 {
                    width = width.saturating_add((flow.len() - 1) * self.gap_x());
                }
                width + padding + border
            }
        }
    }

    fn content_width(&self, parent: usize, fill: bool) -> usize {
        let padding = self.padding.left + self.padding.right;
        let border = if self.border.is_some() { 2 } else { 0 };
        let inner = parent
            .saturating_sub(self.outer_width() + padding + border)
            .max(1);
        // A row whose children grow must claim the full line; without this a
        // synthetic row of empty flex segments (the token bar) measures zero.
        let grow_row = matches!(&self.kind, Kind::Box(children) if self.direction.is_row() && children.iter().any(|child| child.grow > 0.0));
        let natural = self
            .width
            .filter(|width| !width.is_auto())
            .map(|width| width.resolve(inner))
            .unwrap_or_else(|| {
                if fill || grow_row {
                    inner
                } else {
                    self.intrinsic_width(inner)
                }
            });
        let min = self
            .min_width
            .filter(|width| !width.is_auto())
            .map(|width| width.resolve(inner))
            .unwrap_or(0);
        let max = self
            .max_width
            .filter(|width| !width.is_auto())
            .map(|width| width.resolve(inner))
            .unwrap_or(usize::MAX);
        natural.max(min).min(max).min(inner.max(1))
    }

    fn measure(&self, parent_width: usize, fill: bool) -> Size {
        if self.hidden {
            return Size::default();
        }
        let content_width = self.content_width(parent_width, fill);
        let padding = self.padding.left + self.padding.right;
        let padding_v = self.padding.top + self.padding.bottom;
        let border = if self.border.is_some() { 2 } else { 0 };
        let inner_width = content_width.max(1);
        let mut size = match &self.kind {
            Kind::Text(text) => {
                let spans = text_spans(text, self.background.unwrap_or(Rgba::TRANSPARENT));
                let lines = draw::wrap(&spans, inner_width, text.wrap);
                let widest = lines
                    .iter()
                    .map(|spans| {
                        spans
                            .iter()
                            .map(|span| draw::width(&span.content))
                            .sum::<usize>()
                    })
                    .max()
                    .unwrap_or(0);
                Size {
                    width: content_width.max(widest.min(inner_width)),
                    height: lines.len().max(1),
                }
            }
            Kind::Markdown(text) => Size {
                width: content_width,
                height: wrapped_line_count(text, inner_width),
            },
            Kind::Code { text, .. } | Kind::Diff(text) => Size {
                width: content_width,
                height: text.lines().count().max(1),
            },
            Kind::Input(input) => Size {
                width: content_width,
                height: if input.multiline { input.value.lines().count().max(1) } else { 1 },
            },
            Kind::Select(select) => Size {
                width: content_width,
                height: select.options.len().max(1),
            },
            Kind::Box(children) => {
                if self.direction.is_row() && self.wrap {
                    let mut line_width = 0usize;
                    let mut line_height = 0usize;
                    let mut total_height = 0usize;
                    let mut width = 0usize;
                    for child in children.iter().filter(|child| child.position == Position::Relative) {
                        let child_size = child.measure(inner_width, false);
                        let child_width = child_size.width + child.outer_width();
                        let child_height = child_size.height + child.outer_height();
                        if line_width > 0 && line_width + child_width > inner_width {
                            total_height += line_height + self.gap_y();
                            line_width = 0;
                            line_height = 0;
                        }
                        line_width += child_width + self.gap_x();
                        line_height = line_height.max(child_height);
                        width = width.max(line_width.saturating_sub(self.gap_x()));
                    }
                    total_height += line_height;
                    return Size {
                        width: content_width.max(width + padding + border) + self.outer_width(),
                        height: total_height + padding_v + border + self.outer_height(),
                    };
                }
                let mut width = 0usize;
                let mut height = 0usize;
                let mut flow = 0usize;
                for child in children
                    .iter()
                    .filter(|child| child.position == Position::Relative)
                {
                    flow += 1;
                    let mut child_size =
                        child.measure(inner_width, self.direction.is_column());
                    if let Some(basis) = child.basis.filter(|basis| !basis.is_auto()) {
                        if self.direction.is_row() {
                            child_size.width = basis.resolve(inner_width) + child.outer_width();
                        } else {
                            child_size.height = basis.resolve(inner_width) + child.outer_height();
                        }
                    }
                    if self.direction.is_row() {
                        width = width.saturating_add(child_size.width + child.outer_width());
                        height = height.max(child_size.height + child.outer_height());
                    } else {
                        width = width.max(child_size.width + child.outer_width());
                        height = height.saturating_add(child_size.height + child.outer_height());
                    }
                }
                if flow > 1 {
                    let gaps = (flow - 1) * if self.direction.is_row() { self.gap_x() } else { self.gap_y() };
                    if self.direction.is_row() {
                        width = width.saturating_add(gaps);
                    } else {
                        height = height.saturating_add(gaps);
                    }
                }
                Size {
                    width: content_width.max(width + padding + border),
                    height: height + padding_v + border,
                }
            }
        };
        size.width = size.width.saturating_add(self.outer_width());
        size.height = size.height.saturating_add(self.outer_height());
        if let Some(ratio) = self.aspect_ratio {
            if self.height.is_none_or(Dim::is_auto) {
                let width = size.width.saturating_sub(self.outer_width()).max(1);
                size.height = (width as f32 / ratio).round().max(1.0) as usize + self.outer_height();
            }
        }
        if let Some(height) = self.height.filter(|height| !height.is_auto()) {
            size.height = height.resolve(size.height.max(1)).max(1) + self.outer_height();
        }
        let min_height = self
            .min_height
            .filter(|dim| !dim.is_auto())
            .map(|dim| dim.resolve(size.height))
            .unwrap_or(0);
        let max_height = self
            .max_height
            .filter(|dim| !dim.is_auto())
            .map(|dim| dim.resolve(size.height))
            .unwrap_or(usize::MAX);
        size.height = size.height.max(min_height).min(max_height);
        if let Some(width) = self.width.filter(|width| !width.is_auto()) {
            size.width = width.resolve(parent_width).max(1) + self.outer_width();
        }
        size
    }

    fn render(&self, buffer: &mut Buffer, rect: Rect, theme: &Theme, inherited_bg: Rgba) {
        if self.hidden || rect.width == 0 || rect.height == 0 {
            return;
        }
        let background = self
            .background
            .map(|color| color.over(inherited_bg))
            .unwrap_or(inherited_bg);
        draw::fill(buffer, rect, background);
        let mut area = rect;
        if self.border.is_some() && area.width > 1 && area.height > 1 {
            draw_border(buffer, area, self, theme, background);
            area = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
        }
        area = Rect {
            x: area.x + self.padding.left.min(area.width as usize) as u16,
            y: area.y + self.padding.top.min(area.height as usize) as u16,
            width: area
                .width
                .saturating_sub((self.padding.left + self.padding.right) as u16),
            height: area
                .height
                .saturating_sub((self.padding.top + self.padding.bottom) as u16),
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.overflow {
            if let Kind::Box(children) = &self.kind {
                let flow: Vec<&Node> = children.iter().filter(|child| child.position == Position::Relative).collect();
                let content_height = flow
                    .iter()
                    .map(|child| child.measure(area.width as usize, true).height)
                    .sum::<usize>()
                    + self.gap_y().saturating_mul(flow.len().saturating_sub(1));
                let content_width = if self.direction.is_row() {
                    flow.iter()
                        .map(|child| child.measure(area.width as usize, false).width + child.outer_width())
                        .sum::<usize>()
                        + self.gap_x().saturating_mul(flow.len().saturating_sub(1))
                } else {
                    flow.iter()
                        .map(|child| child.measure(area.width as usize, false).width + child.outer_width())
                        .max()
                        .unwrap_or(0)
                };
                let content_height = content_height.max(area.height as usize) as u16;
                let content_width = content_width.max(area.width as usize) as u16;
                let mut temp = Buffer::empty(Rect::new(0, 0, content_width, content_height));
                draw::fill(&mut temp, Rect::new(0, 0, content_width, content_height), background);
                self.render_children(
                    &mut temp,
                    Rect::new(0, 0, content_width, content_height),
                    theme,
                    background,
                    children,
                );
                for y in 0..area.height {
                    for x in 0..area.width {
                        let sx = x as usize + self.scroll_left;
                        let sy = y as usize + self.scroll_top;
                        if sx >= area.width as usize || sy >= temp.area.height as usize {
                            continue;
                        }
                        buffer[(area.x + x, area.y + y)] = temp[(sx as u16, sy as u16)].clone();
                    }
                }
                if content_height > area.height && area.width > 1 {
                    let track = area.height as usize;
                    let thumb = ((track * track) / content_height as usize).clamp(1, track);
                    let max_scroll = (content_height as usize).saturating_sub(track);
                    let offset = if max_scroll == 0 {
                        0
                    } else {
                        (self.scroll_top * (track - thumb)) / max_scroll
                    };
                    let color = theme.base.muted.over(background);
                    let style = Style::default().fg(color.color()).bg(background.color());
                    for row in 0..track {
                        let glyph = if row >= offset && row < offset + thumb { "█" } else { "│" };
                        set(buffer, area.x + area.width - 1, area.y + row as u16, glyph, style);
                    }
                }
                if content_width > area.width && area.height > 1 {
                    let track = area.width as usize;
                    let thumb = ((track * track) / content_width as usize).clamp(1, track);
                    let max_scroll = (content_width as usize).saturating_sub(track);
                    let offset = if max_scroll == 0 { 0 } else { (self.scroll_left * (track - thumb)) / max_scroll };
                    let color = theme.base.muted.over(background);
                    let style = Style::default().fg(color.color()).bg(background.color());
                    for column in 0..track {
                        let glyph = if column >= offset && column < offset + thumb { "█" } else { "─" };
                        set(buffer, area.x + column as u16, area.y + area.height - 1, glyph, style);
                    }
                }
            }
            return;
        }
        match &self.kind {
            Kind::Text(text) => {
                let spans = text_spans(text, background);
                let lines = draw::wrap(&spans, area.width as usize, text.wrap);
                for (index, mut spans) in lines.into_iter().enumerate() {
                    if index >= area.height as usize {
                        break;
                    }
                    let used = spans
                        .iter()
                        .map(|span| draw::width(&span.content))
                        .sum::<usize>();
                    let offset = match self.align {
                        Align::Center => (area.width as usize).saturating_sub(used) / 2,
                        Align::End => (area.width as usize).saturating_sub(used),
                        _ => 0,
                    };
                    if offset > 0 {
                        let mut padded = vec![Span::styled(
                            " ".repeat(offset),
                            Style::default().bg(background.color()),
                        )];
                        padded.append(&mut spans);
                        spans = padded;
                    }
                    let line = draw::line(spans, background);
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Markdown(text) => {
                let lines = markdown::render(text, area.width as usize, theme, background);
                for (index, line) in lines.into_iter().enumerate().take(area.height as usize) {
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Code { text, lang } => {
                let lines = highlight::text(text, lang, &theme.syntax, theme.base.text);
                for (index, runs) in lines.into_iter().take(area.height as usize).enumerate() {
                    let spans = runs
                        .into_iter()
                        .map(|(text, color)| draw::span(text, color, background))
                        .collect::<Vec<_>>();
                    let line = draw::line(spans, background);
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Diff(text) => {
                for (index, source) in text.lines().enumerate().take(area.height as usize) {
                    let color = if source.starts_with('+') {
                        theme.base.success.base
                    } else if source.starts_with('-') {
                        theme.base.error.base
                    } else if source.starts_with("@@") {
                        theme.base.info.base
                    } else {
                        theme.base.text
                    };
                    let line = draw::line(vec![draw::span(source.to_string(), color, background)], background);
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Input(input) => {
                let text = if input.value.is_empty() {
                    input.placeholder.clone()
                } else {
                    input.value.clone()
                };
                let color = if input.value.is_empty() {
                    theme.base.muted
                } else {
                    theme.base.text
                };
                let lines: Vec<String> = if input.multiline {
                    text.split('\n').map(String::from).collect()
                } else {
                    vec![text.clone()]
                };
                let mut cursor_line = 0;
                let mut cursor_col = 0;
                if input.focused {
                    let mut remaining = input.cursor;
                    for (index, line) in lines.iter().enumerate() {
                        let len = line.chars().count();
                        if remaining <= len {
                            cursor_line = index;
                            cursor_col = remaining;
                            break;
                        }
                        remaining = remaining.saturating_sub(len + 1);
                    }
                }
                for (index, line_text) in lines.iter().enumerate().take(area.height as usize) {
                    let mut spans = Vec::new();
                    if input.focused && index == cursor_line {
                        let before: String = line_text.chars().take(cursor_col).collect();
                        let at: String = line_text.chars().skip(cursor_col).take(1).collect();
                        let after: String = line_text.chars().skip(cursor_col + 1).collect();
                        if !before.is_empty() {
                            spans.push(draw::span(before, color, background));
                        }
                        spans.push(Span::styled(
                            if at.is_empty() { " ".to_string() } else { at },
                            draw::st(color, background).add_modifier(Modifier::REVERSED),
                        ));
                        if !after.is_empty() {
                            spans.push(draw::span(after, color, background));
                        }
                    } else {
                        spans.push(draw::span(line_text.clone(), color, background));
                    }
                    let line = draw::line(spans, background);
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Select(select) => {
                for (index, option) in select.options.iter().enumerate().take(area.height as usize) {
                    let selected = index == select.selected;
                    let prefix = if selected { "▸ " } else { "  " };
                    let color = if selected { theme.base.text } else { theme.base.muted };
                    let mut spans = vec![draw::span(format!("{prefix}{option}"), color, background)];
                    if selected && select.focused {
                        for span in &mut spans {
                            span.style = span.style.add_modifier(Modifier::BOLD);
                        }
                    }
                    let line = draw::line(spans, background);
                    draw::draw_line(buffer, area.x, area.y + index as u16, area.width, &line);
                }
            }
            Kind::Box(children) => self.render_children(buffer, area, theme, background, children),
        }
    }

    fn render_children(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        theme: &Theme,
        background: Rgba,
        children: &[Node],
    ) {
        let flow: Vec<&Node> = children
            .iter()
            .filter(|child| child.position == Position::Relative)
            .collect();
        let absolute: Vec<&Node> = children
            .iter()
            .filter(|child| child.position == Position::Absolute)
            .collect();
        if self.direction.reversed() {
            let mut reversed = flow.clone();
            reversed.reverse();
            if self.direction.is_row() {
                self.render_row(buffer, area, theme, background, &reversed);
            } else {
                self.render_column(buffer, area, theme, background, &reversed);
            }
        } else if self.direction.is_row() {
            self.render_row(buffer, area, theme, background, &flow);
        } else {
            self.render_column(buffer, area, theme, background, &flow);
        }
        for child in absolute {
            let width = child.content_width(area.width as usize, false) + child.outer_width();
            let height = child.measure(area.width as usize, false).height;
            let left = child.left.map(|left| left.resolve(area.width as usize));
            let right = child.right.map(|right| right.resolve(area.width as usize));
            let top = child.top.map(|top| top.resolve(area.height as usize));
            let bottom = child.bottom.map(|bottom| bottom.resolve(area.height as usize));
            let x = match (left, right) {
                (Some(left), _) => left + area.x as usize,
                (None, Some(right)) => (area.x as usize)
                    .saturating_add(area.width as usize)
                    .saturating_sub(width + right),
                _ => area.x as usize,
            };
            let y = match (top, bottom) {
                (Some(top), _) => top + area.y as usize,
                (None, Some(bottom)) => (area.y as usize)
                    .saturating_add(area.height as usize)
                    .saturating_sub(height + bottom),
                _ => area.y as usize,
            };
            child.render(
                buffer,
                Rect {
                    x: x.min(u16::MAX as usize) as u16,
                    y: y.min(u16::MAX as usize) as u16,
                    width: width.min(area.width as usize) as u16,
                    height: height.min(area.height as usize) as u16,
                },
                theme,
                background,
            );
        }
    }

    fn render_column(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        theme: &Theme,
        background: Rgba,
        children: &[&Node],
    ) {
        if children.is_empty() {
            return;
        }
        let available = area.height as usize;
        let mut sizes: Vec<Size> = children
            .iter()
            .map(|child| {
                let stretch = matches!(child.align_self.unwrap_or(self.align), Align::Stretch);
                let mut size = child.measure(area.width as usize, stretch);
                if let Some(basis) = child.basis.filter(|basis| !basis.is_auto()) {
                    size.height = basis.resolve(area.height as usize) + child.outer_height();
                }
                size
            })
            .collect();
        let mut total = sizes.iter().map(|size| size.height).sum::<usize>()
            + self.gap_y().saturating_mul(children.len().saturating_sub(1));
        if total > available {
            let overflow = total - available;
            let shrink_total: f32 = children.iter().map(|child| child.shrink.max(0.0)).sum();
            if shrink_total > 0.0 {
                for (index, child) in children.iter().enumerate() {
                    if child.shrink > 0.0 {
                        let take = (overflow as f32 * child.shrink / shrink_total).round() as usize;
                        let min = child.min_height.filter(|dim| !dim.is_auto()).map(|dim| dim.resolve(available)).unwrap_or(1);
                        sizes[index].height = sizes[index].height.saturating_sub(take).max(min);
                    }
                }
                total = sizes.iter().map(|size| size.height).sum::<usize>()
                    + self.gap_y().saturating_mul(children.len().saturating_sub(1));
            }
        }
        let grow_total: f32 = children.iter().map(|child| child.grow.max(0.0)).sum();
        let mut extra = available as isize - total as isize;
        if extra > 0 && grow_total > 0.0 {
            for (index, child) in children.iter().enumerate() {
                if child.grow > 0.0 {
                    let max = child.max_height.filter(|dim| !dim.is_auto()).map(|dim| dim.resolve(available)).unwrap_or(usize::MAX);
                    sizes[index].height = (sizes[index].height
                        + (extra as f32 * child.grow / grow_total).round() as usize)
                        .min(max);
                }
            }
            total = sizes.iter().map(|size| size.height).sum::<usize>()
                + self.gap_y().saturating_mul(children.len().saturating_sub(1));
            extra = available as isize - total as isize;
        }
        let _ = extra;
        let (mut y, gap) = flow_offset(
            area.y as usize,
            available,
            total,
            self.gap_y(),
            children.len(),
            self.justify,
        );
        for (index, child) in children.iter().enumerate() {
            let mut height = sizes[index].height;
            if height > available.saturating_sub(y.saturating_sub(area.y as usize)) {
                height = available.saturating_sub(y.saturating_sub(area.y as usize));
            }
            let outer_width = sizes[index].width.min(area.width as usize);
            let width = outer_width
                .saturating_sub(child.margin.left + child.margin.right)
                .max(1);
            if height == 0 || width == 0 {
                y = y.saturating_add(gap);
                continue;
            }
            let content_height = height
                .saturating_sub(child.margin.top + child.margin.bottom)
                .max(1);
            let x = match child.align_self.unwrap_or(self.align) {
                Align::Center => {
                    area.x as usize
                        + (area.width as usize).saturating_sub(outer_width) / 2
                        + child.margin.left
                }
                Align::End => {
                    area.x as usize
                        + (area.width as usize).saturating_sub(outer_width)
                        + child.margin.left
                }
                _ => area.x as usize + child.margin.left,
            };
            child.render(
                buffer,
                Rect {
                    x: x.min(u16::MAX as usize) as u16,
                    y: (y + child.margin.top).min(u16::MAX as usize) as u16,
                    width: width.min(area.width as usize) as u16,
                    height: content_height.min(available) as u16,
                },
                theme,
                background,
            );
            y = y.saturating_add(sizes[index].height).saturating_add(gap);
            let _ = index;
        }
    }

    fn render_row(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        theme: &Theme,
        background: Rgba,
        children: &[&Node],
    ) {
        if children.is_empty() {
            return;
        }
        if self.wrap {
            let available = area.width as usize;
            let mut x = area.x as usize;
            let mut y = area.y as usize;
            let mut line_height = 0usize;
            for child in children {
                let width = child
                    .width
                    .filter(|width| !width.is_auto())
                    .or(child.basis.filter(|basis| !basis.is_auto()))
                    .map(|width| width.resolve(available) + child.outer_width())
                    .unwrap_or_else(|| child.measure(available, false).width)
                    .min(available);
                if width == 0 {
                    continue;
                }
                if x > area.x as usize && x + width > area.x as usize + available {
                    y += line_height + self.gap_y();
                    x = area.x as usize;
                    line_height = 0;
                }
                let measured = child.measure(width, false);
                let height = measured.height.max(1);
                child.render(
                    buffer,
                    Rect {
                        x: (x + child.margin.left).min(u16::MAX as usize) as u16,
                        y: (y + child.margin.top).min(u16::MAX as usize) as u16,
                        width: width.saturating_sub(child.margin.left + child.margin.right).max(1) as u16,
                        height: height.saturating_sub(child.margin.top + child.margin.bottom).max(1) as u16,
                    },
                    theme,
                    background,
                );
                x += width + self.gap_x();
                line_height = line_height.max(height);
            }
            return;
        }
        let available = area.width as usize;
        let mut widths: Vec<usize> = children
            .iter()
            .map(|child| {
                let explicit = child
                    .width
                    .filter(|width| !width.is_auto())
                    .or(child.basis.filter(|basis| !basis.is_auto()))
                    .map(|width| width.resolve(available) + child.outer_width());
                explicit.unwrap_or_else(|| child.measure(available, false).width)
            })
            .collect();
        let mut total = widths.iter().sum::<usize>()
            + self.gap_x().saturating_mul(children.len().saturating_sub(1));
        if total > available {
            let overflow = total - available;
            let shrink_total: f32 = children.iter().map(|child| child.shrink.max(0.0)).sum();
            if shrink_total > 0.0 {
                for (index, child) in children.iter().enumerate() {
                    if child.shrink > 0.0 {
                        let take = (overflow as f32 * child.shrink / shrink_total).round() as usize;
                        let min = child.min_width.filter(|dim| !dim.is_auto()).map(|dim| dim.resolve(available)).unwrap_or(1);
                        let max = child.max_width.filter(|dim| !dim.is_auto()).map(|dim| dim.resolve(available)).unwrap_or(usize::MAX);
                        widths[index] = widths[index].saturating_sub(take).max(min).min(max);
                    }
                }
                total = widths.iter().sum::<usize>()
                    + self.gap_x().saturating_mul(children.len().saturating_sub(1));
            }
        }
        let grow_total: f32 = children.iter().map(|child| child.grow.max(0.0)).sum();
        let mut extra = available as isize - total as isize;
        if extra > 0 && grow_total > 0.0 {
            for (index, child) in children.iter().enumerate() {
                if child.grow > 0.0 {
                    let max = child.max_width.filter(|dim| !dim.is_auto()).map(|dim| dim.resolve(available)).unwrap_or(usize::MAX);
                    widths[index] = (widths[index] + (extra as f32 * child.grow / grow_total).round() as usize).min(max);
                }
            }
            total = widths.iter().sum::<usize>()
                + self.gap_x().saturating_mul(children.len().saturating_sub(1));
            extra = available as isize - total as isize;
        }
        let _ = extra;
        let (mut x, gap) = flow_offset(
            area.x as usize,
            available,
            total,
            self.gap_x(),
            children.len(),
            self.justify,
        );
        let row_height = area.height as usize;
        for (index, child) in children.iter().enumerate() {
            let width = widths[index].min(available);
            if width == 0 {
                x = x.saturating_add(gap);
                continue;
            }
            let measured = child.measure(width, false);
            let content_width = width
                .saturating_sub(child.margin.left + child.margin.right)
                .max(1);
            let stretch = matches!(child.align_self.unwrap_or(self.align), Align::Stretch)
                && child.height.is_none_or(Dim::is_auto);
            let height = if stretch { row_height } else { measured.height.min(row_height).max(1) };
            let content_height = height
                .saturating_sub(child.margin.top + child.margin.bottom)
                .max(1);
            let y = match child.align_self.unwrap_or(self.align) {
                Align::Center => area.y as usize + row_height.saturating_sub(height) / 2,
                Align::End => area.y as usize + row_height.saturating_sub(height),
                _ => area.y as usize + child.margin.top,
            };
            child.render(
                buffer,
                Rect {
                    x: (x + child.margin.left).min(u16::MAX as usize) as u16,
                    y: y.min(u16::MAX as usize) as u16,
                    width: content_width.min(available) as u16,
                    height: content_height.min(row_height) as u16,
                },
                theme,
                background,
            );
            x = x.saturating_add(width).saturating_add(gap);
            let _ = index;
        }
    }
}

fn flow_offset(
    start: usize,
    available: usize,
    total: usize,
    base_gap: usize,
    count: usize,
    justify: Justify,
) -> (usize, usize) {
    if count <= 1 {
        return (start, base_gap);
    }
    let extra = available.saturating_sub(total);
    let gap = match justify {
        Justify::SpaceBetween if extra > 0 => base_gap + extra / (count - 1),
        Justify::SpaceAround if extra > 0 => base_gap + extra / count,
        Justify::SpaceEvenly if extra > 0 => base_gap + extra / (count + 1),
        _ => base_gap,
    };
    let offset = match justify {
        Justify::Center => extra / 2,
        Justify::End => extra,
        Justify::SpaceAround if extra > 0 => extra / (count * 2),
        Justify::SpaceEvenly if extra > 0 => extra / (count + 1),
        _ => 0,
    };
    (start + offset, gap)
}

fn wrapped_line_count(text: &str, width: usize) -> usize {
    let width = width.max(1);
    text.lines()
        .map(|line| {
            let cells = draw::width(line).max(1);
            (cells + width - 1) / width
        })
        .sum::<usize>()
        .max(1)
}

fn text_spans(text: &Text, default_bg: Rgba) -> Vec<Span<'static>> {
    text.segments
        .iter()
        .map(|segment| {
            let bg = segment.bg.unwrap_or(default_bg);
            let fg = segment.fg.unwrap_or(Rgba::BLACK).over(bg);
            let mut style = Style::default().fg(fg.color()).bg(bg.color());
            if segment.bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            if segment.dim {
                style = style.add_modifier(Modifier::DIM);
            }
            if segment.italic {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if segment.underline {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if segment.strike {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            if segment.inverse {
                style = style.add_modifier(Modifier::REVERSED);
            }
            Span::styled(segment.text.clone(), style)
        })
        .collect()
}

fn draw_border(buffer: &mut Buffer, area: Rect, node: &Node, theme: &Theme, background: Rgba) {
    let color = node
        .border_color
        .unwrap_or(theme.base.border)
        .over(background);
    let style = Style::default().fg(color.color()).bg(background.color());
    let style_name = node.border.as_deref().unwrap_or("single");
    let (horizontal, vertical, top_left, top_right, bottom_left, bottom_right) = match style_name {
        "rounded" => ("─", "│", "╭", "╮", "╰", "╯"),
        "double" => ("═", "║", "╔", "╗", "╚", "╝"),
        "thick" | "heavy" => ("━", "┃", "┏", "┓", "┗", "┛"),
        _ => ("─", "│", "┌", "┐", "└", "┘"),
    };
    let right = area.x + area.width.saturating_sub(1);
    let bottom = area.y + area.height.saturating_sub(1);
    for x in area.x + 1..right {
        set(buffer, x, area.y, horizontal, style);
        set(buffer, x, bottom, horizontal, style);
    }
    for y in area.y + 1..bottom {
        set(buffer, area.x, y, vertical, style);
        set(buffer, right, y, vertical, style);
    }
    set(buffer, area.x, area.y, top_left, style);
    set(buffer, right, area.y, top_right, style);
    set(buffer, area.x, bottom, bottom_left, style);
    set(buffer, right, bottom, bottom_right, style);
    if let Some(title) = node.title.as_deref() {
        let title = draw::truncate(title, area.width.saturating_sub(4) as usize);
        let line = draw::line(vec![Span::styled(format!(" {title} "), style)], background);
        draw::draw_line(
            buffer,
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            &line,
        );
    }
}

fn set(buffer: &mut Buffer, x: u16, y: u16, symbol: &str, style: Style) {
    if x >= buffer.area.x + buffer.area.width || y >= buffer.area.y + buffer.area.height {
        return;
    }
    let cell = &mut buffer[(x, y)];
    cell.set_symbol(symbol);
    cell.set_style(style);
}

fn buffer_lines(buffer: &Buffer, width: u16, height: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for y in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut text = String::new();
        let mut current = Style::default();
        for x in 0..width {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            let symbol = if symbol.is_empty() { " " } else { symbol };
            let style = cell.style();
            if style != current && !text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut text), current));
            }
            current = style;
            text.push_str(symbol);
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, current));
        }
        out.push(Line::from(spans));
        if out.len() >= MAX_LINES {
            break;
        }
    }
    out
}

/// Interactive control exposed by a plugin layout.
#[derive(Clone, Debug)]
pub struct Control {
    pub path: Vec<usize>,
    pub kind: ControlKind,
    pub value: String,
    pub cursor: usize,
    pub options: Vec<String>,
    pub selected: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    Input,
    Select,
}

pub fn controls(layout: &Value) -> Vec<Control> {
    let mut out = Vec::new();
    collect_controls(layout, &mut Vec::new(), &mut out);
    out
}

fn collect_controls(value: &Value, path: &mut Vec<usize>, out: &mut Vec<Control>) {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return;
    };
    if kind == "input" {
        out.push(Control {
            path: path.clone(),
            kind: ControlKind::Input,
            value: value.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
            cursor: number(value.get("cursor")).unwrap_or_else(|| {
                value.get("value").and_then(Value::as_str).map(|value| value.chars().count()).unwrap_or(0)
            }),
            options: Vec::new(),
            selected: 0,
        });
    } else if kind == "select" {
        let options = value
            .get("options")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect())
            .unwrap_or_default();
        out.push(Control {
            path: path.clone(),
            kind: ControlKind::Select,
            value: String::new(),
            cursor: 0,
            options,
            selected: number(value.get("selected")).unwrap_or(0),
        });
    }
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for (index, child) in children.iter().enumerate() {
            path.push(index);
            collect_controls(child, path, out);
            path.pop();
        }
    }
}

fn node_at_mut<'a>(value: &'a mut Value, path: &[usize]) -> Option<&'a mut Value> {
    let mut current = value;
    for index in path {
        current = current.get_mut("children")?.get_mut(*index)?;
    }
    Some(current)
}

pub fn set_control_value(layout: &mut Value, index: usize, value: String) {
    let cursor = value.chars().count();
    set_control_value_at(layout, index, value, cursor);
}

pub fn set_control_value_at(layout: &mut Value, index: usize, value: String, cursor: usize) {
    let Some(control) = controls(layout).get(index).cloned() else {
        return;
    };
    if let Some(node) = node_at_mut(layout, &control.path) {
        node["value"] = Value::String(value);
        node["cursor"] = Value::from(cursor);
    }
}

pub fn set_control_cursor(layout: &mut Value, index: usize, cursor: usize) {
    let Some(control) = controls(layout).get(index).cloned() else {
        return;
    };
    if let Some(node) = node_at_mut(layout, &control.path) {
        node["cursor"] = Value::from(cursor);
    }
}

pub fn set_control_selected(layout: &mut Value, index: usize, selected: usize) {
    let Some(control) = controls(layout).get(index).cloned() else {
        return;
    };
    if let Some(node) = node_at_mut(layout, &control.path) {
        node["selected"] = Value::from(selected);
    }
}

pub fn set_focus(layout: &mut Value, focus: usize) {
    let mut current = 0;
    set_focus_inner(layout, focus, &mut current);
}

fn set_focus_inner(value: &mut Value, focus: usize, current: &mut usize) {
    let Some(kind) = value.get("type").and_then(Value::as_str).map(String::from) else {
        return;
    };
    if kind == "input" || kind == "select" {
        value["focused"] = Value::Bool(*current == focus);
        *current += 1;
    }
    if let Some(children) = value.get_mut("children").and_then(Value::as_array_mut) {
        for child in children {
            set_focus_inner(child, focus, current);
        }
    }
}

pub fn render_focused(
    layout: &Value,
    width: usize,
    theme: &Theme,
    background: Rgba,
    focus: usize,
) -> Vec<Line<'static>> {
    let mut copy = layout.clone();
    set_focus(&mut copy, focus);
    render(&copy, width, theme, background)
}

/// Render a plugin layout tree at `width` columns. `background` is the surface
/// the slot sits on, so boxes without an explicit background blend in.
pub fn render(layout: &Value, width: usize, theme: &Theme, background: Rgba) -> Vec<Line<'static>> {
    let width = width.max(1) as u16;
    let Some(node) = parse(layout, theme, 0, Some(theme.base.text)) else {
        return Vec::new();
    };
    let height = node
        .measure(width as usize, true)
        .height
        .max(1)
        .min(MAX_LINES);
    let mut buffer = Buffer::empty(Rect::new(0, 0, width, height as u16));
    draw::fill(
        &mut buffer,
        Rect::new(0, 0, width, height as u16),
        background,
    );
    node.render(
        &mut buffer,
        Rect::new(0, 0, width, height as u16),
        theme,
        background,
    );
    buffer_lines(&buffer, width, height as u16)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme;

    fn render_json(value: Value, width: usize) -> Vec<String> {
        let theme = theme::fallback(theme::Mode::Dark);
        render(&value, width, &theme, theme.base.bg)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn row_places_children_and_styles_text() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "row",
                "gap": 1,
                "children": [
                    { "type": "text", "text": "Context", "segments": [{ "text": "Context", "color": "#ff0000", "bold": true }] },
                    { "type": "text", "text": "72%" }
                ]
            }),
            20,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Context") && line.contains("72%")),
            "{lines:?}"
        );
    }

    #[test]
    fn column_keeps_children_on_separate_lines() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "column",
                "children": [
                    { "type": "text", "text": "one" },
                    { "type": "text", "text": "two" }
                ]
            }),
            12,
        );
        assert!(
            lines.iter().any(|line| line.trim_end() == "one"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.trim_end() == "two"),
            "{lines:?}"
        );
    }

    #[test]
    fn markdown_renders_headings_and_code() {
        let lines = render_json(
            serde_json::json!({
                "type": "markdown",
                "text": "# Heading\n\n```rust\nfn main() {}\n```"
            }),
            24,
        );
        assert!(lines.iter().any(|line| line.contains("Heading")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("fn main")), "{lines:?}");
    }

    #[test]
    fn controls_edit_inputs_and_selects() {
        let mut layout = serde_json::json!({
            "type": "box",
            "direction": "column",
            "children": [
                { "type": "input", "value": "hello", "placeholder": "name" },
                { "type": "select", "options": ["one", "two"], "selected": 0 }
            ]
        });
        let controls = controls(&layout);
        assert_eq!(controls.len(), 2);
        assert_eq!(controls[0].kind, ControlKind::Input);
        assert_eq!(controls[1].kind, ControlKind::Select);
        set_control_value(&mut layout, 0, "world".into());
        set_control_selected(&mut layout, 1, 1);
        let lines = render_json(layout, 20);
        assert!(lines.iter().any(|line| line.contains("world")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("two")), "{lines:?}");
    }

    #[test]
    fn overflow_clips_and_scrolls_children() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "height": 2,
                "overflow": "scroll",
                "scrollTop": 1,
                "children": [
                    { "type": "text", "text": "zero" },
                    { "type": "text", "text": "one" },
                    { "type": "text", "text": "two" },
                    { "type": "text", "text": "three" }
                ]
            }),
            10,
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("one"), "{lines:?}");
        assert!(lines[1].contains("two"), "{lines:?}");
    }

    #[test]
    fn row_wraps_children() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "row",
                "wrap": "wrap",
                "children": [
                    { "type": "text", "text": "aaaa" },
                    { "type": "text", "text": "bbbb" },
                    { "type": "text", "text": "cccc" }
                ]
            }),
            9,
        );
        assert!(lines.iter().any(|line| line.contains("aaaa") && line.contains("bbbb")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("cccc")), "{lines:?}");
        assert!(lines.len() >= 2, "{lines:?}");
    }

    #[test]
    fn basis_controls_row_widths() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "row",
                "children": [
                    { "type": "text", "text": "a", "basis": 3 },
                    { "type": "text", "text": "b", "basis": 3 }
                ]
            }),
            10,
        );
        assert!(lines.iter().any(|line| line.contains('a') && line.contains('b')), "{lines:?}");
    }

    #[test]
    fn background_applies_to_text_cells() {
        let theme = theme::fallback(theme::Mode::Dark);
        let lines = render(
            &serde_json::json!({
                "type": "box",
                "background": "#ff966c",
                "children": [{ "type": "text", "text": "X" }]
            }),
            6,
            &theme,
            theme.base.raised,
        );
        let colors: Vec<_> = lines[0].spans.iter().map(|span| span.style.bg).collect();
        assert!(colors.contains(&Some(ratatui::style::Color::Rgb(255, 150, 108))), "{colors:?}");
    }

    #[test]
    fn header_segments_keep_title_and_version_on_one_line() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "border": "single",
                "padding": 1,
                "width": "100%",
                "children": [{
                    "type": "box",
                    "direction": "row",
                    "justify": "space-between",
                    "children": [
                        {
                            "type": "box",
                            "paddingLeft": 1,
                            "paddingRight": 1,
                            "children": [{
                                "type": "text",
                                "text": "▼ Magic Context",
                                "segments": [
                                    { "text": "▼ " },
                                    { "text": "Magic Context" }
                                ]
                            }]
                        },
                        { "type": "text", "text": "v0.49.52" }
                    ]
                }]
            }),
            38,
        );
        assert!(
            lines.iter().any(|line| line.contains("Magic Context") && line.contains("v0.49.52")),
            "{lines:?}"
        );
    }

    #[test]
    fn border_and_padding_are_drawn() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "border": "rounded",
                "padding": 1,
                "children": [{ "type": "text", "text": "hi" }]
            }),
            10,
        );
        assert!(lines[0].starts_with('╭'), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("hi")), "{lines:?}");
    }

    #[test]
    fn aspect_ratio_sets_box_height() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "width": 10,
                "aspectRatio": 2,
                "background": "#123456",
                "children": []
            }),
            20,
        );
        assert_eq!(lines.len(), 5, "{lines:?}");
    }

    #[test]
    fn row_and_column_gaps_are_separate() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "column",
                "rowGap": 1,
                "children": [
                    { "type": "text", "text": "one" },
                    { "type": "text", "text": "two" }
                ]
            }),
            12,
        );
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[1].trim_end(), "", "{lines:?}");
        let row = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "row",
                "columnGap": 3,
                "children": [
                    { "type": "text", "text": "a" },
                    { "type": "text", "text": "b" }
                ]
            }),
            12,
        );
        assert!(row[0].contains("a   b"), "{row:?}");
    }

    #[test]
    fn auto_width_uses_content() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "direction": "row",
                "children": [
                    { "type": "text", "text": "x", "width": "auto" },
                    { "type": "text", "text": "y" }
                ]
            }),
            20,
        );
        assert_eq!(lines[0].trim_end(), "xy", "{lines:?}");
    }

    #[test]
    fn absolute_offsets_accept_percentages() {
        let lines = render_json(
            serde_json::json!({
                "type": "box",
                "width": 20,
                "height": 3,
                "children": [
                    { "type": "text", "text": "x", "position": "absolute", "left": "50%", "top": 1 }
                ]
            }),
            20,
        );
        assert!(lines[1].contains('x') && lines[1].find('x') == Some(10), "{lines:?}");
    }
}

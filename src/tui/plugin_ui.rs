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
    padding: Edges,
    margin: Edges,
    gap: usize,
    grow: f32,
    shrink: f32,
    border: Option<String>,
    border_color: Option<Rgba>,
    title: Option<String>,
    background: Option<Rgba>,
    align: Align,
    justify: Justify,
    position: Position,
    left: Option<usize>,
    top: Option<usize>,
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
    if let Some(color) = value.strip_prefix('#').and_then(Rgba::from_hex) {
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
                width: None,
                height: None,
                min_width: None,
                max_width: None,
                min_height: None,
                max_height: None,
                padding: Edges::default(),
                margin: Edges::default(),
                gap: 0,
                grow: 0.0,
                shrink: 1.0,
                border: None,
                border_color: None,
                title: None,
                background,
                align: Align::Start,
                justify: Justify::Start,
                position: Position::Relative,
                left: None,
                top: None,
                hidden: false,
            })
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
                _ => Direction::Column,
            };
            let align = match value.get("align").and_then(Value::as_str) {
                Some("center") => Align::Center,
                Some("end") => Align::End,
                Some("stretch") => Align::Stretch,
                _ => Align::Start,
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
                padding,
                margin,
                gap: number(value.get("gap")).unwrap_or(0).min(16),
                grow: value.get("grow").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                shrink: value.get("shrink").and_then(Value::as_f64).unwrap_or(1.0) as f32,
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
                left: number(value.get("left")),
                top: number(value.get("top")),
                hidden: bool_of(value.get("hidden")),
            })
        }
        _ => None,
    }
}

impl Node {
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
            Kind::Text(text) => text
                .segments
                .iter()
                .flat_map(|segment| segment.text.split('\n'))
                .map(draw::width)
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
                        .map(|value| value.resolve(inner))
                        .unwrap_or_else(|| child.intrinsic_width(inner));
                    if self.direction == Direction::Row {
                        width = width.saturating_add(child_width + child.outer_width());
                    } else {
                        width = width.max(child_width + child.outer_width());
                    }
                }
                if self.direction == Direction::Row && flow.len() > 1 {
                    width = width.saturating_add((flow.len() - 1) * self.gap);
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
        let natural = self
            .width
            .map(|width| width.resolve(inner))
            .unwrap_or_else(|| {
                if fill {
                    inner
                } else {
                    self.intrinsic_width(inner)
                }
            });
        let min = self
            .min_width
            .map(|width| width.resolve(inner))
            .unwrap_or(0);
        let max = self
            .max_width
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
        let border = if self.border.is_some() { 2 } else { 0 };
        let inner_width = content_width.saturating_sub(padding + border).max(1);
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
            Kind::Box(children) => {
                let mut width = 0usize;
                let mut height = 0usize;
                let mut flow = 0usize;
                for child in children
                    .iter()
                    .filter(|child| child.position == Position::Relative)
                {
                    flow += 1;
                    let child_size =
                        child.measure(inner_width, self.direction == Direction::Column);
                    if self.direction == Direction::Row {
                        width = width.saturating_add(child_size.width + child.outer_width());
                        height = height.max(child_size.height + child.outer_height());
                    } else {
                        width = width.max(child_size.width + child.outer_width());
                        height = height.saturating_add(child_size.height + child.outer_height());
                    }
                }
                if flow > 1 {
                    let gaps = (flow - 1) * self.gap;
                    if self.direction == Direction::Row {
                        width = width.saturating_add(gaps);
                    } else {
                        height = height.saturating_add(gaps);
                    }
                }
                Size {
                    width: content_width.max(width + padding + border),
                    height: height + padding + border,
                }
            }
        };
        size.width = size.width.saturating_add(self.outer_width());
        size.height = size.height.saturating_add(self.outer_height());
        if let Some(height) = self.height {
            size.height = height.resolve(size.height.max(1)).max(1) + self.outer_height();
        }
        let min_height = self
            .min_height
            .map(|dim| dim.resolve(size.height))
            .unwrap_or(0);
        let max_height = self
            .max_height
            .map(|dim| dim.resolve(size.height))
            .unwrap_or(usize::MAX);
        size.height = size.height.max(min_height).min(max_height);
        if let Some(width) = self.width {
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
        if self.direction == Direction::Row {
            self.render_row(buffer, area, theme, background, &flow);
        } else {
            self.render_column(buffer, area, theme, background, &flow);
        }
        for child in absolute {
            let width = child.content_width(area.width as usize, false) + child.outer_width();
            let x = child.left.unwrap_or(0) + (area.x as usize);
            let y = child.top.unwrap_or(0) + (area.y as usize);
            child.render(
                buffer,
                Rect {
                    x: x.min(u16::MAX as usize) as u16,
                    y: y.min(u16::MAX as usize) as u16,
                    width: width.min(area.width as usize) as u16,
                    height: child
                        .measure(area.width as usize, false)
                        .height
                        .min(area.height as usize) as u16,
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
            .map(|child| child.measure(area.width as usize, true))
            .collect();
        let mut total = sizes.iter().map(|size| size.height).sum::<usize>()
            + self.gap.saturating_mul(children.len().saturating_sub(1));
        if total > available {
            let overflow = total - available;
            let shrink_total: f32 = children.iter().map(|child| child.shrink.max(0.0)).sum();
            if shrink_total > 0.0 {
                for (index, child) in children.iter().enumerate() {
                    if child.shrink > 0.0 {
                        let take = (overflow as f32 * child.shrink / shrink_total).round() as usize;
                        sizes[index].height = sizes[index].height.saturating_sub(take).max(1);
                    }
                }
                total = sizes.iter().map(|size| size.height).sum::<usize>()
                    + self.gap.saturating_mul(children.len().saturating_sub(1));
            }
        }
        let grow_total: f32 = children.iter().map(|child| child.grow.max(0.0)).sum();
        let mut extra = available as isize - total as isize;
        if extra > 0 && grow_total > 0.0 {
            for (index, child) in children.iter().enumerate() {
                if child.grow > 0.0 {
                    sizes[index].height +=
                        (extra as f32 * child.grow / grow_total).round() as usize;
                }
            }
            total = sizes.iter().map(|size| size.height).sum::<usize>()
                + self.gap.saturating_mul(children.len().saturating_sub(1));
            extra = available as isize - total as isize;
        }
        let _ = extra;
        let (mut y, gap) = flow_offset(
            area.y as usize,
            available,
            total,
            self.gap,
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
            let x = match self.align {
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
        let available = area.width as usize;
        let mut widths: Vec<usize> = children
            .iter()
            .map(|child| {
                let explicit = child
                    .width
                    .map(|width| width.resolve(available) + child.outer_width());
                explicit.unwrap_or_else(|| child.measure(available, false).width)
            })
            .collect();
        let mut total = widths.iter().sum::<usize>()
            + self.gap.saturating_mul(children.len().saturating_sub(1));
        if total > available {
            let overflow = total - available;
            let shrink_total: f32 = children.iter().map(|child| child.shrink.max(0.0)).sum();
            if shrink_total > 0.0 {
                for (index, child) in children.iter().enumerate() {
                    if child.shrink > 0.0 {
                        let take = (overflow as f32 * child.shrink / shrink_total).round() as usize;
                        widths[index] = widths[index].saturating_sub(take).max(1);
                    }
                }
                total = widths.iter().sum::<usize>()
                    + self.gap.saturating_mul(children.len().saturating_sub(1));
            }
        }
        let grow_total: f32 = children.iter().map(|child| child.grow.max(0.0)).sum();
        let mut extra = available as isize - total as isize;
        if extra > 0 && grow_total > 0.0 {
            for (index, child) in children.iter().enumerate() {
                if child.grow > 0.0 {
                    widths[index] += (extra as f32 * child.grow / grow_total).round() as usize;
                }
            }
            total = widths.iter().sum::<usize>()
                + self.gap.saturating_mul(children.len().saturating_sub(1));
            extra = available as isize - total as isize;
        }
        let _ = extra;
        let (mut x, gap) = flow_offset(
            area.x as usize,
            available,
            total,
            self.gap,
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
            let height = measured.height.min(row_height).max(1);
            let content_height = height
                .saturating_sub(child.margin.top + child.margin.bottom)
                .max(1);
            let y = match self.align {
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
}

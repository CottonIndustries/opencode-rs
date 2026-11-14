//! Inline image previews using half-block cells. This is the terminal-safe
//! equivalent of upstream's media renderer when no native graphics protocol is
//! available.

use super::draw::line;
use super::theme::Rgba;
use base64::Engine;
use ratatui::text::{Line, Span};

fn color(pixel: image::Rgba<u8>) -> Rgba {
    Rgba { r: pixel[0] as f32 / 255.0, g: pixel[1] as f32 / 255.0, b: pixel[2] as f32 / 255.0, a: pixel[3] as f32 / 255.0 }
}

/// Render base64 image data as at most `max_rows` lines, `width` cells wide.
pub fn lines(data: &str, mime: &str, width: usize, max_rows: usize, bg: Rgba) -> Vec<Line<'static>> {
    if width == 0 || max_rows == 0 || !mime.starts_with("image/") {
        return Vec::new();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return Vec::new();
    };
    let Ok(source) = image::load_from_memory(&bytes) else {
        return Vec::new();
    };
    let ratio = source.height().max(1) as f32 / source.width().max(1) as f32;
    let rows = ((width as f32 * ratio / 2.0).round() as usize).clamp(1, max_rows);
    let height = rows * 2;
    let resized = source.resize_exact(width as u32, height as u32, image::imageops::FilterType::Triangle).to_rgba8();
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans = Vec::with_capacity(width);
        for x in 0..width {
            let top = resized.get_pixel(x as u32, (row * 2) as u32);
            let bottom = resized.get_pixel(x as u32, (row * 2 + 1) as u32);
            let style = ratatui::style::Style::default().fg(color(*top).color()).bg(color(*bottom).color());
            spans.push(Span::styled("\u{2580}", style));
        }
        out.push(line(spans, bg));
    }
    out
}

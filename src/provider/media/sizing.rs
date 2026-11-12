//! Image size helpers ported from opencode's image sizing protocol utility.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub multiple: u32,
    pub min_edge: Option<u32>,
    pub max_edge: Option<u32>,
    pub min_pixels: Option<f64>,
    pub max_pixels: Option<f64>,
    pub max_ratio: Option<f64>,
}

pub fn is_requested(aspect: Option<&str>, resolution: Option<&str>) -> bool {
    aspect.is_some() || resolution.is_some()
}

pub fn tier(resolution: &str) -> Option<f64> {
    let value = resolution.trim_end_matches(['k', 'K']);
    if value.len() == resolution.len() {
        return None;
    }
    value.parse::<f64>().ok()
}

fn aspect_ratio(aspect: Option<&str>) -> f64 {
    let value = aspect.unwrap_or("1:1");
    let (width, height) = value.split_once(':').unwrap_or(("1", "1"));
    width.parse::<f64>().unwrap_or(1.0) / height.parse::<f64>().unwrap_or(1.0)
}

fn target_pixels(resolution: Option<&str>) -> f64 {
    (tier(resolution.unwrap_or("1K")).unwrap_or(1.0) * 1024.0).powi(2)
}

pub fn fit(aspect: Option<&str>, resolution: Option<&str>, limits: Limits) -> Size {
    let max_ratio = limits.max_ratio.unwrap_or(f64::INFINITY);
    let ratio = aspect_ratio(aspect).clamp(1.0 / max_ratio, max_ratio);
    let area = target_pixels(resolution);
    let width = (area * ratio).sqrt();
    let height = (area / ratio).sqrt();
    let up = (limits.min_edge.unwrap_or(0) as f64 / width.min(height).max(f64::MIN_POSITIVE))
        .max((limits.min_pixels.unwrap_or(0.0) / area).sqrt());
    let scaled = up.max(1.0);
    let down = (limits.max_edge.unwrap_or(u32::MAX) as f64 / (width.max(height) * scaled))
        .min((limits.max_pixels.unwrap_or(f64::INFINITY) / area).sqrt() / scaled);
    let round = if down <= 1.0 {
        f64::floor
    } else if up >= 1.0 {
        f64::ceil
    } else {
        f64::round
    };
    let snap = |edge: f64| {
        let raw = edge * scaled * down.min(1.0) / limits.multiple as f64;
        let rounded = round((raw * 1_000_000.0).round() / 1_000_000.0) * limits.multiple as f64;
        rounded.max(limits.multiple as f64) as u32
    };
    Size { width: snap(width), height: snap(height) }
}

pub fn nearest(aspect: Option<&str>, resolution: Option<&str>, sizes: &[Size]) -> Option<Size> {
    let ratio = aspect_ratio(aspect).ln();
    let area = target_pixels(resolution).ln();
    let distance = |size: &Size| {
        (
            ((size.width as f64 / size.height as f64).ln() - ratio).abs(),
            ((size.width as f64 * size.height as f64).ln() - area).abs(),
        )
    };
    let mut best = sizes.first().copied()?;
    let mut best_distance = distance(&best);
    for size in sizes.iter().skip(1) {
        let candidate = distance(size);
        if (candidate.0 - best_distance.0).abs() > 1e-9 {
            if candidate.0 < best_distance.0 {
                best = *size;
                best_distance = candidate;
            }
        } else if candidate.1 < best_distance.1 {
            best = *size;
            best_distance = candidate;
        }
    }
    Some(best)
}

pub fn missed(aspect: Option<&str>, resolution: Option<&str>, size: Size) -> Option<String> {
    let aspect_missed = aspect.is_some_and(|aspect| {
        ((size.width as f64 / size.height as f64) / aspect_ratio(Some(aspect))).ln().abs() > 0.02
    });
    let resolution_missed = resolution.is_some_and(|resolution| {
        let Some(requested) = tier(resolution) else { return true };
        let actual = ((size.width as f64 * size.height as f64).sqrt() / (requested * 1024.0)).log2().abs();
        actual > 0.5
    });
    if !aspect_missed && !resolution_missed {
        return None;
    }
    Some(
        [aspect, resolution]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" at "),
    )
}

pub fn describe(size: Size) -> String {
    let divisor = gcd(size.width, size.height);
    let width = size.width / divisor;
    let height = size.height / divisor;
    let ratio = if width <= 32 && height <= 32 {
        format!("{width}:{height}")
    } else {
        format!("{:.2}:1", size.width as f64 / size.height as f64)
    };
    format!("{}x{} ({ratio})", size.width, size.height)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_and_fit() {
        assert_eq!(tier("1K"), Some(1.0));
        assert_eq!(tier("0.5k"), Some(0.5));
        assert_eq!(tier("hd"), None);
        let size = fit(Some("16:9"), Some("1K"), Limits { multiple: 16, ..Default::default() });
        assert!(size.width % 16 == 0 && size.height % 16 == 0);
        assert!(size.width > size.height);
    }

    #[test]
    fn nearest_prefers_aspect() {
        let sizes = [Size { width: 1024, height: 1024 }, Size { width: 1792, height: 1024 }];
        assert_eq!(nearest(Some("16:9"), Some("1K"), &sizes), Some(Size { width: 1792, height: 1024 }));
    }
}

//! Theme engine, ported from upstream's `@opencode/theme/tui`: v2 theme documents (hue scales,
//! semantic tokens, dialog surfaces) and the v1 → v2 migration, so built-in and custom themes
//! resolve to the same colors as the TypeScript client.

use ratatui::style::Color;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// A color with float channels in 0..=1, stored as `f32` like opentui's `RGBA`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };

    pub fn from_ints(r: u8, g: u8, b: u8, a: u8) -> Rgba {
        Rgba { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    pub fn from_hex(s: &str) -> Option<Rgba> {
        let h = s.strip_prefix('#')?;
        let digit = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|v| v * 17);
        let pair = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        match h.len() {
            3 => Some(Rgba::from_ints(digit(0)?, digit(1)?, digit(2)?, 255)),
            4 => Some(Rgba::from_ints(digit(0)?, digit(1)?, digit(2)?, digit(3)?)),
            6 => Some(Rgba::from_ints(pair(0)?, pair(2)?, pair(4)?, 255)),
            8 => Some(Rgba::from_ints(pair(0)?, pair(2)?, pair(4)?, pair(6)?)),
            _ => None,
        }
    }

    pub fn to_ints(self) -> [u8; 4] {
        let c = |v: f32| ((v as f64) * 255.0).round().clamp(0.0, 255.0) as u8;
        [c(self.r), c(self.g), c(self.b), c(self.a)]
    }

    pub fn with_alpha(self, a: f32) -> Rgba {
        Rgba { a, ..self }
    }

    /// The color as an opaque terminal color (alpha is resolved by [`Rgba::over`]).
    pub fn color(self) -> Color {
        let [r, g, b, _] = self.to_ints();
        Color::Rgb(r, g, b)
    }

    /// Composite this (possibly translucent) color over an opaque background.
    pub fn over(self, bg: Rgba) -> Rgba {
        if self.a >= 1.0 {
            return self;
        }
        tint(bg, self.with_alpha(1.0), self.a as f64)
    }

    pub fn hex(self) -> String {
        let [r, g, b, a] = self.to_ints();
        if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }

    pub fn same(self, o: Rgba) -> bool {
        self.to_ints() == o.to_ints()
    }
}

/// `theme/color.ts` tint: linear mix in sRGB, rounded to bytes.
pub fn tint(base: Rgba, overlay: Rgba, alpha: f64) -> Rgba {
    let mix = |b: f32, o: f32| (b as f64 + (o as f64 - b as f64) * alpha) * 255.0;
    let c = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    Rgba::from_ints(c(mix(base.r, overlay.r)), c(mix(base.g, overlay.g)), c(mix(base.b, overlay.b)), 255)
}

pub fn color_of(c: Color) -> Rgba {
    match c {
        Color::Rgb(r, g, b) => Rgba::from_ints(r, g, b, 255),
        _ => Rgba::BLACK,
    }
}

const STEPS: [u32; 9] = [100, 200, 300, 400, 500, 600, 700, 800, 900];
const STATES: [&str; 5] = ["disabled", "pressed", "focused", "selected", "hovered"];
const VARIANTS: [&str; 3] = ["primary", "secondary", "destructive"];

type Scale = [Rgba; 9];

/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Stateful {
    pub base: Rgba,
    pub disabled: Rgba,
    pub pressed: Rgba,
    pub focused: Rgba,
    pub selected: Rgba,
    pub hovered: Rgba,
}

/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Feedback {
    pub base: Rgba,
    pub muted: Rgba,
}

/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Diff {
    pub added: Rgba,
    pub removed: Rgba,
    pub context: Rgba,
    pub hunk_header: Rgba,
    pub bg_added: Rgba,
    pub bg_removed: Rgba,
    pub bg_context: Rgba,
    pub highlight_added: Rgba,
    pub highlight_removed: Rgba,
    pub line_number: Rgba,
    pub line_number_bg_added: Rgba,
    pub line_number_bg_removed: Rgba,
}

/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Syntax {
    pub comment: Rgba,
    pub keyword: Rgba,
    pub function: Rgba,
    pub variable: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub ty: Rgba,
    pub operator: Rgba,
    pub punctuation: Rgba,
}

/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Markdown {
    pub text: Rgba,
    pub heading: Rgba,
    pub link: Rgba,
    pub link_text: Rgba,
    pub code: Rgba,
    pub block_quote: Rgba,
    pub emphasis: Rgba,
    pub strong: Rgba,
    pub horizontal_rule: Rgba,
    pub list_item: Rgba,
    pub list_enumeration: Rgba,
    pub image: Rgba,
    pub image_text: Rgba,
    pub code_block: Rgba,
}

/// Resolved semantic tokens for one surface.
/// Part of upstream's complete token surface; not every token has a consumer yet.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    pub text: Rgba,
    pub muted: Rgba,
    pub faint: Rgba,
    pub text_primary: Stateful,
    pub text_secondary: Stateful,
    pub text_destructive: Stateful,
    pub text_formfield: Stateful,
    pub error: Feedback,
    pub warning: Feedback,
    pub success: Feedback,
    pub info: Feedback,
    pub bg: Rgba,
    pub raised: Rgba,
    pub raised_high: Rgba,
    pub raised_max: Rgba,
    pub bg_primary: Stateful,
    pub bg_secondary: Stateful,
    pub bg_destructive: Stateful,
    pub bg_formfield: Stateful,
    pub border: Rgba,
    pub scrollbar: Rgba,
    pub diff: Diff,
    pub syntax: Syntax,
    pub markdown: Markdown,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub mode: Mode,
    pub base: Tokens,
    pub dialog: Tokens,
    pub dialog_footer: Tokens,
    hue: Vec<(String, Scale)>,
    pub categorical: Vec<Scale>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }
}

impl std::ops::Deref for Theme {
    type Target = Tokens;
    fn deref(&self) -> &Tokens {
        &self.base
    }
}

impl Theme {
    /// Move a hue-scale color `amount` steps toward 900 (`increase`) or 100 (`decrease`).
    /// Colors outside every hue scale are returned unchanged, as upstream does.
    pub fn shift(&self, color: Rgba, amount: i32) -> Rgba {
        // Semantic aliases first: upstream references prefer accent/interactive/neutral scales.
        let order = self.hue.iter().rev();
        for (_, scale) in order {
            if let Some(i) = scale.iter().position(|c| *c == color) {
                let j = (i as i32 + amount).clamp(0, 8) as usize;
                return scale[j];
            }
        }
        color
    }
    pub fn decrease(&self, color: Rgba) -> Rgba {
        self.shift(color, -1)
    }
    pub fn hue(&self, name: &str, step: u32) -> Rgba {
        let i = STEPS.iter().position(|s| *s == step).unwrap_or(1);
        self.hue.iter().find(|(n, _)| n == name).map(|(_, s)| s[i]).unwrap_or(self.base.text)
    }
    /// Agent palette: categorical hues at step 200, deduplicated.
    pub fn agent_colors(&self) -> Vec<Rgba> {
        let mut out: Vec<Rgba> = Vec::new();
        for s in &self.categorical {
            if !out.iter().any(|c| c.same(s[1])) {
                out.push(s[1]);
            }
        }
        out
    }
}

// ---------------------------------------------------------------- registry

macro_rules! themes {
    ($($name:literal => $file:literal),* $(,)?) => {
        pub const BUILTIN: &[(&str, &str)] = &[$(($name, include_str!(concat!("../../assets/themes/", $file)))),*];
    };
}

themes! {
    "aura" => "aura.json",
    "ayu" => "ayu.json",
    "catppuccin" => "catppuccin.json",
    "catppuccin-frappe" => "catppuccin-frappe.json",
    "catppuccin-macchiato" => "catppuccin-macchiato.json",
    "cobalt2" => "cobalt2.json",
    "cursor" => "cursor.json",
    "dracula" => "dracula.json",
    "everforest" => "everforest.json",
    "flexoki" => "flexoki.json",
    "github" => "github.json",
    "gruvbox" => "gruvbox.json",
    "kanagawa" => "kanagawa.json",
    "material" => "material.json",
    "matrix" => "matrix.json",
    "mercury" => "mercury.json",
    "monokai" => "monokai.json",
    "nightowl" => "nightowl.json",
    "nord" => "nord.json",
    "one-dark" => "one-dark.json",
    "osaka-jade" => "osaka-jade.json",
    "orng" => "orng.json",
    "lucent-orng" => "lucent-orng.json",
    "palenight" => "palenight.json",
    "rosepine" => "rosepine.json",
    "solarized" => "solarized.json",
    "synthwave84" => "synthwave84.json",
    "tokyonight" => "tokyonight.json",
    "vesper" => "vesper.json",
    "vercel" => "vercel.json",
    "zenburn" => "zenburn.json",
    "carbonfox" => "carbonfox.json",
    "opencode" => "opencode.v2.json",
}

/// All theme sources by name: built-ins, then custom `themes/*.json` files (which win).
pub fn sources(dirs: &[std::path::PathBuf]) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = BUILTIN.iter().filter_map(|(n, s)| serde_json::from_str(s).ok().map(|v| (n.to_string(), v))).collect();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d.join("themes")) else { continue };
        let mut files: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        files.sort();
        for p in files {
            let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
            if v.get("theme").is_none() && v.get("base").is_none() {
                continue;
            }
            out.retain(|(n, _)| *n != name);
            out.push((name, v));
        }
    }
    out
}

/// Modes a theme source provides.
pub fn modes(source: &Value) -> Vec<Mode> {
    let doc = document(source);
    [Mode::Light, Mode::Dark].into_iter().filter(|m| doc.get(m.key()).is_some_and(|v| !v.is_null())).collect()
}

pub fn load(name: &str, source: &Value, mode: Mode) -> Result<Theme, String> {
    let doc = document(source);
    let available = modes(source);
    let mode = if available.contains(&mode) { mode } else { *available.first().ok_or("theme must provide at least one mode")? };
    let merged = merge(&[doc.get("base").unwrap_or(&Value::Null), doc.get(mode.key()).unwrap_or(&Value::Null)]);
    let def = expand_theme(&merged);
    let hue = resolve_hue(def.get("hue").ok_or("theme is missing hue")?)?;
    let categorical = def
        .get("categorical")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|n| n.as_str()).filter_map(|n| hue.iter().find(|(h, _)| h == n).map(|(_, s)| *s)).collect())
        .unwrap_or_default();
    let base_tokens = token_subset(&def);
    let dialog = def.get("@dialog");
    let dialog_tokens = match dialog {
        Some(d) => contextualize(&base_tokens, &token_subset(d)),
        None => base_tokens.clone(),
    };
    let footer_tokens = match dialog.and_then(|d| d.get("@footer")) {
        Some(f) => contextualize(&dialog_tokens, &token_subset(f)),
        None => dialog_tokens.clone(),
    };
    Ok(Theme {
        name: name.to_string(),
        mode,
        base: resolve_tokens(&base_tokens, &hue)?,
        dialog: resolve_tokens(&dialog_tokens, &hue)?,
        dialog_footer: resolve_tokens(&footer_tokens, &hue)?,
        hue,
        categorical,
    })
}

/// The default `opencode` theme, which always resolves.
pub fn fallback(mode: Mode) -> Theme {
    let src: Value = serde_json::from_str(BUILTIN.iter().find(|(n, _)| *n == "opencode").unwrap().1).unwrap();
    load("opencode", &src, mode).expect("built-in theme")
}

fn document(source: &Value) -> Value {
    if source.get("theme").is_some() {
        migrate_v1(source)
    } else {
        source.clone()
    }
}

// ---------------------------------------------------------------- v2 resolution

fn is_record(v: &Value) -> bool {
    v.is_object()
}

/// `mergeTheme`: recursive object merge; later values win, nulls are skipped.
fn merge(values: &[&Value]) -> Value {
    let mut out = Map::new();
    for v in values {
        let Some(obj) = v.as_object() else { continue };
        for (k, item) in obj {
            if item.is_null() {
                continue;
            }
            let next = if is_record(item) { merge(&[out.get(k).unwrap_or(&Value::Null), item]) } else { item.clone() };
            out.insert(k.clone(), next);
        }
    }
    Value::Object(out)
}

fn expand_theme(def: &Value) -> Value {
    let mut out = def.as_object().cloned().unwrap_or_default();
    for (k, v) in expand_tokens(def).as_object().cloned().unwrap_or_default() {
        out.insert(k, v);
    }
    if let Some(d) = def.get("@dialog") {
        let mut dialog = expand_tokens(d).as_object().cloned().unwrap_or_default();
        if let Some(f) = d.get("@footer") {
            dialog.insert("@footer".into(), expand_tokens(f));
        }
        out.insert("@dialog".into(), Value::Object(dialog));
    }
    Value::Object(out)
}

fn expand_tokens(def: &Value) -> Value {
    let mut out = def.as_object().cloned().unwrap_or_default();
    if let Some(text) = def.get("text").and_then(|t| t.as_object()) {
        let mut t = text.clone();
        let has = |k: &str| text.get(k).is_some_and(|v| !v.is_null());
        if !has("muted") && has("base") {
            t.insert("muted".into(), json!("$text.base"));
        }
        if !has("faint") && (has("muted") || has("base")) {
            t.insert("faint".into(), json!("$text.muted"));
        }
        if let Some(a) = text.get("action") {
            t.insert("action".into(), expand_actions(a, "text.action"));
        }
        if let Some(f) = text.get("formfield") {
            t.insert("formfield".into(), expand_formfield(f, "text.formfield"));
        }
        if let Some(fb) = text.get("feedback").and_then(|f| f.as_object()) {
            let mut kinds = Map::new();
            for (kind, item) in fb {
                let mut entry = item.as_object().cloned().unwrap_or_default();
                if entry.get("muted").is_none_or(|v| v.is_null()) && entry.get("base").is_some_and(|v| !v.is_null()) {
                    entry.insert("muted".into(), json!(format!("$text.feedback.{kind}.base")));
                }
                kinds.insert(kind.clone(), Value::Object(entry));
            }
            t.insert("feedback".into(), Value::Object(kinds));
        }
        out.insert("text".into(), Value::Object(t));
    }
    if let Some(bg) = def.get("background").and_then(|t| t.as_object()) {
        let mut b = bg.clone();
        if let Some(a) = bg.get("action") {
            b.insert("action".into(), expand_actions(a, "background.action"));
        }
        if let Some(f) = bg.get("formfield") {
            b.insert("formfield".into(), expand_formfield(f, "background.formfield"));
        }
        out.insert("background".into(), Value::Object(b));
    }
    Value::Object(out)
}

fn expand_formfield(def: &Value, path: &str) -> Value {
    let Some(obj) = def.as_object() else { return def.clone() };
    if obj.get("base").is_none_or(|v| v.is_null()) {
        return def.clone();
    }
    let mut out = obj.clone();
    for s in STATES {
        let k = format!("${s}");
        if out.get(&k).is_none_or(|v| v.is_null()) {
            out.insert(k, json!(format!("${path}.base")));
        }
    }
    Value::Object(out)
}

fn expand_actions(def: &Value, path: &str) -> Value {
    let Some(obj) = def.as_object() else { return def.clone() };
    let mut out = Map::new();
    for (variant, value) in obj {
        match value.as_object() {
            Some(v) if v.get("base").is_some_and(|b| !b.is_null()) => {
                let mut e = v.clone();
                for s in STATES {
                    let k = format!("${s}");
                    if e.get(&k).is_none_or(|x| x.is_null()) {
                        e.insert(k, json!(format!("${path}.{variant}.base")));
                    }
                }
                out.insert(variant.clone(), Value::Object(e));
            }
            _ => {
                out.insert(variant.clone(), value.clone());
            }
        }
    }
    Value::Object(out)
}

fn token_subset(def: &Value) -> Value {
    let mut out = Map::new();
    for k in ["text", "background", "border", "scrollbar", "diff", "syntax", "markdown"] {
        if let Some(v) = def.get(k) {
            out.insert(k.into(), v.clone());
        }
    }
    Value::Object(out)
}

fn contextualize(base: &Value, over: &Value) -> Value {
    let mut result = merge(&[base, over]).as_object().cloned().unwrap_or_default();
    for group in ["text", "background"] {
        let b = base.get(group).and_then(|g| g.get("action"));
        let s = over.get(group).and_then(|g| g.get("action"));
        let mut variants = Map::new();
        for variant in VARIANTS {
            let bv = b.and_then(|x| x.get(variant));
            let sv = s.and_then(|x| x.get(variant));
            let pick = |key: Option<&str>| -> Value {
                let get = |v: Option<&Value>, k: &str| v.and_then(|x| x.get(k)).filter(|x| !x.is_null()).cloned();
                key.and_then(|k| get(sv, k))
                    .or_else(|| get(sv, "base"))
                    .or_else(|| key.and_then(|k| get(bv, k)))
                    .or_else(|| get(bv, "base"))
                    .unwrap_or(Value::Null)
            };
            let mut states = Map::new();
            states.insert("base".into(), pick(None));
            for st in STATES {
                let k = format!("${st}");
                states.insert(k.clone(), pick(Some(&k)));
            }
            variants.insert(variant.into(), Value::Object(states));
        }
        let mut g = result.get(group).and_then(|g| g.as_object()).cloned().unwrap_or_default();
        g.insert("action".into(), Value::Object(variants));
        result.insert(group.into(), Value::Object(g));
    }
    Value::Object(result)
}

fn resolve_hue(def: &Value) -> Result<Vec<(String, Scale)>, String> {
    let src = def.as_object().ok_or("invalid hue")?;
    let mut cache: HashMap<String, Scale> = HashMap::new();
    fn resolve(name: &str, src: &Map<String, Value>, cache: &mut HashMap<String, Scale>, stack: &mut Vec<String>) -> Result<Scale, String> {
        if let Some(s) = cache.get(name) {
            return Ok(*s);
        }
        if stack.iter().any(|s| s == name) {
            return Err(format!("circular hue reference: {name}"));
        }
        let value = src.get(name).ok_or_else(|| format!("hue \"{name}\" was not found"))?;
        let scale = if let Some(alias) = value.as_str() {
            let target = alias.strip_prefix("$hue.").ok_or_else(|| format!("hue alias \"{alias}\" must reference a hue scale"))?;
            stack.push(name.into());
            let s = resolve(target, src, cache, stack)?;
            stack.pop();
            s
        } else {
            let obj = value.as_object().ok_or_else(|| format!("hue \"{name}\" was not found"))?;
            let mut s = [Rgba::TRANSPARENT; 9];
            for (i, step) in STEPS.iter().enumerate() {
                let c = obj.get(&step.to_string()).and_then(|v| v.as_str()).and_then(Rgba::from_hex);
                s[i] = c.ok_or_else(|| format!("invalid hue color at hue.{name}.{step}"))?;
            }
            s
        };
        cache.insert(name.into(), scale);
        Ok(scale)
    }
    let mut out = Vec::new();
    for name in src.keys() {
        out.push((name.clone(), resolve(name, src, &mut cache, &mut Vec::new())?));
    }
    Ok(out)
}

struct Resolver<'a> {
    def: &'a Value,
    hue: &'a [(String, Scale)],
}

impl Resolver<'_> {
    fn read(&self, path: &str) -> Option<&Value> {
        path.split('.').try_fold(self.def, |v, k| v.get(k))
    }

    fn value(&self, v: &Value, stack: &mut Vec<String>) -> Result<Rgba, String> {
        let s = v.as_str().ok_or("invalid theme value")?;
        if s == "transparent" {
            return Ok(Rgba::TRANSPARENT);
        }
        if s.starts_with('#') {
            return Rgba::from_hex(s).ok_or_else(|| format!("invalid color {s}"));
        }
        let target = s.strip_prefix('$').ok_or_else(|| format!("invalid color \"{s}\""))?;
        if let Some(rest) = target.strip_prefix("hue.") {
            if let Some((name, step)) = rest.rsplit_once('.') {
                if let (Some(i), Some((_, scale))) = (STEPS.iter().position(|x| x.to_string() == step), self.hue.iter().find(|(n, _)| n == name)) {
                    return Ok(scale[i]);
                }
            }
            return Err(format!("theme reference \"{s}\" was not found"));
        }
        if stack.iter().any(|x| x == target) {
            return Err(format!("circular theme reference: {target}"));
        }
        let next = self.read(target).ok_or_else(|| format!("theme reference \"{s}\" was not found"))?;
        stack.push(target.to_string());
        let r = self.value(next, stack);
        stack.pop();
        r
    }

    fn get(&self, path: &str) -> Result<Rgba, String> {
        let v = self.read(path).ok_or_else(|| format!("theme token {path} is missing"))?;
        self.value(v, &mut Vec::new())
    }

    fn stateful(&self, path: &str) -> Result<Stateful, String> {
        let base = self.get(&format!("{path}.base"))?;
        let st = |s: &str| self.get(&format!("{path}.${s}")).or(Ok::<Rgba, String>(base));
        Ok(Stateful { base, disabled: st("disabled")?, pressed: st("pressed")?, focused: st("focused")?, selected: st("selected")?, hovered: st("hovered")? })
    }

    fn feedback(&self, kind: &str) -> Result<Feedback, String> {
        let base = self.get(&format!("text.feedback.{kind}.base"))?;
        Ok(Feedback { base, muted: self.get(&format!("text.feedback.{kind}.muted")).unwrap_or(base) })
    }
}

fn resolve_tokens(def: &Value, hue: &[(String, Scale)]) -> Result<Tokens, String> {
    let r = Resolver { def, hue };
    let g = |p: &str| r.get(p);
    Ok(Tokens {
        text: g("text.base")?,
        muted: g("text.muted")?,
        faint: g("text.faint").or_else(|_| g("text.muted"))?,
        text_primary: r.stateful("text.action.primary")?,
        text_secondary: r.stateful("text.action.secondary")?,
        text_destructive: r.stateful("text.action.destructive")?,
        text_formfield: r.stateful("text.formfield")?,
        error: r.feedback("error")?,
        warning: r.feedback("warning")?,
        success: r.feedback("success")?,
        info: r.feedback("info")?,
        bg: g("background.base")?,
        raised: g("background.raised.base")?,
        raised_high: g("background.raised.high")?,
        raised_max: g("background.raised.max")?,
        bg_primary: r.stateful("background.action.primary")?,
        bg_secondary: r.stateful("background.action.secondary")?,
        bg_destructive: r.stateful("background.action.destructive")?,
        bg_formfield: r.stateful("background.formfield")?,
        border: g("border.base")?,
        scrollbar: g("scrollbar.base")?,
        diff: Diff {
            added: g("diff.text.added")?,
            removed: g("diff.text.removed")?,
            context: g("diff.text.context")?,
            hunk_header: g("diff.text.hunkHeader")?,
            bg_added: g("diff.background.added")?,
            bg_removed: g("diff.background.removed")?,
            bg_context: g("diff.background.context")?,
            highlight_added: g("diff.highlight.added")?,
            highlight_removed: g("diff.highlight.removed")?,
            line_number: g("diff.lineNumber.text")?,
            line_number_bg_added: g("diff.lineNumber.background.added")?,
            line_number_bg_removed: g("diff.lineNumber.background.removed")?,
        },
        syntax: Syntax {
            comment: g("syntax.comment")?,
            keyword: g("syntax.keyword")?,
            function: g("syntax.function")?,
            variable: g("syntax.variable")?,
            string: g("syntax.string")?,
            number: g("syntax.number")?,
            ty: g("syntax.type")?,
            operator: g("syntax.operator")?,
            punctuation: g("syntax.punctuation")?,
        },
        markdown: Markdown {
            text: g("markdown.text")?,
            heading: g("markdown.heading")?,
            link: g("markdown.link")?,
            link_text: g("markdown.linkText")?,
            code: g("markdown.code")?,
            block_quote: g("markdown.blockQuote")?,
            emphasis: g("markdown.emphasis")?,
            strong: g("markdown.strong")?,
            horizontal_rule: g("markdown.horizontalRule")?,
            list_item: g("markdown.listItem")?,
            list_enumeration: g("markdown.listEnumeration")?,
            image: g("markdown.image")?,
            image_text: g("markdown.imageText")?,
            code_block: g("markdown.codeBlock")?,
        },
    })
}

// ---------------------------------------------------------------- OKLCH

#[derive(Debug, Clone, Copy)]
struct Oklch {
    l: f64,
    c: f64,
    h: f64,
}

fn srgb_to_linear(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(v: f64) -> f64 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

fn rgb_to_oklch(red: f64, green: f64, blue: f64) -> Oklch {
    let (lr, lg, lb) = (srgb_to_linear(red), srgb_to_linear(green), srgb_to_linear(blue));
    let l_ = (0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb).cbrt();
    let m_ = (0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb).cbrt();
    let s_ = (0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb).cbrt();
    let l = 0.2104542553 * l_ + 0.793617785 * m_ - 0.0040720468 * s_;
    let a = 1.9779984951 * l_ - 2.428592205 * m_ + 0.4505937099 * s_;
    let b = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.808675766 * s_;
    let c = (a * a + b * b).sqrt();
    let angle = b.atan2(a) * (180.0 / std::f64::consts::PI);
    Oklch { l, c, h: if angle < 0.0 { angle + 360.0 } else { angle } }
}

fn oklch_to_rgb(c: Oklch) -> (f64, f64, f64) {
    let a = c.c * (c.h * std::f64::consts::PI / 180.0).cos();
    let b = c.c * (c.h * std::f64::consts::PI / 180.0).sin();
    let l_ = c.l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = c.l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = c.l - 0.0894841775 * a - 1.291485548 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    (
        linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    )
}

fn in_gamut((r, g, b): (f64, f64, f64)) -> bool {
    (0.0..=1.0).contains(&r) && (0.0..=1.0).contains(&g) && (0.0..=1.0).contains(&b)
}

fn fit_oklch(c: Oklch) -> Oklch {
    let base = Oklch { l: c.l.clamp(0.0, 1.0), c: c.c.max(0.0), h: ((c.h % 360.0) + 360.0) % 360.0 };
    if in_gamut(oklch_to_rgb(base)) {
        return base;
    }
    for i in 0..24 {
        let next = Oklch { c: base.c * 0.9f64.powi(i + 1), ..base };
        if in_gamut(oklch_to_rgb(next)) {
            return next;
        }
    }
    Oklch { c: 0.0, ..base }
}

fn oklch_to_hex(c: Oklch) -> String {
    let (r, g, b) = oklch_to_rgb(fit_oklch(c));
    let h = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", h(r), h(g), h(b))
}

fn to_oklch(c: Rgba) -> Oklch {
    let [r, g, b, _] = c.to_ints();
    rgb_to_oklch(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

// ---------------------------------------------------------------- v1 migration

const CHROMATIC: [&str; 7] = ["red", "orange", "yellow", "green", "cyan", "blue", "purple"];
const CATEGORICAL_TOKENS: [&str; 7] = ["secondary", "accent", "success", "warning", "primary", "error", "info"];
const HUE_LIGHT: [&str; 7] = ["#fca5a5", "#fdba74", "#fde047", "#86efac", "#67e8f9", "#93c5fd", "#d8b4fe"];
const HUE_DARK: [&str; 7] = ["#b91c1c", "#c2410c", "#a16207", "#15803d", "#0e7490", "#1d4ed8", "#7e22ce"];

type V1 = HashMap<String, Rgba>;

fn ansi(code: u64) -> Rgba {
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff",
        "#00ffff", "#ffffff",
    ];
    if code < 16 {
        return Rgba::from_hex(BASIC[code as usize]).unwrap();
    }
    if code < 232 {
        let i = code - 16;
        let v = |p: u64| if p == 0 { 0 } else { (p * 40 + 55) as u8 };
        return Rgba::from_ints(v(i / 36), v((i / 6) % 6), v(i % 6), 255);
    }
    if code < 256 {
        let g = ((code - 232) * 10 + 8) as u8;
        return Rgba::from_ints(g, g, g, 255);
    }
    Rgba::BLACK
}

fn resolve_v1(theme: &Value, mode: Mode) -> (V1, bool) {
    let defs = theme.get("defs").and_then(|d| d.as_object()).cloned().unwrap_or_default();
    let colors = theme.get("theme").and_then(|t| t.as_object()).cloned().unwrap_or_default();
    fn resolve(v: &Value, defs: &Map<String, Value>, colors: &Map<String, Value>, mode: Mode, depth: usize) -> Rgba {
        if depth > 32 {
            return Rgba::BLACK;
        }
        match v {
            Value::String(s) if s == "transparent" || s == "none" => Rgba::TRANSPARENT,
            Value::String(s) if s.starts_with('#') => Rgba::from_hex(s).unwrap_or(Rgba::BLACK),
            Value::String(s) => match defs.get(s).or_else(|| colors.get(s)) {
                Some(n) => resolve(n, defs, colors, mode, depth + 1),
                None => Rgba::BLACK,
            },
            Value::Number(n) => ansi(n.as_u64().unwrap_or(0)),
            Value::Object(o) => o.get(mode.key()).map(|n| resolve(n, defs, colors, mode, depth + 1)).unwrap_or(Rgba::BLACK),
            _ => Rgba::BLACK,
        }
    }
    let mut out = V1::new();
    for (k, v) in &colors {
        if ["selectedListItemText", "backgroundMenu", "thinkingOpacity"].contains(&k.as_str()) {
            continue;
        }
        out.insert(k.clone(), resolve(v, &defs, &colors, mode, 0));
    }
    let has_selected = colors.contains_key("selectedListItemText");
    let sel = match colors.get("selectedListItemText") {
        Some(v) => resolve(v, &defs, &colors, mode, 0),
        None => out.get("background").copied().unwrap_or(Rgba::BLACK),
    };
    out.insert("selectedListItemText".into(), sel);
    let menu = match colors.get("backgroundMenu") {
        Some(v) if !v.is_null() => resolve(v, &defs, &colors, mode, 0),
        _ => out.get("backgroundElement").copied().unwrap_or(Rgba::BLACK),
    };
    out.insert("backgroundMenu".into(), menu);
    (out, has_selected)
}

fn c(t: &V1, k: &str) -> Rgba {
    t.get(k).copied().unwrap_or(Rgba::BLACK)
}

fn luminance(c: Rgba) -> f64 {
    0.299 * c.r as f64 + 0.587 * c.g as f64 + 0.114 * c.b as f64
}

fn detect_mode(t: &V1) -> Mode {
    if luminance(c(t, "text")) > luminance(c(t, "background")) {
        Mode::Dark
    } else {
        Mode::Light
    }
}

fn selected_foreground(t: &V1, has_selected: bool, bg: Rgba) -> Rgba {
    if has_selected {
        return c(t, "selectedListItemText");
    }
    let background = c(t, "background");
    if background.a != 0.0 {
        return background;
    }
    if luminance(bg) > 0.5 {
        Rgba::from_ints(0, 0, 0, 255)
    } else {
        Rgba::from_ints(255, 255, 255, 255)
    }
}

fn infer_hue(color: Rgba) -> Option<(usize, f64)> {
    let v = to_oklch(color);
    if color.to_ints()[3] == 0 || v.c < 0.03 {
        return None;
    }
    let refs = if v.l >= 0.6 { HUE_LIGHT } else { HUE_DARK };
    let mut best: Option<(usize, f64)> = None;
    for (i, r) in refs.iter().enumerate() {
        let h = to_oklch(Rgba::from_hex(r).unwrap()).h;
        let d = (v.h - h).abs();
        let d = d.min(360.0 - d);
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best
}

struct Inferred {
    by_hue: [Option<(Rgba, f64)>; 7],
    by_token: HashMap<&'static str, usize>,
}

fn infer_hues(t: &V1) -> Inferred {
    let mut r = Inferred { by_hue: [None; 7], by_token: HashMap::new() };
    for token in ["accent", "success", "warning", "primary", "error", "info", "secondary"] {
        let color = c(t, token);
        let Some((i, d)) = infer_hue(color) else { continue };
        if !r.by_hue[i].is_some_and(|(_, cd)| cd <= d) {
            r.by_hue[i] = Some((color, d));
        }
        r.by_token.insert(token, i);
    }
    for token in ["accent", "primary"] {
        let color = c(t, token);
        let Some((i, d)) = infer_hue(color) else { continue };
        r.by_hue[i] = Some((color, d));
        r.by_token.insert(token, i);
    }
    r
}

fn hue_scale(color: Rgba, mode: Mode) -> Value {
    let v = to_oklch(color);
    let endpoint = if mode == Mode::Light { v.l.max(0.97) } else { v.l.min(0.18) };
    let alpha = color.to_ints()[3];
    let mut out = Map::new();
    for step in STEPS {
        let hex = if step == 200 {
            color.hex()
        } else {
            let p = (step as f64 - 200.0) / 700.0;
            let g = oklch_to_hex(Oklch { l: v.l + (endpoint - v.l) * p, c: v.c * (1.0 - p * 0.5), h: v.h });
            if alpha == 255 {
                g
            } else {
                format!("{g}{alpha:02x}")
            }
        };
        out.insert(step.to_string(), json!(hex));
    }
    Value::Object(out)
}

fn interpolate(first: Rgba, second: Rgba, amount: f64) -> String {
    let s = to_oklch(first);
    let e = to_oklch(second);
    let hue = (((e.h - s.h) % 360.0) + 540.0) % 360.0 - 180.0;
    let g = oklch_to_hex(Oklch { l: s.l + (e.l - s.l) * amount, c: s.c + (e.c - s.c) * amount, h: s.h + hue * amount });
    let fa = first.to_ints()[3] as f64;
    let sa = second.to_ints()[3] as f64;
    let alpha = (fa + (sa - fa) * amount).round().clamp(0.0, 255.0) as u8;
    if alpha == 255 {
        g
    } else {
        format!("{g}{alpha:02x}")
    }
}

fn neutral_scale(t: &V1) -> Value {
    let anchors = [(200u32, c(t, "text")), (400, c(t, "textMuted")), (600, c(t, "backgroundElement")), (700, c(t, "backgroundPanel")), (800, c(t, "background"))];
    let mut out = Map::new();
    for step in STEPS {
        let hex = if let Some((_, col)) = anchors.iter().find(|(s, _)| *s == step) {
            col.hex()
        } else {
            let (lower, upper) = if step < anchors[0].0 {
                (anchors[0], anchors[1])
            } else if step > anchors[4].0 {
                (anchors[3], anchors[4])
            } else {
                (*anchors.iter().filter(|(s, _)| *s < step).last().unwrap(), *anchors.iter().find(|(s, _)| *s > step).unwrap())
            };
            interpolate(lower.1, upper.1, (step as f64 - lower.0 as f64) / (upper.0 as f64 - lower.0 as f64))
        };
        out.insert(step.to_string(), json!(hex));
    }
    Value::Object(out)
}

fn dialog_background(t: &V1) -> String {
    if c(t, "backgroundPanel").a == 1.0 {
        return "$background.raised.base".into();
    }
    let visible = ["backgroundPanel", "backgroundMenu", "backgroundElement", "background"].iter().map(|k| c(t, k)).find(|c| c.a > 0.0);
    match visible {
        Some(v) => v.with_alpha(1.0).hex(),
        None if luminance(c(t, "text")) > 0.5 => "#000000".into(),
        None => "#ffffff".into(),
    }
}

fn migrate_mode(t: &V1, has_selected: bool, mode: Mode) -> Value {
    let hx = |k: &str| json!(c(t, k).hex());
    let selected = selected_foreground(t, has_selected, c(t, "primary")).hex();
    let destructive = selected_foreground(t, has_selected, c(t, "error")).hex();
    let hues = infer_hues(t);
    let mut categorical: Vec<String> = Vec::new();
    for token in CATEGORICAL_TOKENS {
        if let Some(i) = hues.by_token.get(token) {
            let n = CHROMATIC[*i].to_string();
            if !categorical.contains(&n) {
                categorical.push(n);
            }
        }
    }
    if categorical.is_empty() {
        categorical.push("neutral".into());
    }
    let mut hue = Map::new();
    hue.insert("gray".into(), neutral_scale(t));
    for (i, name) in CHROMATIC.iter().enumerate() {
        if let Some((color, _)) = hues.by_hue[i] {
            hue.insert(name.to_string(), hue_scale(color, mode));
        }
    }
    let alias = |token: &str| hues.by_token.get(token).map(|i| format!("$hue.{}", CHROMATIC[*i])).unwrap_or_else(|| "$hue.gray".into());
    hue.insert("accent".into(), json!(alias("accent")));
    hue.insert("interactive".into(), json!(alias("primary")));
    hue.insert("neutral".into(), json!("$hue.gray"));
    let (text, muted, primary) = ("$hue.neutral.200", "$hue.neutral.400", "$hue.interactive.200");
    let (bg, panel, menu, max) = ("$hue.neutral.800", "$hue.neutral.700", "$hue.neutral.600", "$hue.neutral.500");
    json!({
        "hue": Value::Object(hue),
        "categorical": categorical,
        "text": {
            "base": text,
            "muted": muted,
            "action": {
                "primary": { "base": "$text.base", "$disabled": muted, "$focused": selected, "$selected": primary },
                "secondary": { "base": "$text.muted", "$hovered": "$text.base" },
                "destructive": { "base": destructive, "$disabled": muted },
            },
            "formfield": { "base": text, "$hovered": primary, "$focused": primary, "$pressed": primary, "$disabled": muted, "$selected": primary },
            "feedback": {
                "error": { "base": hx("error") },
                "warning": { "base": hx("warning") },
                "success": { "base": hx("success") },
                "info": { "base": hx("info") },
            },
        },
        "background": {
            "base": bg,
            "raised": { "base": panel, "high": menu, "max": max },
            "action": {
                "primary": { "base": "transparent", "$hovered": panel, "$focused": primary, "$selected": "transparent" },
                "secondary": { "base": "transparent" },
                "destructive": { "base": hx("error") },
            },
            "formfield": { "base": "$background.base" },
            "feedback": {
                "error": { "base": "$background.base" },
                "warning": { "base": "$background.base" },
                "success": { "base": "$background.base" },
                "info": { "base": "$background.base" },
            },
        },
        "border": { "base": hx("border") },
        "scrollbar": { "base": hx("borderActive") },
        "diff": {
            "text": { "added": hx("diffAdded"), "removed": hx("diffRemoved"), "context": hx("diffContext"), "hunkHeader": hx("diffHunkHeader") },
            "background": { "added": hx("diffAddedBg"), "removed": hx("diffRemovedBg"), "context": hx("diffContextBg") },
            "highlight": { "added": hx("diffHighlightAdded"), "removed": hx("diffHighlightRemoved") },
            "lineNumber": { "text": hx("diffLineNumber"), "background": { "added": hx("diffAddedLineNumberBg"), "removed": hx("diffRemovedLineNumberBg") } },
        },
        "syntax": {
            "comment": hx("syntaxComment"), "keyword": hx("syntaxKeyword"), "function": hx("syntaxFunction"), "variable": hx("syntaxVariable"),
            "string": hx("syntaxString"), "number": hx("syntaxNumber"), "type": hx("syntaxType"), "operator": hx("syntaxOperator"),
            "punctuation": hx("syntaxPunctuation"),
        },
        "markdown": {
            "text": hx("markdownText"), "heading": hx("markdownHeading"), "link": hx("markdownLink"), "linkText": hx("markdownLinkText"),
            "code": hx("markdownCode"), "blockQuote": hx("markdownBlockQuote"), "emphasis": hx("markdownEmph"), "strong": hx("markdownStrong"),
            "horizontalRule": hx("markdownHorizontalRule"), "listItem": hx("markdownListItem"), "listEnumeration": hx("markdownListEnumeration"),
            "image": hx("markdownImage"), "imageText": hx("markdownImageText"), "codeBlock": hx("markdownCodeBlock"),
        },
        "@dialog": { "background": { "base": dialog_background(t), "action": { "primary": { "$hovered": "$background.raised.high" } } } },
    })
}

fn split_base(def: &Value) -> (Value, Value) {
    let mut base = def.as_object().cloned().unwrap_or_default();
    let hue = base.remove("hue").unwrap_or(Value::Null);
    (Value::Object(base), hue)
}

pub fn migrate_v1(theme: &Value) -> Value {
    let (light, light_sel) = resolve_v1(theme, Mode::Light);
    let (dark, dark_sel) = resolve_v1(theme, Mode::Dark);
    let (lb, db) = (c(&light, "background"), c(&dark, "background"));
    if lb.a > 0.0 && db.a > 0.0 && lb == db {
        let (lm, dm) = (detect_mode(&light), detect_mode(&dark));
        if lm == dm {
            let def = if lm == Mode::Light { migrate_mode(&light, light_sel, lm) } else { migrate_mode(&dark, dark_sel, lm) };
            let (base, hue) = split_base(&def);
            return json!({ "base": base, lm.key(): { "hue": hue } });
        }
    }
    let ld = migrate_mode(&light, light_sel, Mode::Light);
    let dd = migrate_mode(&dark, dark_sel, Mode::Dark);
    let (base, light_hue) = split_base(&ld);
    json!({ "base": base, "light": { "hue": light_hue }, "dark": dd })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(name: &str, mode: Mode) -> Theme {
        let src: Value = serde_json::from_str(BUILTIN.iter().find(|(n, _)| *n == name).unwrap().1).unwrap();
        load(name, &src, mode).unwrap()
    }

    #[test]
    fn all_builtins_resolve() {
        for (name, src) in BUILTIN {
            let v: Value = serde_json::from_str(src).unwrap();
            for m in modes(&v) {
                load(name, &v, m).unwrap_or_else(|e| panic!("{name} {m:?}: {e}"));
            }
        }
    }

    #[test]
    fn opencode_dark_tokens() {
        let t = builtin("opencode", Mode::Dark);
        assert_eq!(t.bg.hex(), "#0a0a0a");
        assert_eq!(t.raised.hex(), "#141414");
        assert_eq!(t.decrease(t.raised).hex(), "#1e1e1e");
        assert_eq!(t.text.hex(), "#eeeeee");
        assert_eq!(t.dialog.bg.hex(), "#141414");
        assert_eq!(t.agent_colors()[0].hex(), "#5c9cf5");
    }

    /// Colors captured from the TypeScript client running the tokyonight theme.
    #[test]
    fn tokyonight_matches_upstream() {
        let t = builtin("tokyonight", Mode::Dark);
        assert_eq!(t.bg.hex(), "#1a1b26");
        assert_eq!(t.raised.hex(), "#1e2030");
        assert_eq!(t.decrease(t.raised).hex(), "#222436");
        assert_eq!(t.decrease(t.bg).hex(), "#1e2030");
        assert_eq!(t.text.hex(), "#c8d3f5");
        assert_eq!(t.muted.hex(), "#828bb8");
        assert_eq!(t.warning.base.hex(), "#ff966c");
        assert_eq!(t.markdown.code.hex(), "#c3e88d");
        assert_eq!(t.agent_colors()[0].hex(), "#c099ff");
        assert_eq!(t.warning.base.with_alpha(0.6).over(t.bg).hex(), "#a36550");
    }
}

//! Turns slot state into pixels for keys and touch-strip segments.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ab_glyph::{FontVec, PxScale};
use base64::Engine;
use image::{imageops, DynamicImage, Rgba, RgbaImage};
use serde_json::{Map, Value};
use std::path::Path;

use crate::config::{Label, LabelPos, State};
use crate::layout::{ItemKind, Layout};

/// Transient overlay shown after a plugin calls showAlert / showOk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flash {
    Alert,
    Ok,
}

pub struct Renderer {
    font: FontVec,
    images: Mutex<HashMap<(String, u32), Arc<RgbaImage>>>,
}

impl Renderer {
    pub fn new() -> anyhow::Result<Self> {
        let path = find_font().ok_or_else(|| anyhow::anyhow!("no usable font found (install noto-fonts)"))?;
        let font = FontVec::try_from_vec(std::fs::read(&path)?)?;
        Ok(Self { font, images: Mutex::new(HashMap::new()) })
    }

    pub fn render_key(&self, size: u32, state: Option<&State>, flash: Option<Flash>) -> DynamicImage {
        let bg = state.and_then(|s| s.background.as_deref()).and_then(parse_color).unwrap_or(Rgba([0, 0, 0, 255]));
        let mut canvas = RgbaImage::from_pixel(size, size, bg);

        if let Some(src) = state.and_then(|s| s.image.as_deref()) {
            if let Some(img) = self.load_image(src, size) {
                let x = (size - img.width()) / 2;
                let y = (size - img.height()) / 2;
                imageops::overlay(&mut canvas, img.as_ref(), x as i64, y as i64);
            }
        }

        for (pos, label) in &collect_labels(state) {
            let px = label.size.unwrap_or(16.0) * size as f32 / 72.0 * 0.9;
            self.draw_block(&mut canvas, label, *pos, px, 0.0, size as f32, size as f32, 4.0, 6.0);
        }
        if let Some(f) = flash {
            self.draw_flash(&mut canvas, f);
        }
        DynamicImage::ImageRgba8(canvas)
    }

    /// One touch-strip segment for a non-plugin action: icon on the left, labels on the right.
    pub fn render_dial(&self, w: u32, h: u32, state: Option<&State>) -> DynamicImage {
        let bg = state.and_then(|s| s.background.as_deref()).and_then(parse_color).unwrap_or(Rgba([0, 0, 0, 255]));
        let mut canvas = RgbaImage::from_pixel(w, h, bg);

        let mut text_x = 0.0;
        if let Some(src) = state.and_then(|s| s.image.as_deref()) {
            let icon = h * 7 / 10;
            if let Some(img) = self.load_image(src, icon) {
                let pad = (h - icon) / 2;
                imageops::overlay(&mut canvas, img.as_ref(), pad as i64, ((h - img.height()) / 2) as i64);
                text_x = (icon + pad * 2) as f32;
            }
        }

        let area = w as f32 - text_x;
        for (pos, label) in &collect_labels(state) {
            let px = label.size.unwrap_or(16.0) * 1.3;
            self.draw_block(&mut canvas, label, *pos, px, text_x, area, h as f32, 8.0, 8.0);
        }
        DynamicImage::ImageRgba8(canvas)
    }

    /// Renders an Elgato feedback layout. Layout coordinates are for a 200x100 canvas.
    pub fn render_layout(&self, w: u32, h: u32, layout: &Layout, background: Option<&str>, plugin_dir: Option<&Path>, flash: Option<Flash>) -> DynamicImage {
        let mut canvas = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
        if let Some(bg) = background {
            if let Some(c) = parse_color(bg) {
                canvas = RgbaImage::from_pixel(w, h, c);
            } else if let Some(img) = self.load_image(bg, w.max(h)) {
                let img = imageops::resize(img.as_ref(), w, h, imageops::FilterType::Triangle);
                imageops::overlay(&mut canvas, &img, 0, 0);
            }
        }
        let sx = w as f32 / 200.0;
        let sy = h as f32 / 100.0;
        for item in &layout.items {
            let p = &item.props;
            if p.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let [x, y, iw, ih] = item.rect;
            let (x, y, iw, ih) = ((x as f32 * sx), (y as f32 * sy), (iw as f32 * sx), (ih as f32 * sy));
            let opacity = p.get("opacity").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0) as f32;
            match item.kind {
                ItemKind::Text => {
                    let text = match p.get("value") {
                        Some(Value::String(s)) => s.clone(),
                        Some(Value::Number(n)) => n.to_string(),
                        _ => continue,
                    };
                    if text.is_empty() {
                        continue;
                    }
                    let size = p.get("font").and_then(|f| f.get("size")).and_then(Value::as_f64).unwrap_or(16.0) as f32 * sy;
                    let align = match p.get("alignment").and_then(Value::as_str) {
                        Some("left") => Align::Left,
                        Some("right") => Align::Right,
                        _ => Align::Center,
                    };
                    let mut color = p.get("color").and_then(Value::as_str).and_then(parse_color).unwrap_or(Rgba([255, 255, 255, 255]));
                    color.0[3] = (color.0[3] as f32 * opacity) as u8;
                    let label = Label { text, size: None, color: None };
                    let ty = y + (ih - size) / 2.0;
                    self.draw_text_color(&mut canvas, &label, size, x, iw, ty, align, color);
                }
                ItemKind::Pixmap => {
                    let Some(src) = p.get("value").and_then(Value::as_str).filter(|s| !s.is_empty()) else { continue };
                    let src = resolve_src(src, plugin_dir);
                    if let Some(img) = self.load_image_box(&src, iw as u32, ih as u32) {
                        let ox = x + (iw - img.width() as f32) / 2.0;
                        let oy = y + (ih - img.height() as f32) / 2.0;
                        if opacity < 1.0 {
                            let mut img = (*img).clone();
                            for px in img.pixels_mut() {
                                px.0[3] = (px.0[3] as f32 * opacity) as u8;
                            }
                            imageops::overlay(&mut canvas, &img, ox as i64, oy as i64);
                        } else {
                            imageops::overlay(&mut canvas, img.as_ref(), ox as i64, oy as i64);
                        }
                    }
                }
                ItemKind::Bar | ItemKind::GBar => {
                    let min = p.get("range").and_then(|r| r.get("min")).and_then(Value::as_f64).unwrap_or(0.0);
                    let max = p.get("range").and_then(|r| r.get("max")).and_then(Value::as_f64).unwrap_or(100.0);
                    let v = p.get("value").and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0.0);
                    let frac = if max > min { ((v - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 } as f32;
                    self.draw_bar(&mut canvas, item.kind, p, (x, y, iw, ih), frac, sy);
                }
            }
        }
        if let Some(f) = flash {
            self.draw_flash(&mut canvas, f);
        }
        DynamicImage::ImageRgba8(canvas)
    }

    /// Analog clock face. Drawn fresh each time, without touching the image cache.
    pub fn render_analog_clock(&self, w: u32, h: u32, hour: u32, minute: u32, second: u32, background: Option<&str>) -> DynamicImage {
        let bg = background.and_then(parse_color).unwrap_or(Rgba([0, 0, 0, 255]));
        let mut canvas = RgbaImage::from_pixel(w, h, bg);
        let side = w.min(h);
        let (cx, cy, r) = (50.0, 50.0, 46.0);
        let mut ticks = String::new();
        for i in 0..12 {
            let a = i as f32 * std::f32::consts::PI / 6.0;
            let (inner, width) = if i % 3 == 0 { (r - 10.0, 3.5) } else { (r - 6.0, 2.0) };
            ticks += &format!(
                r##"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="#cfcfcf" stroke-width="{width}" stroke-linecap="round"/>"##,
                cx + inner * a.sin(), cy - inner * a.cos(), cx + (r - 2.0) * a.sin(), cy - (r - 2.0) * a.cos()
            );
        }
        let hand = |angle: f32, len: f32, width: f32, color: &str| {
            format!(
                r#"<line x1="{cx}" y1="{cy}" x2="{:.2}" y2="{:.2}" stroke="{color}" stroke-width="{width}" stroke-linecap="round"/>"#,
                cx + len * angle.sin(), cy - len * angle.cos()
            )
        };
        let tau = std::f32::consts::TAU;
        let m = minute as f32 + second as f32 / 60.0;
        let hr = (hour % 12) as f32 + m / 60.0;
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><circle cx="50" cy="50" r="{r}" fill="#151515" stroke="#3a3a3a" stroke-width="2"/>{ticks}{}{}{}<circle cx="50" cy="50" r="3.5" fill="#f7821b"/></svg>"##,
            hand(hr / 12.0 * tau, 24.0, 5.0, "#ffffff"),
            hand(m / 60.0 * tau, 36.0, 3.5, "#ffffff"),
            hand(second as f32 / 60.0 * tau, 38.0, 1.5, "#f7821b"),
        );
        if let Ok(face) = render_svg_uncached(&svg, side) {
            imageops::overlay(&mut canvas, &face, ((w - face.width()) / 2) as i64, ((h - face.height()) / 2) as i64);
        }
        DynamicImage::ImageRgba8(canvas)
    }

    /// Native volume dial: icon left, name over percentage right, bar along the bottom.
    pub fn render_volume(&self, w: u32, h: u32, icon: &str, name: &str, percent: Option<u32>, muted: bool, flash: Option<Flash>) -> DynamicImage {
        use imageproc::drawing::{draw_filled_rect_mut, draw_line_segment_mut};
        use imageproc::rect::Rect;
        let mut canvas = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
        let (sx, sy) = (w as f32 / 200.0, h as f32 / 100.0);
        let accent = if muted { Rgba([110, 110, 110, 255]) } else { Rgba([247, 130, 27, 255]) };

        // Icon
        let icon_box = (52.0 * sy) as u32;
        let (ix, iy) = ((12.0 * sx) as i64, (12.0 * sy) as i64);
        if let Some(img) = self.load_image(&resolve_src(icon, None), icon_box) {
            let mut img = (*img).clone();
            if muted {
                for px in img.pixels_mut() {
                    px.0[3] /= 3;
                }
            }
            let ox = ix + (icon_box as i64 - img.width() as i64) / 2;
            let oy = iy + (icon_box as i64 - img.height() as i64) / 2;
            imageops::overlay(&mut canvas, &img, ox, oy);
            if muted {
                let red = Rgba([230, 50, 50, 255]);
                let (x0, y0, x1, y1) = (ix as f32 + 4.0, iy as f32 + 4.0, ix as f32 + icon_box as f32 - 4.0, iy as f32 + icon_box as f32 - 4.0);
                for o in -2..=2 {
                    draw_line_segment_mut(&mut canvas, (x0 + o as f32, y0), (x1 + o as f32, y1), red);
                }
            }
        }

        // Text column to the right of the icon
        let tx = 76.0 * sx;
        let tw = w as f32 - tx - 10.0 * sx;
        let name_label = Label { text: name.to_string(), ..Default::default() };
        self.draw_text_color(&mut canvas, &name_label, 17.0 * sy, tx, tw, 12.0 * sy, Align::Left, Rgba([200, 200, 200, 255]));
        let value = match percent {
            Some(_) if muted => "Muted".to_string(),
            Some(p) => format!("{p}%"),
            None => "—".to_string(),
        };
        let value_label = Label { text: value, ..Default::default() };
        self.draw_text_color(&mut canvas, &value_label, 30.0 * sy, tx, tw, 36.0 * sy, Align::Left, Rgba([255, 255, 255, 255]));

        // Bar
        let (bx, by, bw, bh) = ((12.0 * sx) as i32, (80.0 * sy) as i32, (176.0 * sx) as u32, (8.0 * sy) as u32);
        draw_filled_rect_mut(&mut canvas, Rect::at(bx, by).of_size(bw, bh), Rgba([51, 51, 51, 255]));
        let fill = (bw as f32 * percent.unwrap_or(0).min(100) as f32 / 100.0) as u32;
        if fill > 0 {
            draw_filled_rect_mut(&mut canvas, Rect::at(bx, by).of_size(fill, bh), accent);
        }
        if let Some(f) = flash {
            self.draw_flash(&mut canvas, f);
        }
        DynamicImage::ImageRgba8(canvas)
    }

    fn draw_bar(&self, canvas: &mut RgbaImage, kind: ItemKind, p: &Map<String, Value>, (x, y, w, h): (f32, f32, f32, f32), frac: f32, sy: f32) {
        use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut};
        use imageproc::rect::Rect;
        let color = |k: &str, d: [u8; 4]| p.get(k).and_then(Value::as_str).and_then(parse_color).unwrap_or(Rgba(d));
        let bg = color("bar_bg_c", [80, 80, 80, 255]);
        let border = color("bar_border_c", [255, 255, 255, 255]);
        let border_w = p.get("border_w").and_then(Value::as_f64).unwrap_or(2.0) as u32;
        let bar_h = if kind == ItemKind::GBar { p.get("bar_h").and_then(Value::as_f64).unwrap_or(10.0) as f32 * sy } else { h.min(12.0 * sy) };
        let by = y + (h - bar_h) / 2.0;
        let rect = |x: f32, w: f32| Rect::at(x as i32, by as i32).of_size((w.max(1.0)) as u32, bar_h.max(1.0) as u32);

        if kind == ItemKind::GBar {
            // Gradient track with a marker at the current value.
            let stops = parse_gradient(p.get("bar_fill_c").and_then(Value::as_str).unwrap_or("0:#ff0000,0.5:#ffff00,1:#00ff00"));
            for i in 0..w as u32 {
                let c = sample_gradient(&stops, i as f32 / w.max(1.0));
                draw_filled_rect_mut(canvas, Rect::at((x as u32 + i) as i32, by as i32).of_size(1, bar_h.max(1.0) as u32), c);
            }
            let mx = x + frac * w;
            draw_filled_rect_mut(canvas, Rect::at(mx as i32 - 2, (by - 3.0) as i32).of_size(4, (bar_h + 6.0) as u32), Rgba([255, 255, 255, 255]));
        } else {
            let fill = color("bar_fill_c", [255, 255, 255, 255]);
            draw_filled_rect_mut(canvas, rect(x, w), bg);
            if frac > 0.0 {
                draw_filled_rect_mut(canvas, rect(x, w * frac), fill);
            }
            for i in 0..border_w {
                let r = Rect::at(x as i32 - i as i32, by as i32 - i as i32).of_size(w as u32 + 2 * i, bar_h as u32 + 2 * i);
                draw_hollow_rect_mut(canvas, r, border);
            }
        }
    }

    fn draw_flash(&self, canvas: &mut RgbaImage, flash: Flash) {
        let (tint, text) = match flash {
            Flash::Alert => (Rgba([200, 30, 30, 170]), "!"),
            Flash::Ok => (Rgba([30, 170, 60, 170]), "OK"),
        };
        let overlay = RgbaImage::from_pixel(canvas.width(), canvas.height(), tint);
        imageops::overlay(canvas, &overlay, 0, 0);
        let h = canvas.height() as f32;
        let label = Label { text: text.into(), ..Default::default() };
        self.draw_text(canvas, &label, h * 0.45, 0.0, canvas.width() as f32, h * 0.25, Align::Center);
    }

    /// Draws a possibly multi-line label anchored at `pos`, shrinking it if the lines don't fit.
    #[allow(clippy::too_many_arguments)]
    fn draw_block(&self, canvas: &mut RgbaImage, label: &Label, pos: LabelPos, px: f32, x0: f32, width: f32, height: f32, top_pad: f32, bottom_pad: f32) {
        let lines: Vec<&str> = label.text.lines().collect();
        if lines.is_empty() {
            return;
        }
        let avail = height - top_pad - bottom_pad;
        let mut px = px;
        if lines.len() as f32 * px * 1.1 > avail {
            px = avail / (lines.len() as f32 * 1.1);
        }
        let line_h = px * 1.1;
        let block = line_h * lines.len() as f32 - (line_h - px);
        let y0 = match pos {
            LabelPos::Top => top_pad,
            LabelPos::Center => (height - block) / 2.0,
            LabelPos::Bottom => height - block - bottom_pad,
        };
        for (i, line) in lines.iter().enumerate() {
            let l = Label { text: line.trim().to_string(), size: label.size, color: label.color.clone() };
            self.draw_text(canvas, &l, px, x0, width, y0 + i as f32 * line_h, Align::Center);
        }
    }

    fn draw_text(&self, canvas: &mut RgbaImage, label: &Label, px: f32, x0: f32, width: f32, y: f32, align: Align) {
        let color = label.color.as_deref().and_then(parse_color).unwrap_or(Rgba([255, 255, 255, 255]));
        self.draw_text_color(canvas, label, px, x0, width, y, align, color);
    }

    /// Draws text aligned within [x0, x0+width), shrinking it to fit.
    #[allow(clippy::too_many_arguments)]
    fn draw_text_color(&self, canvas: &mut RgbaImage, label: &Label, px: f32, x0: f32, width: f32, y: f32, align: Align, color: Rgba<u8>) {
        // The font has no glyphs for control characters; they'd show as boxes.
        let folded;
        let label = if label.text.chars().any(char::is_control) {
            folded = Label { text: label.text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect(), ..label.clone() };
            &folded
        } else {
            label
        };
        let mut scale = PxScale::from(px);
        let (mut tw, _) = imageproc::drawing::text_size(scale, &self.font, &label.text);
        if tw as f32 > width - 4.0 && tw > 0 {
            let f = (width - 4.0) / tw as f32;
            scale = PxScale::from(px * f);
            tw = (tw as f32 * f) as u32;
        }
        let x = match align {
            Align::Left => x0,
            Align::Center => x0 + (width - tw as f32) / 2.0,
            Align::Right => x0 + width - tw as f32,
        };
        // Drop shadow keeps text readable on top of icons.
        let shadow = Rgba([0, 0, 0, (200.0 * color.0[3] as f32 / 255.0) as u8]);
        imageproc::drawing::draw_text_mut(canvas, shadow, x as i32 + 1, y as i32 + 1, scale, &self.font, &label.text);
        imageproc::drawing::draw_text_mut(canvas, color, x as i32, y as i32, scale, &self.font, &label.text);
    }

    /// Natural size of an image (SVG viewport or raster dimensions).
    pub fn image_dims(&self, src: &str) -> Option<(u32, u32)> {
        let key = (format!("dims#{:x}", fxhash(src)), 0);
        if let Some(img) = self.images.lock().unwrap().get(&key) {
            return Some((img.width(), img.height()));
        }
        let dims = natural_dims(src)?;
        // Cache as an empty image of the right size to reuse the same map.
        self.images.lock().unwrap().insert(key, Arc::new(RgbaImage::new(dims.0, dims.1)));
        Some(dims)
    }

    /// Loads and scales an image to fit a `size`x`size` box, caching the result.
    fn load_image(&self, src: &str, size: u32) -> Option<Arc<RgbaImage>> {
        self.load_image_box(src, size, size)
    }

    /// Loads and scales an image to fit within `w`x`h`, keeping its aspect ratio.
    fn load_image_box(&self, src: &str, w: u32, h: u32) -> Option<Arc<RgbaImage>> {
        // Hash long data URIs instead of storing them twice as cache keys.
        let id = if src.len() > 256 { format!("#{:x}", fxhash(src)) } else { src.to_string() };
        let key = (format!("{id}@{w}x{h}"), 1);
        if let Some(img) = self.images.lock().unwrap().get(&key) {
            return Some(img.clone());
        }
        let img = match decode_image(src, w.max(1), h.max(1)) {
            Ok(img) => Arc::new(img),
            Err(e) => {
                tracing::warn!("image {}: {e:#}", short(src));
                return None;
            }
        };
        let mut cache = self.images.lock().unwrap();
        if cache.len() > 512 {
            cache.clear();
        }
        cache.insert(key, img.clone());
        Some(img)
    }
}

#[derive(Debug, Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
}

fn resolve_src(src: &str, plugin_dir: Option<&Path>) -> String {
    if src.starts_with("data:") || src.starts_with('/') || src.starts_with("glyph:") {
        return src.to_string();
    }
    if src.trim_start().starts_with("<svg") || src.trim_start().starts_with("<?xml") {
        return format!("data:image/svg+xml,{src}");
    }
    plugin_dir.and_then(|d| crate::plugins::manifest::resolve_image(d, src)).unwrap_or_else(|| src.to_string())
}

fn fxhash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn parse_gradient(spec: &str) -> Vec<(f32, Rgba<u8>)> {
    let stops: Vec<(f32, Rgba<u8>)> = spec
        .split(',')
        .filter_map(|s| {
            let (pos, c) = s.trim().split_once(':')?;
            Some((pos.trim().parse().ok()?, parse_color(c.trim())?))
        })
        .collect();
    if stops.is_empty() {
        vec![(0.0, Rgba([255, 255, 255, 255])), (1.0, Rgba([255, 255, 255, 255]))]
    } else {
        stops
    }
}

fn sample_gradient(stops: &[(f32, Rgba<u8>)], t: f32) -> Rgba<u8> {
    let mut prev = stops[0];
    for &s in stops {
        if t <= s.0 {
            let span = (s.0 - prev.0).max(1e-6);
            let f = ((t - prev.0) / span).clamp(0.0, 1.0);
            let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * f) as u8;
            return Rgba([mix(prev.1 .0[0], s.1 .0[0]), mix(prev.1 .0[1], s.1 .0[1]), mix(prev.1 .0[2], s.1 .0[2]), 255]);
        }
        prev = s;
    }
    prev.1
}

fn collect_labels(state: Option<&State>) -> Vec<(LabelPos, Label)> {
    state
        .map(|s| s.labels.iter().filter(|(_, l)| !l.text.is_empty()).map(|(p, l)| (*p, l.clone())).collect())
        .unwrap_or_default()
}

fn read_source(src: &str) -> anyhow::Result<(Vec<u8>, bool)> {
    if src.starts_with("glyph:") {
        let svg = crate::icons::resolve_ref(src).ok_or_else(|| anyhow::anyhow!("icon not found (is its icon set installed?)"))?;
        return Ok((svg.into_bytes(), true));
    }
    if let Some(rest) = src.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',').ok_or_else(|| anyhow::anyhow!("malformed data uri"))?;
        let bytes = if meta.ends_with(";base64") {
            base64::engine::general_purpose::STANDARD.decode(data.trim())?
        } else {
            urlencoding_decode(data).into_bytes()
        };
        Ok((bytes, meta.starts_with("image/svg")))
    } else {
        let path = src.strip_prefix("file://").unwrap_or(src);
        Ok((std::fs::read(path)?, path.ends_with(".svg")))
    }
}

fn natural_dims(src: &str) -> Option<(u32, u32)> {
    let (bytes, is_svg) = read_source(src).ok()?;
    if is_svg {
        let tree = resvg::usvg::Tree::from_data(&bytes, svg_options()).ok()?;
        let s = tree.size();
        return Some((s.width() as u32, s.height() as u32));
    }
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

/// SVG options with system fonts loaded, so `<text>` in plugin images renders.
fn svg_options() -> &'static resvg::usvg::Options<'static> {
    static OPTS: std::sync::OnceLock<resvg::usvg::Options<'static>> = std::sync::OnceLock::new();
    OPTS.get_or_init(|| {
        let mut opts = resvg::usvg::Options::default();
        opts.fontdb_mut().load_system_fonts();
        opts.font_family = "Noto Sans".into();
        opts
    })
}

fn render_svg_uncached(svg: &str, size: u32) -> anyhow::Result<RgbaImage> {
    decode_image(&format!("data:image/svg+xml,{svg}"), size, size)
}

fn decode_image(src: &str, w: u32, h: u32) -> anyhow::Result<RgbaImage> {
    let (bytes, is_svg) = read_source(src)?;
    if is_svg {
        let tree = resvg::usvg::Tree::from_data(&bytes, svg_options())?;
        let ts = tree.size();
        let scale = (w as f32 / ts.width()).min(h as f32 / ts.height());
        let (pw, ph) = ((ts.width() * scale).round() as u32, (ts.height() * scale).round() as u32);
        let mut pixmap = resvg::tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).unwrap();
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        // tiny-skia stores premultiplied alpha; image expects straight alpha.
        let mut out = RgbaImage::new(pixmap.width(), pixmap.height());
        for (dst, p) in out.pixels_mut().zip(pixmap.pixels()) {
            let c = p.demultiply();
            *dst = Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
        }
        return Ok(out);
    }
    let img = image::load_from_memory(&bytes)?;
    Ok(img.resize(w, h, imageops::FilterType::Lanczos3).into_rgba8())
}

fn urlencoding_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_color(s: &str) -> Option<Rgba<u8>> {
    let s = s.trim();
    let named = match s.to_ascii_lowercase().as_str() {
        "white" => Some([255, 255, 255]),
        "black" => Some([0, 0, 0]),
        "red" => Some([255, 0, 0]),
        "green" => Some([0, 128, 0]),
        "blue" => Some([0, 0, 255]),
        "yellow" => Some([255, 255, 0]),
        "gray" | "grey" => Some([128, 128, 128]),
        "darkgray" | "darkgrey" => Some([64, 64, 64]),
        "lightgray" | "lightgrey" => Some([192, 192, 192]),
        "transparent" => return Some(Rgba([0, 0, 0, 0])),
        _ => None,
    };
    if let Some([r, g, b]) = named {
        return Some(Rgba([r, g, b, 255]));
    }
    let hex = s.strip_prefix('#')?;
    let v = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        3 => Some(Rgba([((v >> 8) & 0xf) as u8 * 17, ((v >> 4) & 0xf) as u8 * 17, (v & 0xf) as u8 * 17, 255])),
        6 => Some(Rgba([(v >> 16) as u8, (v >> 8) as u8, v as u8, 255])),
        8 => Some(Rgba([(v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8])),
        _ => None,
    }
}

fn find_font() -> Option<String> {
    if let Ok(out) = std::process::Command::new("fc-match").args(["-f", "%{file}", "Noto Sans:bold"]).output() {
        let path = String::from_utf8_lossy(&out.stdout).into_owned();
        if !path.is_empty() && std::path::Path::new(&path).exists() {
            return Some(path);
        }
    }
    ["/usr/share/fonts/noto/NotoSans-Bold.ttf", "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(String::from)
}

fn short(s: &str) -> &str {
    &s[..s.len().min(80)]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders `DECKD_TEST_IMAGE` (a data URI file) as a full-canvas dial segment.
    #[test]
    fn full_canvas_preview() {
        let (Ok(input), Ok(out)) = (std::env::var("DECKD_TEST_IMAGE"), std::env::var("DECKD_TEST_OUT")) else { return };
        let uri = std::fs::read_to_string(input).unwrap();
        let r = Renderer::new().unwrap();
        let mut l = Layout::resolve(&Value::String("$A0".into()), None);
        l.items.retain(|i| i.key == "full-canvas");
        l.set_default("full-canvas", Value::String(uri));
        r.render_layout(200, 100, &l, None, None, None).save(out).unwrap();
    }
}

/// Default icons for built-in actions (Material Design Icons, Apache 2.0).
pub mod icons {
    pub const SPEAKER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#fff" d="M3 9v6h4l5 5V4L7 9H3zm13.5 3A4.5 4.5 0 0 0 14 7.97v8.05c1.48-.73 2.5-2.25 2.5-4.02zM14 3.23v2.06c2.89.86 5 3.54 5 6.71s-2.11 5.85-5 6.71v2.06c4.01-.91 7-4.49 7-8.77s-2.99-7.86-7-8.77z"/></svg>"##;
    pub const HEADSET: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#fff" d="M12 1a9 9 0 0 0-9 9v7c0 1.66 1.34 3 3 3h3v-8H5v-2c0-3.87 3.13-7 7-7s7 3.13 7 7v2h-4v8h3c1.66 0 3-1.34 3-3v-7a9 9 0 0 0-9-9z"/></svg>"##;
    pub const MIC: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#fff" d="M12 14c1.66 0 2.99-1.34 2.99-3L15 5c0-1.66-1.34-3-3-3S9 3.34 9 5v6c0 1.66 1.34 3 3 3zm5.3-3c0 3-2.54 5.1-5.3 5.1S6.7 14 6.7 11H5c0 3.41 2.72 6.23 6 6.72V21h2v-3.28c3.28-.48 6-3.3 6-6.72h-1.7z"/></svg>"##;
}

#[cfg(test)]
mod volume_tests {
    use super::*;

    #[test]
    fn volume_preview() {
        let Ok(out) = std::env::var("DECKD_TEST_OUT") else { return };
        let r = Renderer::new().unwrap();
        let mut canvas = RgbaImage::from_pixel(620, 100, Rgba([60, 60, 60, 255]));
        let a = r.render_volume(200, 100, icons::SPEAKER, "Speakers", Some(60), false, None);
        let b = r.render_volume(200, 100, icons::HEADSET, "Headset", Some(100), false, None);
        let c = r.render_volume(200, 100, icons::SPEAKER, "Speakers", Some(35), true, None);
        imageops::overlay(&mut canvas, &a, 0, 0);
        imageops::overlay(&mut canvas, &b, 210, 0);
        imageops::overlay(&mut canvas, &c, 420, 0);
        canvas.save(out).unwrap();
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;

    #[test]
    fn multiline_preview() {
        let Ok(out) = std::env::var("DECKD_TEST_OUT") else { return };
        let r = Renderer::new().unwrap();
        let mut canvas = RgbaImage::from_pixel(390, 120, Rgba([60, 60, 60, 255]));
        for (i, (text, pos)) in [("Starting\nsoon\nscene", LabelPos::Bottom), ("BRB scene", LabelPos::Bottom), ("Screen 1\ncapture\nscene", LabelPos::Center)].iter().enumerate() {
            let mut st = State::default();
            st.labels.insert(*pos, Label { text: text.to_string(), ..Default::default() });
            imageops::overlay(&mut canvas, &r.render_key(120, Some(&st), None), i as i64 * 135, 0);
        }
        canvas.save(out).unwrap();
    }
}

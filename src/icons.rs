//! Icon library: Iconify icon sets (Material, Font Awesome, ...) and Elgato icon packs.
//!
//! Sets are downloaded once (Iconify JSON from the npm CDN) into
//! `~/.config/deckd/iconsets/<prefix>.json`; searching and rendering are local.
//! A chosen icon is referenced as `glyph:<prefix>:<name>?color=%23rrggbb&scale=0.6`
//! and rendered as a vector at whatever size it's drawn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config;

/// Sets offered in the UI: (prefix, display name, license).
pub const SETS: &[(&str, &str, &str)] = &[
    ("mdi", "Material Design Icons", "Apache 2.0"),
    ("material-symbols", "Material Symbols", "Apache 2.0"),
    ("fa6-solid", "Font Awesome 6 Solid", "CC BY 4.0"),
    ("fa6-regular", "Font Awesome 6 Regular", "CC BY 4.0"),
    ("fa6-brands", "Font Awesome 6 Brands", "CC BY 4.0"),
    ("simple-icons", "Simple Icons (brand logos)", "CC0 1.0"),
    ("tabler", "Tabler Icons", "MIT"),
    ("lucide", "Lucide", "ISC"),
    ("ph", "Phosphor", "MIT"),
];

pub fn sets_dir() -> PathBuf {
    config::config_dir().join("iconsets")
}

pub fn packs_dir() -> PathBuf {
    config::config_dir().join("iconpacks")
}

fn set_path(prefix: &str) -> Option<PathBuf> {
    SETS.iter().any(|(p, _, _)| *p == prefix).then(|| sets_dir().join(format!("{prefix}.json")))
}

// ------------------------------------------------------------------ cache

/// Parsed sets are large; keep them only briefly after use.
const SET_TTL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Cache {
    sets: HashMap<String, (Instant, Arc<Value>)>,
    /// prefix:name -> (svg body, width, height); small and kept for the process lifetime.
    glyphs: HashMap<String, Option<(String, f32, f32)>>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

fn load_set(prefix: &str) -> Option<Arc<Value>> {
    let mut c = cache().lock().unwrap();
    c.sets.retain(|_, (t, _)| t.elapsed() < SET_TTL);
    if let Some((t, v)) = c.sets.get_mut(prefix) {
        *t = Instant::now();
        return Some(v.clone());
    }
    drop(c);
    let text = std::fs::read_to_string(set_path(prefix)?).ok()?;
    let v: Arc<Value> = Arc::new(serde_json::from_str(&text).ok()?);
    cache().lock().unwrap().sets.insert(prefix.to_string(), (Instant::now(), v.clone()));
    Some(v)
}

/// Looks up an icon (following aliases) and returns (body, width, height).
fn lookup(set: &Value, name: &str) -> Option<(String, f32, f32)> {
    let dim = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).map(|n| n as f32);
    let (dw, dh) = (dim(set, "width").unwrap_or(16.0), dim(set, "height").unwrap_or(16.0));
    let mut name = name.to_string();
    for _ in 0..4 {
        if let Some(icon) = set.pointer(&format!("/icons/{name}")) {
            let body = icon.get("body")?.as_str()?.to_string();
            return Some((body, dim(icon, "width").unwrap_or(dw), dim(icon, "height").unwrap_or(dh)));
        }
        name = set.pointer(&format!("/aliases/{name}/parent"))?.as_str()?.to_string();
    }
    None
}

fn glyph(prefix: &str, name: &str) -> Option<(String, f32, f32)> {
    let key = format!("{prefix}:{name}");
    if let Some(g) = cache().lock().unwrap().glyphs.get(&key) {
        return g.clone();
    }
    let g = load_set(prefix).and_then(|set| lookup(&set, name));
    cache().lock().unwrap().glyphs.insert(key, g.clone());
    g
}

/// Builds a standalone SVG for an icon, coloured and padded so the glyph fills `scale` of the box.
pub fn svg(prefix: &str, name: &str, color: &str, scale: f32) -> Option<String> {
    let (body, w, h) = glyph(prefix, name)?;
    let scale = scale.clamp(0.1, 1.0);
    let side = w.max(h) / scale;
    let (x, y) = (-(side - w) / 2.0, -(side - h) / 2.0);
    let color = if color.starts_with('#') && color.len() <= 9 && color[1..].chars().all(|c| c.is_ascii_hexdigit()) { color } else { "#ffffff" };
    Some(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x} {y} {side} {side}" width="256" height="256" color="{color}" fill="{color}">{body}</svg>"#
    ))
}

/// Parses `glyph:prefix:name?color=..&scale=..` into an SVG document.
pub fn resolve_ref(src: &str) -> Option<String> {
    let rest = src.strip_prefix("glyph:")?;
    let (id, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (prefix, name) = id.split_once(':')?;
    let mut color = "#ffffff".to_string();
    let mut scale = 0.6f32;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("color", v)) => color = v.replace("%23", "#"),
            Some(("scale", v)) => scale = v.parse().unwrap_or(scale),
            _ => {}
        }
    }
    svg(prefix, name, &color, scale)
}

// ------------------------------------------------------------------ sets

pub fn list_sets() -> Vec<Value> {
    SETS.iter()
        .map(|(prefix, name, license)| {
            let path = sets_dir().join(format!("{prefix}.json"));
            let size = std::fs::metadata(&path).map(|m| m.len()).ok();
            json!({ "prefix": prefix, "name": name, "license": license, "installed": size.is_some(), "bytes": size })
        })
        .collect()
}

/// Downloads an Iconify set from the npm CDN.
pub async fn install_set(prefix: &str) -> Result<()> {
    let dest = set_path(prefix).context("unknown icon set")?;
    std::fs::create_dir_all(sets_dir())?;
    let tmp = dest.with_extension("json.download");
    let url = format!("https://cdn.jsdelivr.net/npm/@iconify-json/{prefix}@1/icons.json");
    let status = tokio::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "120", "-o"])
        .arg(&tmp)
        .arg(&url)
        .status()
        .await
        .context("running curl")?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("download failed ({status}) from {url}");
    }
    // Make sure it really is an icon set before keeping it.
    let text = std::fs::read_to_string(&tmp)?;
    let v: Value = serde_json::from_str(&text).context("downloaded file is not JSON")?;
    if v.get("icons").and_then(Value::as_object).is_none() {
        let _ = std::fs::remove_file(&tmp);
        bail!("downloaded file is not an Iconify icon set");
    }
    std::fs::rename(&tmp, &dest)?;
    let mut c = cache().lock().unwrap();
    c.sets.remove(prefix);
    c.glyphs.retain(|k, _| !k.starts_with(&format!("{prefix}:")));
    Ok(())
}

pub fn remove_set(prefix: &str) -> Result<()> {
    let path = set_path(prefix).context("unknown icon set")?;
    std::fs::remove_file(path)?;
    cache().lock().unwrap().sets.remove(prefix);
    Ok(())
}

/// Searches installed sets (or one set) by icon name. Returns ready-to-show SVGs.
pub fn search(query: &str, only: Option<&str>, limit: usize) -> Vec<Value> {
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(String::from).collect();
    let mut out = Vec::new();
    for (prefix, _, _) in SETS {
        if only.is_some_and(|o| o != *prefix) {
            continue;
        }
        let Some(set) = load_set(prefix) else { continue };
        let Some(icons) = set.get("icons").and_then(Value::as_object) else { continue };
        let mut names: Vec<&String> = icons.keys().filter(|n| words.iter().all(|w| n.contains(w.as_str()))).collect();
        // Exact and shorter names first: "home" before "home-alert-outline".
        names.sort_by_key(|n| (words.first().is_none_or(|w| !n.starts_with(w.as_str())), n.len()));
        for name in names.into_iter().take(limit.saturating_sub(out.len())) {
            if let Some((body, w, h)) = lookup(&set, name) {
                let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" fill="currentColor">{body}</svg>"#);
                out.push(json!({ "icon": format!("{prefix}:{name}"), "svg": svg }));
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    // Elgato icon packs: plain image files, matched by name and tags.
    for pack in list_packs() {
        if only.is_some_and(|o| o != format!("pack:{}", pack["id"].as_str().unwrap_or_default())) {
            continue;
        }
        for icon in pack["icons"].as_array().into_iter().flatten() {
            if out.len() >= limit {
                break;
            }
            let hay = format!("{} {}", icon["name"].as_str().unwrap_or_default(), icon["tags"]).to_lowercase();
            if words.iter().all(|w| hay.contains(w.as_str())) {
                out.push(json!({ "path": icon["path"], "name": icon["name"] }));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ Elgato icon packs

/// Installed `.sdIconPack` folders with their icon lists.
pub fn list_packs() -> Vec<Value> {
    let mut packs = Vec::new();
    for entry in std::fs::read_dir(packs_dir()).into_iter().flatten().flatten() {
        let dir = entry.path();
        let manifest: Value = config::read_json(&dir.join("manifest.json")).unwrap_or_default();
        let list: Value = config::read_json(&dir.join("icons.json")).unwrap_or_default();
        let icons: Vec<Value> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let path = dir.join("icons").join(i.get("path")?.as_str()?);
                path.exists().then(|| json!({ "path": path.to_string_lossy(), "name": i.get("name"), "tags": i.get("tags") }))
            })
            .collect();
        let id = dir.file_name().unwrap().to_string_lossy().trim_end_matches(".sdIconPack").to_string();
        packs.push(json!({
            "id": id,
            "name": manifest.get("Name").cloned().unwrap_or(json!(id)),
            "author": manifest.get("Author"),
            "icons": icons,
        }));
    }
    packs
}

/// Installs a `.streamDeckIconPack` archive.
pub fn install_pack(archive: &Path) -> Result<String> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).context("not a zip archive")?;
    let top = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok()?.enclosed_name())
        .filter_map(|p| p.components().next().map(|c| c.as_os_str().to_owned()))
        .find(|c| c.to_string_lossy().ends_with(".sdIconPack"))
        .context("archive has no .sdIconPack folder")?;
    std::fs::create_dir_all(packs_dir())?;
    let dest = packs_dir().join(&top);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    zip.extract(packs_dir())?;
    Ok(top.to_string_lossy().into_owned())
}

pub fn remove_pack(id: &str) -> Result<()> {
    if id.contains(['/', '\\']) || id.starts_with('.') {
        bail!("bad pack id");
    }
    std::fs::remove_dir_all(packs_dir().join(format!("{id}.sdIconPack")))?;
    Ok(())
}

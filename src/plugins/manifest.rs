//! Elgato `manifest.json` parsing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Label, LabelPos, State};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawManifest {
    #[serde(rename = "UUID")]
    uuid: Option<String>,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    author: String,
    #[serde(default)]
    icon: String,
    code_path: Option<String>,
    code_path_lin: Option<String>,
    code_path_win: Option<String>,
    /// OpenAction extension: target triple -> binary.
    code_paths: Option<HashMap<String, String>>,
    nodejs: Option<Value>,
    property_inspector_path: Option<String>,
    #[serde(default)]
    actions: Vec<RawAction>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawAction {
    #[serde(rename = "UUID")]
    uuid: String,
    name: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    tooltip: String,
    #[serde(default)]
    controllers: Vec<String>,
    #[serde(default)]
    states: Vec<RawState>,
    encoder: Option<RawEncoder>,
    property_inspector_path: Option<String>,
    #[serde(default = "yes")]
    visible_in_action_list: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct RawState {
    #[serde(default)]
    image: String,
    title: Option<String>,
    #[serde(default = "yes")]
    show_title: bool,
    title_alignment: Option<String>,
    font_size: Option<Value>,
    title_color: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawEncoder {
    layout: Option<String>,
    #[serde(rename = "Icon")]
    icon: Option<String>,
    background: Option<String>,
}

fn yes() -> bool {
    true
}

/// How to start a plugin process.
#[derive(Debug, Clone)]
pub enum Runtime {
    Node(PathBuf),
    Native(PathBuf),
    Html(PathBuf),
    Wine(PathBuf),
}

#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub uuid: String,
    pub name: String,
    pub version: String,
    pub author: String,
    pub icon: Option<String>,
    pub dir: PathBuf,
    pub runtime: Option<Runtime>,
    pub actions: Vec<ActionInfo>,
}

#[derive(Debug, Clone)]
pub struct ActionInfo {
    pub uuid: String,
    pub plugin: String,
    pub name: String,
    pub icon: Option<String>,
    pub tooltip: String,
    pub keypad: bool,
    pub encoder: bool,
    pub states: Vec<State>,
    pub encoder_layout: String,
    pub encoder_icon: Option<String>,
    pub encoder_background: Option<String>,
    pub property_inspector: Option<PathBuf>,
    pub visible: bool,
}

pub fn load(dir: &Path) -> Result<PluginInfo> {
    let text = std::fs::read_to_string(dir.join("manifest.json")).with_context(|| format!("{}", dir.display()))?;
    // Some manifests ship with a BOM.
    let raw: RawManifest = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("parsing {}/manifest.json", dir.display()))?;

    let uuid = raw.uuid.clone().unwrap_or_else(|| {
        dir.file_name().unwrap().to_string_lossy().trim_end_matches(".sdPlugin").to_string()
    });
    let runtime = pick_runtime(dir, &raw);
    let plugin_pi = raw.property_inspector_path.as_ref().map(|p| dir.join(p));

    let actions = raw
        .actions
        .into_iter()
        .map(|a| {
            let controllers = if a.controllers.is_empty() { vec!["Keypad".to_string()] } else { a.controllers };
            let states: Vec<State> = a.states.iter().map(|s| convert_state(dir, s)).collect();
            let enc = a.encoder.as_ref();
            ActionInfo {
                plugin: uuid.clone(),
                name: a.name,
                icon: resolve_image(dir, &a.icon),
                tooltip: a.tooltip,
                keypad: controllers.iter().any(|c| c == "Keypad"),
                encoder: controllers.iter().any(|c| c == "Encoder"),
                encoder_layout: enc.and_then(|e| e.layout.clone()).unwrap_or_else(|| "$X1".into()),
                encoder_icon: enc.and_then(|e| e.icon.as_deref()).and_then(|i| resolve_image(dir, i)),
                encoder_background: enc.and_then(|e| e.background.as_deref()).and_then(|i| resolve_image(dir, i)),
                property_inspector: a.property_inspector_path.map(|p| dir.join(p)).or(plugin_pi.clone()),
                visible: a.visible_in_action_list,
                uuid: a.uuid,
                states,
            }
        })
        .collect();

    Ok(PluginInfo {
        uuid,
        name: raw.name,
        version: raw.version,
        author: raw.author,
        icon: resolve_image(dir, &raw.icon),
        dir: dir.to_path_buf(),
        runtime,
        actions,
    })
}

fn pick_runtime(dir: &Path, m: &RawManifest) -> Option<Runtime> {
    let native = m
        .code_paths
        .as_ref()
        .and_then(|cp| cp.get(&format!("{}-unknown-linux-gnu", std::env::consts::ARCH)))
        .or(m.code_path_lin.as_ref());
    let candidates = native.into_iter().chain(m.code_path.as_ref()).chain(m.code_path_win.as_ref());
    for code in candidates {
        let path = dir.join(code);
        let lower = code.to_ascii_lowercase();
        let rt = if lower.ends_with(".html") || lower.ends_with(".htm") {
            Runtime::Html(path)
        } else if lower.ends_with(".js") || lower.ends_with(".mjs") || lower.ends_with(".cjs") || m.nodejs.is_some() {
            Runtime::Node(path)
        } else if lower.ends_with(".exe") {
            Runtime::Wine(path)
        } else {
            Runtime::Native(path)
        };
        if path_exists(&rt) {
            return Some(rt);
        }
    }
    None
}

fn path_exists(rt: &Runtime) -> bool {
    match rt {
        Runtime::Node(p) | Runtime::Native(p) | Runtime::Html(p) | Runtime::Wine(p) => p.exists(),
    }
}

fn convert_state(dir: &Path, s: &RawState) -> State {
    let mut state = State { image: resolve_image(dir, &s.image), ..Default::default() };
    if let Some(title) = s.title.as_ref().filter(|t| s.show_title && !t.is_empty()) {
        let pos = match s.title_alignment.as_deref() {
            Some("top") => LabelPos::Top,
            Some("middle") => LabelPos::Center,
            _ => LabelPos::Bottom,
        };
        let size = s.font_size.as_ref().and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())).map(|v| v as f32);
        state.labels.insert(pos, Label { text: title.clone(), size, color: s.title_color.clone() });
    }
    state
}

/// Elgato image paths omit the extension; find the best file on disk.
pub fn resolve_image(dir: &Path, rel: &str) -> Option<String> {
    if rel.is_empty() {
        return None;
    }
    let base = dir.join(rel);
    if base.extension().is_some() && base.is_file() {
        return Some(base.to_string_lossy().into_owned());
    }
    let b = base.to_string_lossy();
    ["@2x.png", ".svg", ".png", ".gif", ".jpg", "@2x.jpg"]
        .iter()
        .map(|ext| format!("{b}{ext}"))
        .find(|p| Path::new(p).is_file())
}

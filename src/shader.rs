//! Hyprland screen shaders (port of the StreamController "Hypr Shader" plugin).
//!
//! Shaders are `.glsl` files in `~/.config/hypr/shaders` (and `animated/`).
//! They're applied with `hyprctl eval 'hl.config{...}'`, since the Lua config
//! parser rejects `hyprctl keyword`; `keyword` is tried as a fallback for
//! legacy configs.

use std::path::PathBuf;

const EXCLUDE: &[&str] = &["test-passthrough.glsl", "common.glsl"];

pub fn shaders_dir(folder: &str) -> PathBuf {
    let base = dirs::config_dir().unwrap_or_default().join("hypr/shaders");
    if folder.is_empty() || folder.contains("..") { base } else { base.join(folder) }
}

/// Sorted (name, path) pairs for a shader folder.
pub fn list(folder: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(shaders_dir(folder))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let fname = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            // exists() also skips broken symlinks.
            (fname.ends_with(".glsl") && !EXCLUDE.contains(&fname.as_str()) && path.exists())
                .then(|| (fname.trim_end_matches(".glsl").to_string(), path.to_string_lossy().into_owned()))
        })
        .collect();
    out.sort();
    out
}

async fn hyprctl(args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(std::time::Duration::from_secs(3), tokio::process::Command::new("hyprctl").args(args).output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The active screen shader path, or "" when none.
pub async fn active() -> Option<String> {
    let out = hyprctl(&["getoption", "decoration:screen_shader"]).await?;
    for line in out.lines() {
        if let Some(v) = line.trim().strip_prefix("str:") {
            let v = v.trim();
            return Some(if matches!(v, "[[EMPTY]]" | "UNSET" | "") { String::new() } else { v.to_string() });
        }
    }
    Some(String::new())
}

async fn set_option(section: &str, key: &str, value: &str, is_string: bool) {
    let lua = if is_string { format!("[==[{value}]==]") } else { value.to_string() };
    let expr = format!("hl.config({{ {section} = {{ {key} = {lua} }} }})");
    let out = hyprctl(&["eval", &expr]).await.unwrap_or_default();
    // Legacy (hyprlang) configs don't have eval.
    if out.contains("unknown request") || out.contains("Invalid") {
        let v = if is_string && value.is_empty() { "[[EMPTY]]" } else { value };
        hyprctl(&["keyword", &format!("{section}:{key}"), v]).await;
    }
}

/// Applies a shader, or clears it when `path` is empty.
pub async fn apply(path: &str) {
    if path.is_empty() {
        set_option("decoration", "screen_shader", "", true).await;
        // 2 = full damage tracking, Hyprland's default.
        set_option("debug", "damage_tracking", "2", false).await;
    } else {
        // Screen shaders only re-composite damaged regions, so static layers keep
        // unshaded pixels; full redraws keep the whole output shaded.
        set_option("debug", "damage_tracking", "0", false).await;
        set_option("decoration", "screen_shader", path, true).await;
    }
}

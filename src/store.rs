//! Plugin sources: installing plugins from URLs and from published indexes.
//!
//! A source is a URL to an `index.json` that lists plugins:
//!
//! ```json
//! { "name": "deckd-plugins", "homepage": "https://…",
//!   "plugins": [{ "uuid": "me.example.thing", "name": "Thing", "version": "1.2.0",
//!                 "author": "…", "description": "…", "icon": "data:…",
//!                 "url": "me.example.thing.streamDeckPlugin", "sha256": "…" }] }
//! ```
//!
//! Relative `url`s resolve against the index, so a GitHub release that holds
//! `index.json` next to the packages works as
//! `https://github.com/<owner>/<repo>/releases/latest/download/index.json`; a bare
//! `https://github.com/<owner>/<repo>` is expanded to that. Sources are kept in
//! settings.json under `plugin_sources`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config;

const INDEX_TIMEOUT_S: &str = "20";
const DOWNLOAD_TIMEOUT_S: &str = "300";
const MAX_DOWNLOAD: &str = "512M";

/// Expands GitHub repository links to their latest release's index.
pub fn normalize(url: &str) -> Result<String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("only http(s) URLs are supported");
    }
    if let Some(rest) = url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.trim_end_matches('/').trim_end_matches(".git").split('/').collect();
        if parts.len() == 2 && parts.iter().all(|p| !p.is_empty()) {
            return Ok(format!("https://github.com/{}/{}/releases/latest/download/index.json", parts[0], parts[1]));
        }
    }
    Ok(url.to_string())
}

/// Resolves `href` against the index URL it came from.
fn resolve(base: &str, href: &str) -> String {
    if href.starts_with("https://") || href.starts_with("http://") {
        return href.to_string();
    }
    let dir = base.rsplit_once('/').map_or(base, |(d, _)| d);
    format!("{dir}/{}", href.trim_start_matches("./"))
}

async fn curl(url: &str, args: &[&str]) -> Result<std::process::Output> {
    let out = tokio::process::Command::new("curl")
        // Follow redirects (GitHub release downloads redirect), only ever over http(s).
        .args(["-fsSL", "--proto", "=http,https", "--proto-redir", "=http,https"])
        .args(args)
        .arg(url)
        .output()
        .await
        .context("running curl")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("download failed: {}", err.trim().trim_start_matches("curl: "));
    }
    Ok(out)
}

/// Fetches and checks a source's index, with every plugin URL made absolute.
pub async fn fetch_index(url: &str) -> Result<Value> {
    let out = curl(url, &["--max-time", INDEX_TIMEOUT_S, "--max-filesize", "4M"]).await?;
    let mut index: Value = serde_json::from_slice(&out.stdout).context("the source isn't a plugin index (not JSON)")?;
    let plugins = index.get_mut("plugins").and_then(Value::as_array_mut).context("the source isn't a plugin index (no \"plugins\" list)")?;
    plugins.retain(|p| p.get("uuid").and_then(Value::as_str).is_some() && p.get("url").and_then(Value::as_str).is_some());
    for p in plugins.iter_mut() {
        let href = p["url"].as_str().unwrap_or_default().to_string();
        p["url"] = json!(resolve(url, &href));
    }
    Ok(index)
}

/// Downloads a plugin package to a temporary file, checking its SHA-256 when one is given.
pub async fn download(url: &str, sha256: Option<&str>) -> Result<PathBuf> {
    let url = normalize(url)?;
    let dir = config::config_dir().join("tmp");
    std::fs::create_dir_all(&dir)?;
    let name = url.rsplit('/').next().filter(|n| !n.is_empty()).unwrap_or("download");
    // Keep the extension: the installer tells icon packs from plugins by it.
    let tmp = dir.join(format!("{}-{}", uuid::Uuid::new_v4().simple(), sanitize(name)));
    let tmp_arg = tmp.to_string_lossy().into_owned();
    if let Err(e) = curl(&url, &["--max-time", DOWNLOAD_TIMEOUT_S, "--max-filesize", MAX_DOWNLOAD, "-o", &tmp_arg]).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(want) = sha256.map(str::trim).filter(|s| !s.is_empty()) {
        let got = sha256_file(&tmp)?;
        if !got.eq_ignore_ascii_case(want) {
            let _ = std::fs::remove_file(&tmp);
            bail!("checksum mismatch: the download isn't the file the source lists");
        }
    }
    Ok(tmp)
}

fn sanitize(name: &str) -> String {
    let name = name.split(['?', '#']).next().unwrap_or(name);
    name.chars().map(|c| if c.is_ascii_alphanumeric() || ".-_".contains(c) { c } else { '_' }).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect())
}

// ------------------------------------------------------------------ sources

fn settings_path() -> PathBuf {
    config::config_dir().join("settings.json")
}

pub fn sources() -> Vec<String> {
    let settings: Value = config::read_json(&settings_path()).unwrap_or_default();
    settings["plugin_sources"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

fn save_sources(list: &[String]) -> Result<()> {
    // Other keys in settings.json are left as they are.
    let mut settings: Value = config::read_json(&settings_path()).unwrap_or_default();
    if !settings.is_object() {
        settings = json!({});
    }
    settings["plugin_sources"] = json!(list);
    config::write_json(&settings_path(), &settings)
}

/// Adds a source after checking that it serves an index. Returns its normalized URL.
pub async fn add_source(url: &str) -> Result<String> {
    let url = normalize(url)?;
    fetch_index(&url).await?;
    let mut list = sources();
    if !list.contains(&url) {
        list.push(url.clone());
        save_sources(&list)?;
    }
    Ok(url)
}

pub fn remove_source(url: &str) -> Result<()> {
    let mut list = sources();
    list.retain(|u| u != url);
    save_sources(&list)
}

/// `a` is newer than `b`, comparing dotted numbers (1.10.0 > 1.9.2).
pub fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_repo_links_point_at_the_latest_release() {
        assert_eq!(normalize("https://github.com/a/b").unwrap(), "https://github.com/a/b/releases/latest/download/index.json");
        assert_eq!(normalize("https://github.com/a/b.git/").unwrap(), "https://github.com/a/b/releases/latest/download/index.json");
        let index = "https://github.com/a/b/releases/latest/download/index.json";
        assert_eq!(normalize(index).unwrap(), index);
        assert!(normalize("file:///etc/passwd").is_err());
    }

    #[test]
    fn plugin_urls_resolve_against_the_index() {
        let base = "https://h.example/r/latest/download/index.json";
        assert_eq!(resolve(base, "x.streamDeckPlugin"), "https://h.example/r/latest/download/x.streamDeckPlugin");
        assert_eq!(resolve(base, "./x.zip"), "https://h.example/r/latest/download/x.zip");
        assert_eq!(resolve(base, "https://o.example/y"), "https://o.example/y");
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("1.10.0", "1.9.2"));
        assert!(newer("v2", "1.99"));
        assert!(!newer("1.0.0", "1.0"));
        assert!(!newer("0.9", "1.0"));
    }
}

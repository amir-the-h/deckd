//! JSON API behind the web UI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::audio::AudioWatch;
use crate::config::{self, Page, Profile};
use crate::device::parse_input;
use crate::hub::{Hub, UiCmd};
use crate::plugins::{self, Host};
use crate::render::icons;

#[derive(Clone)]
pub struct AppState {
    pub host: Arc<Host>,
    pub hub: Arc<Hub>,
    pub audio: AudioWatch,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/devices", get(devices))
        .route("/api/devices/{serial}", get(snapshot))
        .route("/api/devices/{serial}/pages/{name}", put(put_page).delete(delete_page))
        .route("/api/devices/{serial}/pages/{name}/rename", post(rename_page))
        .route("/api/devices/{serial}/switch", post(switch_page))
        .route("/api/devices/{serial}/profile", put(set_profile))
        .route("/api/devices/{serial}/render/{page}/{input}", get(render))
        .route("/api/devices/{serial}/inspector/{context}/{op}", post(inspector))
        .route("/api/actions", get(catalog))
        .route("/api/plugins", get(list_plugins).post(install_plugin).layer(axum::extract::DefaultBodyLimit::max(512 << 20)))
        .route("/api/plugins/{uuid}", axum::routing::delete(uninstall_plugin))
        .route("/api/plugins/url", post(install_from_url))
        .route("/api/store", get(store))
        .route("/api/store/sources", post(add_source).delete(remove_source))
        .route("/api/audio", get(audio_devices))
        .route("/api/windows", get(list_windows))
        .route("/api/icons", get(list_icons).post(upload_icon).layer(axum::extract::DefaultBodyLimit::max(8 << 20)))
        .route("/api/events", get(events))
        .route("/api/iconsets", get(list_iconsets))
        .route("/api/iconsets/search", get(search_icons))
        .route("/api/iconsets/{prefix}", post(install_iconset).delete(remove_iconset))
        .route("/api/iconpacks/{id}", axum::routing::delete(remove_iconpack))
        .route("/api/glyph", get(glyph))
        .route("/files", get(file))
}

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn not_found(what: &str) -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, format!("{what} not found"))
}

fn bad(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

/// Sends a command to a deck's actor and waits for the reply.
async fn ask<T>(s: &AppState, serial: &str, make: impl FnOnce(oneshot::Sender<T>) -> UiCmd) -> ApiResult<T> {
    let deck = s.hub.deck(serial).ok_or_else(|| not_found("device"))?;
    let (tx, rx) = oneshot::channel();
    deck.tx.send(make(tx)).map_err(|_| not_found("device"))?;
    rx.await.map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "device went away".into()))
}

async fn devices(State(s): State<AppState>) -> Json<Value> {
    let list: Vec<Value> = s
        .hub
        .decks()
        .into_iter()
        .map(|(serial, d)| {
            let (rows, cols) = d.kind.key_layout();
            json!({
                "serial": serial,
                "kind": format!("{:?}", d.kind),
                "rows": rows,
                "columns": cols,
                "keys": d.kind.key_count(),
                "dials": d.kind.encoder_count(),
                "lcd": d.kind.lcd_strip_size(),
                "keySize": d.kind.key_image_format().size.0,
            })
        })
        .collect();
    Json(json!(list))
}

async fn snapshot(State(s): State<AppState>, UrlPath(serial): UrlPath<String>) -> ApiResult<Json<Value>> {
    let snap = ask(&s, &serial, UiCmd::Snapshot).await?;
    Ok(Json(serde_json::to_value(snap).unwrap()))
}

async fn put_page(State(s): State<AppState>, UrlPath((serial, name)): UrlPath<(String, String)>, Json(page): Json<Page>) -> ApiResult<Json<Page>> {
    let page = ask(&s, &serial, |reply| UiCmd::PutPage { name, page, reply }).await?.map_err(bad)?;
    Ok(Json(page))
}

async fn delete_page(State(s): State<AppState>, UrlPath((serial, name)): UrlPath<(String, String)>) -> ApiResult<StatusCode> {
    ask(&s, &serial, |reply| UiCmd::DeletePage { name, reply }).await?.map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RenameBody {
    to: String,
}

async fn rename_page(State(s): State<AppState>, UrlPath((serial, from)): UrlPath<(String, String)>, Json(b): Json<RenameBody>) -> ApiResult<StatusCode> {
    ask(&s, &serial, |reply| UiCmd::RenamePage { from, to: b.to, reply }).await?.map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SwitchBody {
    page: String,
}

async fn switch_page(State(s): State<AppState>, UrlPath(serial): UrlPath<String>, Json(b): Json<SwitchBody>) -> ApiResult<StatusCode> {
    let deck = s.hub.deck(&serial).ok_or_else(|| not_found("device"))?;
    let _ = deck.tx.send(UiCmd::Switch { page: b.page });
    Ok(StatusCode::NO_CONTENT)
}

async fn set_profile(State(s): State<AppState>, UrlPath(serial): UrlPath<String>, Json(profile): Json<Profile>) -> ApiResult<StatusCode> {
    ask(&s, &serial, |reply| UiCmd::SetProfile { profile, reply }).await?.map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn render(State(s): State<AppState>, UrlPath((serial, page, input)): UrlPath<(String, String, String)>) -> ApiResult<Response> {
    let input = parse_input(&input).ok_or_else(|| bad("bad input"))?;
    let png = ask(&s, &serial, |reply| UiCmd::Render { page, input, reply }).await?.ok_or_else(|| not_found("slot"))?;
    Ok(([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-store")], png).into_response())
}

/// Everything a property inspector page needs to call `connectElgatoStreamDeckSocket`.
async fn inspector(State(s): State<AppState>, UrlPath((serial, context, op)): UrlPath<(String, String, String)>) -> ApiResult<Json<Value>> {
    let open = op == "open";
    let action_info = ask(&s, &serial, |reply| UiCmd::Inspector { context: context.clone(), open, reply }).await?.map_err(bad)?;
    if !open {
        return Ok(Json(json!({})));
    }
    let uuid = action_info["action"].as_str().unwrap_or_default().to_string();
    let action = s.host.action(&uuid).ok_or_else(|| not_found("action"))?;
    let url = action.property_inspector.as_deref().and_then(plugin_url);
    Ok(Json(json!({
        "url": url,
        "port": s.host.port(),
        "uuid": context,
        "registerEvent": "registerPropertyInspector",
        "info": s.host.app_info(&action.plugin),
        "actionInfo": action_info,
    })))
}

/// Built-in actions, described the same way as plugin actions.
fn builtins() -> Vec<Value> {
    let svg = |s: &str| format!("data:image/svg+xml;base64,{}", base64_encode(s));
    let icon = |path: &str| svg(&format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><path fill="#fff" d="{path}"/></svg>"##));
    vec![
        json!({ "uuid": "deckd.page", "name": "Switch Page", "tooltip": "Go to another page, or back", "keypad": true, "encoder": true,
                "icon": icon("M10 6 8.59 7.41 13.17 12l-4.58 4.59L10 18l6-6z") }),
        json!({ "uuid": "deckd.command", "name": "Run Command", "tooltip": "Run a shell command", "keypad": true, "encoder": true,
                "icon": icon("M20 4H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2zm0 14H4V8h16v10zM6 10l3 3-3 3 1.4 1.4L11.8 13 7.4 8.6zM12 15h6v2h-6z") }),
        json!({ "uuid": "deckd.volume", "name": "Volume", "tooltip": "Audio device volume and mute", "keypad": true, "encoder": true,
                "icon": svg(icons::SPEAKER) }),
        json!({ "uuid": "deckd.dial", "name": "Dial Commands", "tooltip": "Run commands when a dial is turned, pushed or touched", "keypad": false, "encoder": true,
                "icon": icon("M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 18a8 8 0 1 1 0-16 8 8 0 0 1 0 16zm1-13h-2v6l5.2 3.2 1-1.6-4.2-2.5z") }),
        json!({ "uuid": "deckd.clock", "name": "Clock", "tooltip": "Time or date, digital or analog", "keypad": true, "encoder": true,
                "icon": icon("M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20zm0 18a8 8 0 1 1 0-16 8 8 0 0 1 0 16zm.5-13H11v6l5.2 3.2.8-1.3-4.5-2.7z") }),
        json!({ "uuid": "deckd.media", "name": "Now Playing", "tooltip": "Track, artist and cover art; press to play/pause, turn to skip", "keypad": true, "encoder": true,
                "icon": icon("M12 3v10.55A4 4 0 1 0 14 17V7h4V3h-6z") }),
        json!({ "uuid": "deckd.mediakey", "name": "Media Key", "tooltip": "Play/pause, skip, stop or seek the current media player", "keypad": true, "encoder": true,
                "icon": icon("M3 5v14l8-7zm10 0h3v14h-3zm5 0h3v14h-3z") }),
        json!({ "uuid": "deckd.mangohud", "name": "MangoHud", "tooltip": "Toggle the HUD or show a live game stat", "keypad": true, "encoder": true,
                "fields": crate::widgets::MANGO_FIELDS,
                "icon": icon("M21 6H3a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h18a1 1 0 0 0 1-1V7a1 1 0 0 0-1-1zM10 13H8v2H6v-2H4v-2h2V9h2v2h2v2zm4.5 2a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3zm3-3a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3z") }),
        json!({ "uuid": "deckd.brightness", "name": "Brightness", "tooltip": "Change the deck's brightness", "keypad": true, "encoder": true,
                "icon": icon("M20 8.69V4h-4.69L12 .69 8.69 4H4v4.69L.69 12 4 15.31V20h4.69L12 23.31 15.31 20H20v-4.69L23.31 12 20 8.69zM12 18a6 6 0 1 1 0-12 6 6 0 0 1 0 12z") }),
    ]
}

async fn catalog(State(s): State<AppState>) -> Json<Value> {
    let mut groups = vec![json!({ "plugin": "deckd", "name": "Built-in", "actions": builtins() })];
    for p in s.host.plugins() {
        let runnable = p.runtime.is_some();
        let actions: Vec<Value> = p
            .actions
            .iter()
            .filter(|a| a.visible)
            .map(|a| {
                json!({
                    "uuid": a.uuid,
                    "name": a.name,
                    "tooltip": a.tooltip,
                    "keypad": a.keypad,
                    "encoder": a.encoder,
                    "icon": a.icon.as_deref().and_then(image_url),
                    "inspector": a.property_inspector.is_some(),
                })
            })
            .collect();
        groups.push(json!({
            "plugin": p.uuid,
            "name": p.name,
            "author": p.author,
            "version": p.version,
            "icon": p.icon.as_deref().and_then(image_url),
            "runnable": runnable,
            "actions": actions,
        }));
    }
    Json(json!(groups))
}

async fn list_plugins(State(s): State<AppState>) -> Json<Value> {
    let list: Vec<Value> = s
        .host
        .plugins()
        .iter()
        .map(|p| {
            json!({
                "uuid": p.uuid,
                "name": p.name,
                "version": p.version,
                "author": p.author,
                "icon": p.icon.as_deref().and_then(image_url),
                "runtime": plugins::runtime_name(&p.runtime),
                "running": s.host.is_running(&p.uuid),
                // Linked for development: installs from a source must not replace it.
                "linked": std::fs::symlink_metadata(&p.dir).is_ok_and(|m| m.file_type().is_symlink()),
                "actions": p.actions.iter().map(|a| a.uuid.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(json!(list))
}

#[derive(Deserialize)]
struct InstallQuery {
    name: String,
}

/// Accepts an uploaded `.streamDeckPlugin` (a zip) and installs it.
async fn install_plugin(State(s): State<AppState>, Query(q): Query<InstallQuery>, body: Bytes) -> ApiResult<Json<Value>> {
    if body.len() > 512 << 20 {
        return Err(bad("file too large"));
    }
    let tmp_dir = config::config_dir().join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(bad)?;
    let name = Path::new(&q.name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "upload.zip".into());
    let tmp = tmp_dir.join(format!("{}-{name}", std::process::id()));
    std::fs::write(&tmp, &body).map_err(bad)?;
    install_file(&s, tmp, &name).await
}

/// Installs a downloaded or uploaded package (plugin or icon pack, told apart by `name`), then deletes it.
async fn install_file(s: &AppState, tmp: PathBuf, name: &str) -> ApiResult<Json<Value>> {
    if name.ends_with(".streamDeckIconPack") {
        let t = tmp.clone();
        let result = tokio::task::spawn_blocking(move || crate::icons::install_pack(&t)).await.map_err(bad)?;
        let _ = std::fs::remove_file(&tmp);
        let id = result.map_err(|e| bad(format!("{e:#}")))?;
        return Ok(Json(json!({ "iconPack": id })));
    }
    let host = s.host.clone();
    let t = tmp.clone();
    let result = tokio::task::spawn_blocking(move || host.install(&t)).await.map_err(bad)?;
    let _ = std::fs::remove_file(&tmp);
    let p = result.map_err(|e| bad(format!("{e:#}")))?;
    s.hub.plugins_loaded(vec![p.uuid.clone()]);
    Ok(Json(json!({ "uuid": p.uuid, "name": p.name, "version": p.version, "runtime": plugins::runtime_name(&p.runtime) })))
}

#[derive(Deserialize)]
struct UrlInstall {
    url: String,
    #[serde(default)]
    sha256: Option<String>,
}

/// Downloads a `.streamDeckPlugin` (or icon pack) from a URL and installs it.
async fn install_from_url(State(s): State<AppState>, Json(req): Json<UrlInstall>) -> ApiResult<Json<Value>> {
    let tmp = crate::store::download(&req.url, req.sha256.as_deref()).await.map_err(|e| bad(format!("{e:#}")))?;
    let name = tmp.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    install_file(&s, tmp, &name).await
}

/// Every source's plugins, each marked with what's installed here.
async fn store(State(s): State<AppState>) -> Json<Value> {
    let installed: std::collections::HashMap<String, (String, bool)> = s
        .host
        .plugins()
        .iter()
        .map(|p| (p.uuid.clone(), (p.version.clone(), std::fs::symlink_metadata(&p.dir).is_ok_and(|m| m.file_type().is_symlink()))))
        .collect();
    let urls = crate::store::sources();
    let indexes = futures_util::future::join_all(urls.iter().map(|u| crate::store::fetch_index(u))).await;
    let sources: Vec<Value> = urls
        .iter()
        .zip(indexes)
        .map(|(url, index)| match index {
            Ok(mut index) => {
                for p in index["plugins"].as_array_mut().into_iter().flatten() {
                    let uuid = p["uuid"].as_str().unwrap_or_default().to_string();
                    if let Some((version, linked)) = installed.get(&uuid) {
                        let offered = p["version"].as_str().unwrap_or_default().to_string();
                        p["installed"] = json!(version);
                        p["linked"] = json!(linked);
                        p["update"] = json!(!linked && crate::store::newer(&offered, version));
                    }
                }
                json!({ "url": url, "name": index["name"], "homepage": index["homepage"], "plugins": index["plugins"] })
            }
            Err(e) => json!({ "url": url, "error": format!("{e:#}"), "plugins": [] }),
        })
        .collect();
    Json(json!({ "sources": sources }))
}

#[derive(Deserialize)]
struct SourceReq {
    url: String,
}

async fn add_source(Json(req): Json<SourceReq>) -> ApiResult<Json<Value>> {
    let url = crate::store::add_source(&req.url).await.map_err(|e| bad(format!("{e:#}")))?;
    Ok(Json(json!({ "url": url })))
}

async fn remove_source(Query(req): Query<SourceReq>) -> ApiResult<StatusCode> {
    crate::store::remove_source(&req.url).map_err(bad)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn uninstall_plugin(State(s): State<AppState>, UrlPath(uuid): UrlPath<String>) -> ApiResult<StatusCode> {
    let host = s.host.clone();
    tokio::task::spawn_blocking(move || host.uninstall(&uuid)).await.map_err(bad)?.map_err(|e| bad(format!("{e:#}")))?;
    s.hub.emit(json!({ "type": "plugins" }));
    Ok(StatusCode::NO_CONTENT)
}

async fn list_iconsets() -> Json<Value> {
    let packs: Vec<Value> = crate::icons::list_packs()
        .into_iter()
        .map(|p| json!({ "id": p["id"], "name": p["name"], "author": p["author"], "count": p["icons"].as_array().map(|a| a.len()) }))
        .collect();
    Json(json!({ "sets": crate::icons::list_sets(), "packs": packs }))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    set: Option<String>,
}

async fn search_icons(Query(q): Query<SearchQuery>) -> Json<Value> {
    let results = tokio::task::spawn_blocking(move || {
        let mut r = crate::icons::search(&q.q, q.set.as_deref().filter(|s| !s.is_empty()), 240);
        for item in &mut r {
            if let Some(path) = item["path"].as_str().map(String::from) {
                item["url"] = json!(image_url(&path));
            }
        }
        r
    })
    .await
    .unwrap_or_default();
    Json(json!(results))
}

async fn install_iconset(State(s): State<AppState>, UrlPath(prefix): UrlPath<String>) -> ApiResult<StatusCode> {
    crate::icons::install_set(&prefix).await.map_err(|e| bad(format!("{e:#}")))?;
    s.hub.emit(json!({ "type": "iconsets" }));
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_iconset(State(s): State<AppState>, UrlPath(prefix): UrlPath<String>) -> ApiResult<StatusCode> {
    crate::icons::remove_set(&prefix).map_err(bad)?;
    s.hub.emit(json!({ "type": "iconsets" }));
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_iconpack(State(s): State<AppState>, UrlPath(id): UrlPath<String>) -> ApiResult<StatusCode> {
    crate::icons::remove_pack(&id).map_err(bad)?;
    s.hub.emit(json!({ "type": "iconsets" }));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct GlyphQuery {
    icon: String,
}

/// Renders a `glyph:` reference as SVG for previews in the UI.
async fn glyph(Query(q): Query<GlyphQuery>) -> ApiResult<Response> {
    let svg = tokio::task::spawn_blocking(move || crate::icons::resolve_ref(&q.icon)).await.ok().flatten().ok_or_else(|| not_found("icon"))?;
    Ok(([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response())
}

async fn list_windows() -> Json<Value> {
    let list: Vec<Value> = crate::focus::windows().await.into_iter().map(|(class, title)| json!({ "class": class, "title": title })).collect();
    Json(json!(list))
}

async fn audio_devices(State(s): State<AppState>) -> Json<Value> {
    let state = s.audio.borrow().clone();
    let mut list: Vec<Value> = state
        .devices
        .iter()
        .map(|(name, v)| {
            json!({
                "name": name,
                "description": v.description,
                "kind": if name.starts_with("alsa_input") || name.contains(".monitor") || name.contains("input") { "source" } else { "sink" },
                "monitor": name.ends_with(".monitor"),
            })
        })
        .collect();
    list.sort_by(|a, b| a["description"].as_str().cmp(&b["description"].as_str()));
    Json(json!(list))
}

fn icons_dir() -> PathBuf {
    config::config_dir().join("icons")
}

async fn list_icons() -> Json<Value> {
    let mut list: Vec<Value> = std::fs::read_dir(icons_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_image(p))
        .map(|p| json!({ "path": p.to_string_lossy(), "url": image_url(&p.to_string_lossy()) }))
        .collect();
    list.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    Json(json!(list))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
}

async fn upload_icon(Query(q): Query<UploadQuery>, body: Bytes) -> ApiResult<Json<Value>> {
    let name = Path::new(&q.name).file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| bad("bad name"))?;
    let path = icons_dir().join(&name);
    if !is_image(&path) {
        return Err(bad("only png, jpg, gif, svg and webp images"));
    }
    if body.len() > 8 << 20 {
        return Err(bad("image too large"));
    }
    std::fs::create_dir_all(icons_dir()).map_err(bad)?;
    std::fs::write(&path, &body).map_err(bad)?;
    Ok(Json(json!({ "path": path.to_string_lossy(), "url": image_url(&path.to_string_lossy()) })))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

/// Serves images referenced by absolute path in the config, but only from deckd's own folders.
async fn file(Query(q): Query<FileQuery>) -> ApiResult<Response> {
    let path = std::fs::canonicalize(&q.path).map_err(|_| not_found("file"))?;
    let root = std::fs::canonicalize(config::config_dir()).map_err(|_| not_found("file"))?;
    if !path.starts_with(&root) || !is_image(&path) {
        return Err((StatusCode::FORBIDDEN, "not allowed".into()));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|_| not_found("file"))?;
    Ok(([(header::CONTENT_TYPE, mime_for(&path))], bytes).into_response())
}

async fn events(State(s): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let mut rx = s.hub.subscribe();
    ws.on_upgrade(move |mut socket: WebSocket| async move {
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(text) => if socket.send(Message::Text(text.into())).await.is_err() { break },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                incoming = socket.recv() => if !matches!(incoming, Some(Ok(_))) { break },
            }
        }
    })
}

// ------------------------------------------------------------------ helpers

fn plugin_url(path: &Path) -> Option<String> {
    let rel = path.strip_prefix(plugins::plugins_dir()).ok()?;
    Some(format!("/plugins/{}", rel.to_string_lossy()))
}

/// A browser-loadable URL for an image reference from the config or a manifest.
pub fn image_url(src: &str) -> Option<String> {
    if src.starts_with("data:") || src.starts_with("http") {
        return Some(src.to_string());
    }
    let p = Path::new(src);
    plugin_url(p).or_else(|| Some(format!("/files?path={}", urlencode(src))))
}

fn is_image(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp")
    )
}

fn mime_for(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn base64_encode(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s)
}

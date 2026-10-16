//! Elgato Stream Deck SDK host: launches plugins and speaks their WebSocket protocol.
//!
//! Plugins are started lazily, the first time a message is addressed to one of
//! their actions, so installed-but-unused plugins cost no memory. Messages sent
//! before a plugin has registered are queued and flushed on registration.

pub mod manifest;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

pub use manifest::{ActionInfo, PluginInfo, Runtime};

use crate::config;

/// Elgato's device type ids.
pub fn device_type(kind: elgato_streamdeck::info::Kind) -> u8 {
    use elgato_streamdeck::info::Kind::*;
    match kind {
        Original | OriginalV2 | Mk2 | Mk2Scissor | Mk2Module => 0,
        Mini | MiniMk2 | MiniDiscord | MiniMk2Module => 1,
        Xl | XlV2 | XlV2Module => 2,
        Pedal => 5,
        Plus => 7,
        Neo => 9,
        PlusXl => 11,
    }
}

/// A message for a deck actor, originating from a plugin or property inspector.
#[derive(Debug)]
pub enum ToDeck {
    FromPlugin(Value),
    FromInspector(Value),
}

struct DeckEntry {
    tx: mpsc::UnboundedSender<ToDeck>,
    info: Value,
}

#[derive(Default)]
struct Inner {
    conns: HashMap<String, mpsc::UnboundedSender<String>>,
    queued: HashMap<String, Vec<String>>,
    running: HashSet<String>,
    /// context -> (deck serial, action uuid)
    contexts: HashMap<String, (String, String)>,
    decks: HashMap<String, DeckEntry>,
    inspectors: HashMap<String, mpsc::UnboundedSender<String>>,
    stops: HashMap<String, Arc<tokio::sync::Notify>>,
}

/// Installed plugins and their actions. Swapped under a lock when plugins are
/// installed or removed at runtime.
#[derive(Default)]
struct Catalog {
    plugins: BTreeMap<String, Arc<PluginInfo>>,
    actions: HashMap<String, Arc<ActionInfo>>,
}

impl Catalog {
    fn insert(&mut self, p: PluginInfo) {
        self.remove(&p.uuid);
        for a in &p.actions {
            self.actions.insert(a.uuid.clone(), Arc::new(a.clone()));
        }
        self.plugins.insert(p.uuid.clone(), Arc::new(p));
    }

    fn remove(&mut self, uuid: &str) -> Option<Arc<PluginInfo>> {
        let p = self.plugins.remove(uuid)?;
        self.actions.retain(|_, a| a.plugin != uuid);
        Some(p)
    }
}

pub struct Host {
    catalog: std::sync::RwLock<Catalog>,
    /// Plugin links whose target was missing at startup (e.g. a network share
    /// not mounted yet); [`Host::load_pending`] retries them.
    pending: Mutex<Vec<PathBuf>>,
    port: u16,
    node: Option<PathBuf>,
    inner: Mutex<Inner>,
}

pub fn plugins_dir() -> PathBuf {
    config::config_dir().join("plugins")
}

fn load_plugin(dir: &Path) -> Option<PluginInfo> {
    match manifest::load(dir) {
        Ok(p) => {
            tracing::info!("plugin {} {} ({} actions, {})", p.uuid, p.version, p.actions.len(), runtime_name(&p.runtime));
            Some(p)
        }
        Err(e) => {
            tracing::warn!("skipping {}: {e:#}", dir.display());
            None
        }
    }
}

fn data_dir() -> PathBuf {
    config::config_dir().join("plugin-data")
}

impl Host {
    pub fn load(port: u16) -> Arc<Host> {
        let mut catalog = Catalog::default();
        let mut pending = Vec::new();
        if let Ok(entries) = std::fs::read_dir(plugins_dir()) {
            for entry in entries.flatten() {
                let dir = entry.path();
                // Dot-folders are installs in progress.
                if entry.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                if !dir.is_dir() {
                    if dir.is_symlink() {
                        let target = std::fs::read_link(&dir).unwrap_or_default();
                        tracing::warn!("plugin link {} points to missing {}; will load it when it appears", dir.display(), target.display());
                        pending.push(dir);
                    }
                    continue;
                }
                if let Some(p) = load_plugin(&dir) {
                    catalog.insert(p);
                }
            }
        }
        Arc::new(Host { catalog: std::sync::RwLock::new(catalog), pending: Mutex::new(pending), port, node: find_node(), inner: Mutex::new(Inner::default()) })
    }

    /// Whether some plugin links were missing at startup and are still being retried.
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().unwrap().is_empty()
    }

    /// Loads the pending plugin links whose targets have appeared. Returns the
    /// UUIDs of the plugins that were added.
    pub fn load_pending(&self) -> Vec<String> {
        let ready: Vec<PathBuf> = {
            let mut pending = self.pending.lock().unwrap();
            let (ready, still): (Vec<_>, Vec<_>) = pending.drain(..).partition(|d| d.is_dir() || !d.is_symlink());
            *pending = still;
            ready
        };
        let mut added = Vec::new();
        for dir in ready.iter().filter(|d| d.is_dir()) {
            if let Some(p) = load_plugin(dir) {
                if self.plugin(&p.uuid).is_none() {
                    added.push(p.uuid.clone());
                    self.catalog.write().unwrap().insert(p);
                }
            }
        }
        added
    }

    pub fn action(&self, uuid: &str) -> Option<Arc<ActionInfo>> {
        self.catalog.read().unwrap().actions.get(uuid).cloned()
    }

    pub fn plugin(&self, uuid: &str) -> Option<Arc<PluginInfo>> {
        self.catalog.read().unwrap().plugins.get(uuid).cloned()
    }

    pub fn plugins(&self) -> Vec<Arc<PluginInfo>> {
        self.catalog.read().unwrap().plugins.values().cloned().collect()
    }

    pub fn is_running(&self, uuid: &str) -> bool {
        self.inner.lock().unwrap().conns.contains_key(uuid)
    }

    /// Installs (or upgrades) a plugin from an archive or folder and makes it available immediately.
    pub fn install(self: &Arc<Self>, src: &Path) -> anyhow::Result<Arc<PluginInfo>> {
        let dir = crate::installer::install(src)?;
        let info = manifest::load(&dir)?;
        let uuid = info.uuid.clone();
        let was_running = self.stop(&uuid);
        self.catalog.write().unwrap().insert(info);
        tracing::info!("installed plugin {uuid}");
        if was_running {
            // Bring the new version up for the actions that are on screen.
            self.inner.lock().unwrap().running.insert(uuid.clone());
            self.clone().spawn(uuid.clone());
        }
        self.plugin(&uuid).ok_or_else(|| anyhow::anyhow!("plugin vanished"))
    }

    /// Stops and deletes a plugin. Its saved settings are kept in plugin-data.
    pub fn uninstall(&self, uuid: &str) -> anyhow::Result<()> {
        let p = self.catalog.read().unwrap().plugins.get(uuid).cloned().ok_or_else(|| anyhow::anyhow!("no such plugin"))?;
        self.stop(uuid);
        self.catalog.write().unwrap().remove(uuid);
        // Only delete inside our own plugins folder.
        if p.dir.starts_with(plugins_dir()) {
            std::fs::remove_dir_all(&p.dir)?;
        }
        tracing::info!("uninstalled plugin {uuid}");
        Ok(())
    }

    /// Stops a running plugin process. Returns whether it was running.
    fn stop(&self, uuid: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let was = inner.running.remove(uuid);
        inner.queued.remove(uuid);
        inner.conns.remove(uuid);
        if let Some(stop) = inner.stops.remove(uuid) {
            stop.notify_one();
        }
        was
    }

    pub fn register_deck(&self, serial: &str, kind: elgato_streamdeck::info::Kind, tx: mpsc::UnboundedSender<ToDeck>) {
        let (rows, cols) = kind.key_layout();
        let info = json!({
            "id": serial,
            "name": format!("Stream Deck {kind:?}"),
            "size": { "columns": cols, "rows": rows },
            "type": device_type(kind),
        });
        let msg = json!({ "event": "deviceDidConnect", "device": serial, "deviceInfo": info }).to_string();
        let mut inner = self.inner.lock().unwrap();
        for conn in inner.conns.values() {
            let _ = conn.send(msg.clone());
        }
        inner.decks.insert(serial.to_string(), DeckEntry { tx, info });
    }

    pub fn unregister_deck(&self, serial: &str) {
        let msg = json!({ "event": "deviceDidDisconnect", "device": serial }).to_string();
        let mut inner = self.inner.lock().unwrap();
        inner.decks.remove(serial);
        inner.contexts.retain(|_, (s, _)| s != serial);
        for conn in inner.conns.values() {
            let _ = conn.send(msg.clone());
        }
    }

    pub fn register_context(&self, context: &str, serial: &str, action: &str) {
        self.inner.lock().unwrap().contexts.insert(context.to_string(), (serial.to_string(), action.to_string()));
    }

    /// Sends an event to whichever plugin owns `action`, starting it if needed.
    pub fn send_to_action(self: &Arc<Self>, action: &str, msg: Value) {
        let Some(plugin) = self.action(action).map(|a| a.plugin.clone()) else { return };
        let text = msg.to_string();
        let mut inner = self.inner.lock().unwrap();
        if let Some(conn) = inner.conns.get(&plugin) {
            let _ = conn.send(text);
            return;
        }
        let queue = inner.queued.entry(plugin.clone()).or_default();
        if queue.len() < 1000 {
            queue.push(text);
        }
        if inner.running.insert(plugin.clone()) {
            drop(inner);
            self.clone().spawn(plugin);
        }
    }

    pub fn send_to_inspector(&self, context: &str, msg: Value) {
        if let Some(conn) = self.inner.lock().unwrap().inspectors.get(context) {
            let _ = conn.send(msg.to_string());
        }
    }

    fn spawn(self: Arc<Self>, plugin: String) {
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let started = Instant::now();
                let stop = Arc::new(tokio::sync::Notify::new());
                self.inner.lock().unwrap().stops.insert(plugin.clone(), stop.clone());
                tokio::select! {
                    r = self.launch(&plugin) => match r {
                        Ok(status) => tracing::warn!("plugin {plugin} exited: {status}"),
                        Err(e) => tracing::error!("plugin {plugin}: {e:#}"),
                    },
                    // Dropping the launch future kills the child (kill_on_drop).
                    _ = stop.notified() => {
                        tracing::info!("stopped plugin {plugin}");
                        return;
                    }
                }
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.conns.remove(&plugin);
                    inner.stops.remove(&plugin);
                    if !inner.running.contains(&plugin) {
                        return;
                    }
                }
                failures = if started.elapsed() > Duration::from_secs(60) { 0 } else { failures + 1 };
                if failures >= 5 {
                    tracing::error!("plugin {plugin} keeps crashing; giving up (see logs/{plugin}.log)");
                    let mut inner = self.inner.lock().unwrap();
                    inner.running.remove(&plugin);
                    inner.queued.remove(&plugin);
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1 << failures.min(5))).await;
                // Plugins expect willAppear for visible actions after a restart.
                self.replay_appearances(&plugin);
            }
        });
    }

    fn replay_appearances(&self, plugin: &str) {
        let inner = self.inner.lock().unwrap();
        let serials: HashSet<&String> = inner
            .contexts
            .values()
            .filter(|(_, action)| self.action(action).is_some_and(|a| a.plugin == plugin))
            .map(|(serial, _)| serial)
            .collect();
        for serial in serials {
            if let Some(deck) = inner.decks.get(serial) {
                let _ = deck.tx.send(ToDeck::FromPlugin(json!({ "event": "deckd:reappear", "plugin": plugin })));
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `-info` / `inInfo` JSON given to plugins and property inspectors.
    pub fn app_info(&self, plugin: &str) -> Value {
        let devices: Vec<Value> = self.inner.lock().unwrap().decks.values().map(|d| d.info.clone()).collect();
        let version = self.plugin(plugin).map(|p| p.version.clone()).unwrap_or_default();
        json!({
            "application": { "font": "", "language": "en", "platform": "linux", "platformVersion": "", "version": "6.9.0" },
            "plugin": { "uuid": plugin, "version": version },
            "devicePixelRatio": 2,
            "colors": {
                "buttonPressedBackgroundColor": "#303030FF",
                "buttonPressedBorderColor": "#646464FF",
                "buttonPressedTextColor": "#969696FF",
                "disabledColor": "#007AFF59",
                "highlightColor": "#007AFFFF",
                "mouseDownColor": "#2EA8FFFF"
            },
            "devices": devices,
        })
    }

    async fn launch(&self, uuid: &str) -> anyhow::Result<std::process::ExitStatus> {
        let p = self.plugin(uuid).ok_or_else(|| anyhow::anyhow!("unknown plugin"))?;
        let p = p.as_ref();
        let info = self.app_info(uuid);
        let args = [
            "-port".to_string(),
            self.port.to_string(),
            "-pluginUUID".into(),
            uuid.into(),
            "-registerEvent".into(),
            "registerPlugin".into(),
            "-info".into(),
            info.to_string(),
        ];

        let cmd = match p.runtime.as_ref() {
            Some(Runtime::Node(path)) => {
                let node = self.node.as_ref().ok_or_else(|| anyhow::anyhow!("node.js not found; set DECKD_NODE"))?;
                let mut c = tokio::process::Command::new(node);
                c.arg(path);
                c
            }
            Some(Runtime::Native(path)) => {
                ensure_executable(path);
                tokio::process::Command::new(path)
            }
            Some(Runtime::Wine(path)) => {
                let mut c = tokio::process::Command::new("wine");
                c.arg(path).env("WINEPREFIX", config::config_dir().join("wine")).env("WINEDEBUG", "-all");
                c
            }
            Some(Runtime::Html(page)) if html_runtime(uuid) == "webkit" => {
                // gjs would swallow Elgato's -info flag as its own -I, so pass args via the environment.
                let mut c = tokio::process::Command::new("gjs");
                c.arg("-m").arg(runtime_file("html-runner.js")?).arg(page).env("DECKD_PLUGIN_ARGS", serde_json::to_string(&args)?);
                c.env("DECKD_PLUGIN_DATA", data_dir());
                return self.run_child(uuid, p, c, &[]).await;
            }
            Some(Runtime::Html(page)) => {
                let node = self.node.as_ref().ok_or_else(|| anyhow::anyhow!("node.js not found; set DECKD_NODE"))?;
                let mut c = tokio::process::Command::new(node);
                c.arg(runtime_file("html-host.mjs")?).arg(page).env("DECKD_PLUGIN_DATA", data_dir());
                c
            }
            None => anyhow::bail!("no runnable code for linux"),
        };

        self.run_child(uuid, p, cmd, &args).await
    }

    async fn run_child(&self, uuid: &str, p: &PluginInfo, mut cmd: tokio::process::Command, args: &[String]) -> anyhow::Result<std::process::ExitStatus> {
        let log_dir = config::config_dir().join("logs");
        std::fs::create_dir_all(&log_dir)?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log_dir.join(format!("{uuid}.log")))?;
        cmd.args(args)
            .current_dir(&p.dir)
            .stdin(std::process::Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true);
        tracing::info!("starting plugin {uuid}");
        let mut child = cmd.spawn()?;
        Ok(child.wait().await?)
    }

    pub async fn handle_socket(self: Arc<Self>, socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        let mut role: Option<Role> = None;
        while let Some(Ok(msg)) = stream.next().await {
            let text = match msg {
                Message::Text(t) => t.to_string(),
                Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                tracing::debug!("non-json message: {text:.200}");
                continue;
            };
            let event = v.get("event").and_then(Value::as_str).unwrap_or_default();
            match (&role, event) {
                (None, "registerPlugin") => {
                    let Some(uuid) = v.get("uuid").and_then(Value::as_str) else { continue };
                    tracing::info!("plugin registered: {uuid}");
                    self.on_plugin_registered(uuid, tx.clone());
                    role = Some(Role::Plugin(uuid.to_string()));
                }
                (None, "registerPropertyInspector") => {
                    let Some(ctx) = v.get("uuid").and_then(Value::as_str) else { continue };
                    self.inner.lock().unwrap().inspectors.insert(ctx.to_string(), tx.clone());
                    role = Some(Role::Inspector(ctx.to_string()));
                }
                (Some(Role::Plugin(uuid)), _) => self.from_plugin(uuid, v),
                (Some(Role::Inspector(ctx)), _) => self.from_inspector(ctx, v),
                (None, _) => tracing::debug!("message before registration: {text:.200}"),
            }
        }

        let mut inner = self.inner.lock().unwrap();
        match role {
            Some(Role::Plugin(uuid)) => {
                // Only drop the connection if it hasn't already been replaced by a reconnect.
                if inner.conns.get(&uuid).is_some_and(|c| c.same_channel(&tx)) {
                    inner.conns.remove(&uuid);
                }
            }
            Some(Role::Inspector(ctx)) => {
                inner.inspectors.remove(&ctx);
            }
            None => {}
        }
        writer.abort();
    }

    fn on_plugin_registered(&self, uuid: &str, tx: mpsc::UnboundedSender<String>) {
        let mut inner = self.inner.lock().unwrap();
        for (serial, deck) in &inner.decks {
            let _ = tx.send(json!({ "event": "deviceDidConnect", "device": serial, "deviceInfo": deck.info }).to_string());
        }
        for msg in inner.queued.remove(uuid).unwrap_or_default() {
            let _ = tx.send(msg);
        }
        inner.running.insert(uuid.to_string());
        inner.conns.insert(uuid.to_string(), tx);
    }

    fn from_plugin(&self, plugin: &str, v: Value) {
        let event = v.get("event").and_then(Value::as_str).unwrap_or_default();
        let context = v.get("context").and_then(Value::as_str).unwrap_or_default();
        tracing::debug!("<- {plugin}: {event} {}", if event == "setImage" { "" } else { context });
        match event {
            "setGlobalSettings" => {
                let payload = v.get("payload").cloned().unwrap_or(json!({}));
                let _ = config::write_json(&data_dir().join(format!("{plugin}.json")), &payload);
                let inner = self.inner.lock().unwrap();
                let msg = json!({ "event": "didReceiveGlobalSettings", "payload": { "settings": payload } }).to_string();
                for (ctx, conn) in &inner.inspectors {
                    if self.context_plugin(&inner, ctx).as_deref() == Some(plugin) {
                        let _ = conn.send(msg.clone());
                    }
                }
            }
            "getGlobalSettings" => {
                let settings = self.global_settings(plugin);
                if let Some(conn) = self.inner.lock().unwrap().conns.get(plugin) {
                    let _ = conn.send(
                        json!({ "event": "didReceiveGlobalSettings", "payload": { "settings": settings } }).to_string(),
                    );
                }
            }
            "openUrl" => {
                if let Some(url) = v.pointer("/payload/url").and_then(Value::as_str) {
                    crate::actions::spawn_shell(&format!("xdg-open {}", shell_quote(url)));
                }
            }
            "logMessage" => {
                let m = v.pointer("/payload/message").and_then(Value::as_str).unwrap_or_default();
                tracing::info!("[{plugin}] {m}");
            }
            "sendToPropertyInspector" => {
                let action = self.inner.lock().unwrap().contexts.get(context).map(|(_, a)| a.clone());
                self.send_to_inspector(
                    context,
                    json!({ "event": "sendToPropertyInspector", "action": action, "context": context, "payload": v.get("payload") }),
                );
            }
            _ => self.route_to_deck(context, ToDeck::FromPlugin(v.clone()), v.get("device").and_then(Value::as_str)),
        }
    }

    fn from_inspector(&self, context: &str, v: Value) {
        let event = v.get("event").and_then(Value::as_str).unwrap_or_default();
        let inner = self.inner.lock().unwrap();
        let plugin = self.context_plugin(&inner, context);
        drop(inner);
        match event {
            "sendToPlugin" => {
                let action = v.get("action").and_then(Value::as_str).map(String::from).or_else(|| {
                    self.inner.lock().unwrap().contexts.get(context).map(|(_, a)| a.clone())
                });
                if let (Some(plugin), Some(action)) = (plugin, action) {
                    if let Some(conn) = self.inner.lock().unwrap().conns.get(&plugin) {
                        let msg = json!({ "event": "sendToPlugin", "action": action, "context": context, "payload": v.get("payload") });
                        let _ = conn.send(msg.to_string());
                    }
                }
            }
            "getGlobalSettings" | "setGlobalSettings" => {
                let Some(plugin) = plugin else { return };
                if event == "setGlobalSettings" {
                    let payload = v.get("payload").cloned().unwrap_or(json!({}));
                    let _ = config::write_json(&data_dir().join(format!("{plugin}.json")), &payload);
                    if let Some(conn) = self.inner.lock().unwrap().conns.get(&plugin) {
                        let msg = json!({ "event": "didReceiveGlobalSettings", "payload": { "settings": payload } });
                        let _ = conn.send(msg.to_string());
                    }
                } else {
                    let msg = json!({ "event": "didReceiveGlobalSettings", "payload": { "settings": self.global_settings(&plugin) } });
                    self.send_to_inspector(context, msg);
                }
            }
            "openUrl" => {
                if let Some(url) = v.pointer("/payload/url").and_then(Value::as_str) {
                    crate::actions::spawn_shell(&format!("xdg-open {}", shell_quote(url)));
                }
            }
            "logMessage" => tracing::info!("[inspector {context}] {}", v.pointer("/payload/message").unwrap_or(&serde_json::Value::Null)),
            _ => self.route_to_deck(context, ToDeck::FromInspector(v), None),
        }
    }

    fn route_to_deck(&self, context: &str, msg: ToDeck, device: Option<&str>) {
        let inner = self.inner.lock().unwrap();
        let serial = inner.contexts.get(context).map(|(s, _)| s.as_str()).or(device);
        if let Some(deck) = serial.and_then(|s| inner.decks.get(s)) {
            let _ = deck.tx.send(msg);
        }
    }

    fn context_plugin(&self, inner: &Inner, context: &str) -> Option<String> {
        let (_, action) = inner.contexts.get(context)?;
        Some(self.action(action)?.plugin.clone())
    }

    pub fn global_settings(&self, plugin: &str) -> Value {
        config::read_json::<Value>(&data_dir().join(format!("{plugin}.json"))).ok().filter(|v| !v.is_null()).unwrap_or(json!({}))
    }
}

enum Role {
    Plugin(String),
    Inspector(String),
}

/// Which engine hosts an HTML plugin: "node" (default, light) or "webkit" (full browser).
/// Set per plugin in settings.json: `{ "html_runtime": { "<plugin uuid>": "webkit" } }`.
fn html_runtime(uuid: &str) -> String {
    let settings: Value = config::read_json(&config::config_dir().join("settings.json")).unwrap_or_default();
    settings.pointer(&format!("/html_runtime/{uuid}")).and_then(Value::as_str).unwrap_or("node").to_string()
}

/// The runner scripts ship inside the binary and are written out on demand.
fn runtime_file(name: &str) -> anyhow::Result<PathBuf> {
    let content = match name {
        "html-host.mjs" => include_str!("../../runtime/html-host.mjs"),
        "html-runner.js" => include_str!("../../runtime/html-runner.js"),
        _ => anyhow::bail!("unknown runtime file {name}"),
    };
    let path = config::config_dir().join("runtime").join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(content) {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, content)?;
    }
    Ok(path)
}

pub fn runtime_name(rt: &Option<Runtime>) -> &'static str {
    match rt {
        Some(Runtime::Node(_)) => "node",
        Some(Runtime::Native(_)) => "native",
        Some(Runtime::Html(_)) => "html",
        Some(Runtime::Wine(_)) => "wine",
        None => "none",
    }
}

fn find_node() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DECKD_NODE") {
        return Some(p.into());
    }
    if let Some(path) = std::env::var_os("PATH") {
        if let Some(p) = std::env::split_paths(&path).map(|d| d.join("node")).find(|p| p.is_file()) {
            return Some(p);
        }
    }
    // Version managers aren't on PATH when started from a systemd user unit.
    let home = dirs::home_dir()?;
    for root in [".config/nvm/versions/node", ".nvm/versions/node", ".local/share/fnm/node-versions"] {
        let mut versions: Vec<PathBuf> = std::fs::read_dir(home.join(root)).into_iter().flatten().flatten().map(|e| e.path()).collect();
        versions.sort();
        for v in versions.iter().rev() {
            for candidate in [v.join("bin/node"), v.join("installation/bin/node")] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn ensure_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o111 == 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o755));
        }
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

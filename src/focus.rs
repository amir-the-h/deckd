//! Tracks the focused window through Hyprland's IPC event socket.

use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::watch;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Focus {
    pub class: String,
    pub title: String,
    /// Steam app id when the window belongs to a game Steam launched.
    pub steam_app: Option<u32>,
}

pub type FocusWatch = watch::Receiver<Focus>;

/// Starts following focus changes. Without Hyprland the value just never changes.
pub fn start() -> FocusWatch {
    let (tx, rx) = watch::channel(Focus::default());
    tokio::spawn(async move {
        let mut warned = false;
        loop {
            if let Some(f) = query().await {
                tx.send_if_modified(|cur| replace(cur, f));
            }
            match follow(&tx).await {
                Ok(()) => {}
                Err(e) if !warned => {
                    tracing::info!("focus tracking unavailable ({e}); auto page switching is off until Hyprland is reachable");
                    warned = true;
                }
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    rx
}

fn replace(cur: &mut Focus, new: Focus) -> bool {
    if *cur == new {
        return false;
    }
    *cur = new;
    true
}

fn socket_path() -> Option<std::path::PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(std::path::PathBuf::from(runtime).join("hypr").join(sig).join(".socket2.sock"))
}

async fn follow(tx: &watch::Sender<Focus>) -> anyhow::Result<()> {
    let path = socket_path().ok_or_else(|| anyhow::anyhow!("HYPRLAND_INSTANCE_SIGNATURE not set"))?;
    let stream = UnixStream::connect(&path).await?;
    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        // activewindow>>CLASS,TITLE — the title may itself contain commas.
        if let Some(rest) = line.strip_prefix("activewindow>>") {
            let (class, title) = rest.split_once(',').unwrap_or((rest, ""));
            let mut f = Focus { class: class.to_string(), title: title.to_string(), steam_app: None };
            // The event has no pid; ask for it, and trust the answer only if it's still the same window.
            if let Some(now) = query().await.filter(|q| q.class == f.class) {
                f.steam_app = now.steam_app;
            }
            tx.send_if_modified(|cur| replace(cur, f));
        }
    }
    Ok(())
}

/// The focused window right now, via `hyprctl`.
async fn query() -> Option<Focus> {
    let out = tokio::process::Command::new("hyprctl").args(["activewindow", "-j"]).output().await.ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    Some(Focus {
        class: v.get("class").and_then(Value::as_str).unwrap_or_default().to_string(),
        title: v.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
        steam_app: v.get("pid").and_then(Value::as_i64).and_then(steam_app_of),
    })
}

/// Steam passes `SteamAppId` to every game it starts, and Proton keeps it, so the game's
/// own process carries it even when the window class is just the executable's name.
fn steam_app_of(pid: i64) -> Option<u32> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    env.split(|&b| b == 0).find_map(|kv| {
        let id = kv.strip_prefix(b"SteamAppId=").or_else(|| kv.strip_prefix(b"SteamGameId="))?;
        std::str::from_utf8(id).ok()?.parse().ok().filter(|&n| n != 0)
    })
}

/// Open windows (class, title), for picking a rule in the UI.
pub async fn windows() -> Vec<(String, String)> {
    let Ok(out) = tokio::process::Command::new("hyprctl").args(["clients", "-j"]).output().await else { return vec![] };
    let list: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut v: Vec<(String, String)> = list
        .iter()
        .filter_map(|c| Some((c.get("class")?.as_str()?.to_string(), c.get("title")?.as_str()?.to_string())))
        .filter(|(c, _)| !c.is_empty())
        .collect();
    v.sort();
    v.dedup();
    v
}

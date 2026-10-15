//! Data sources for live built-ins: MangoHud stats/control and MPRIS media info.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde_json::Value;

// ------------------------------------------------------------------ MangoHud
//
// Relies on MangoHud being configured (globally, in MangoHud.conf) with
//   control=/run/user/<uid>/scmango-%p, output_folder=/tmp/sc_mangohud,
//   autostart_log=1, log_interval=100
// which is what the StreamController MangoHud plugin set up.

pub const MANGO_LOG_DIR: &str = "/tmp/sc_mangohud";

/// Fields MangoHud logs, in the order the UI offers them.
pub const MANGO_FIELDS: &[&str] = &[
    "fps", "frametime", "cpu_load", "gpu_load", "cpu_temp", "gpu_temp", "cpu_power", "gpu_power",
    "gpu_core_clock", "gpu_mem_clock", "gpu_vram_used", "ram_used", "swap_used", "process_rss", "cpu_mhz", "elapsed",
];

/// Latest stats from the most recently written MangoHud log, if a game is running.
pub fn mango_stats() -> Option<HashMap<String, f64>> {
    let dir = match std::fs::read_dir(MANGO_LOG_DIR) {
        Ok(dir) => dir,
        Err(_) => {
            ensure_mango_log_dir();
            return None;
        }
    };
    let newest = dir
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(_, p)| p.extension().is_some_and(|x| x == "csv"))
        .max_by_key(|(t, _)| *t)?;
    // A log that stopped updating means the game has exited.
    if SystemTime::now().duration_since(newest.0).unwrap_or_default() > Duration::from_secs(3) {
        return None;
    }
    let mut f = std::fs::File::open(&newest.1).ok()?;

    // The column header sits near the top, after a few lines of system info.
    let mut head = vec![0u8; 4096];
    let n = f.read(&mut head).ok()?;
    let head = String::from_utf8_lossy(&head[..n]).into_owned();
    let keys: Vec<String> = head.lines().find(|l| l.starts_with("fps,frametime"))?.split(',').map(|s| s.trim().to_string()).collect();

    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(2048))).ok()?;
    let mut tail = String::new();
    f.read_to_string(&mut tail).ok()?;
    // The last line may still be half-written; take the last complete one.
    let line = tail.lines().rev().find(|l| l.split(',').count() == keys.len() && !l.starts_with("fps"))?;
    let values = line.split(',').map(|v| v.trim().parse::<f64>().unwrap_or(0.0));
    Some(keys.into_iter().zip(values).collect())
}

/// MangoHud won't create its output folder and silently drops every log line without it
/// ("Failed to write log file"); /tmp is wiped on boot, so make sure it's there.
pub fn ensure_mango_log_dir() {
    if let Err(e) = std::fs::create_dir_all(MANGO_LOG_DIR) {
        tracing::warn!("can't create {MANGO_LOG_DIR} for MangoHud logs: {e}");
    }
}

pub fn mango_format(field: &str, v: f64) -> String {
    match field {
        "fps" => format!("{v:.0}"),
        "frametime" => format!("{v:.1} ms"),
        "cpu_load" | "gpu_load" => format!("{v:.0}%"),
        "gpu_vram_used" | "ram_used" | "swap_used" => format!("{v:.1} GB"),
        "process_rss" => format!("{:.1} GB", v / 1024.0 / 1024.0 / 1024.0),
        "cpu_power" | "gpu_power" => format!("{v:.0} W"),
        "cpu_temp" | "gpu_temp" => format!("{v:.0}°C"),
        "gpu_core_clock" | "gpu_mem_clock" | "cpu_mhz" => format!("{v:.0} MHz"),
        "elapsed" => {
            let s = (v / 1e9) as u64;
            format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
        }
        _ => format!("{v}"),
    }
}

/// Toggles the HUD in every running MangoHud instance. Returns how many were reached.
pub fn mango_toggle_hud() -> usize {
    use std::io::Write;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};

    let uid = unsafe { getuid() };
    let prefix = format!("@/run/user/{uid}/scmango-");
    let sockets: Vec<String> = std::fs::read_to_string("/proc/net/unix")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_whitespace().last().filter(|p| p.starts_with(&prefix)).map(String::from))
        .collect();
    let mut reached = 0;
    for name in sockets.iter().collect::<std::collections::BTreeSet<_>>() {
        let Ok(addr) = SocketAddr::from_abstract_name(&name.as_bytes()[1..]) else { continue };
        if let Ok(mut s) = UnixStream::connect_addr(&addr) {
            let _ = s.set_write_timeout(Some(Duration::from_millis(500)));
            if s.write_all(b":hud;").is_ok() {
                reached += 1;
            }
        }
    }
    reached
}

unsafe extern "C" {
    fn getuid() -> u32;
}

// ------------------------------------------------------------------ MPRIS

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Media {
    pub bus: String,
    pub title: String,
    pub artist: String,
    /// Local file path of the cover art, if any.
    pub art: Option<String>,
    pub playing: bool,
}

async fn busctl(args: &[&str]) -> Option<Value> {
    let out = tokio::time::timeout(Duration::from_secs(2), tokio::process::Command::new("busctl").arg("--user").arg("--json=short").args(args).output())
        .await
        .ok()?
        .ok()?;
    serde_json::from_slice(&out.stdout).ok()
}

async fn player_prop(bus: &str, prop: &str) -> Option<Value> {
    busctl(&["get-property", bus, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", prop]).await.map(|v| v["data"].clone())
}

/// Picks a player whose bus name contains `filter` (any player if empty), preferring one that's playing.
pub async fn media(filter: &str) -> Option<Media> {
    let names = busctl(&["call", "org.freedesktop.DBus", "/", "org.freedesktop.DBus", "ListNames"]).await?;
    let filter = filter.to_lowercase();
    let players: Vec<String> = names["data"][0]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|n| n.starts_with("org.mpris.MediaPlayer2.") && n.to_lowercase().contains(&filter))
        .map(String::from)
        .collect();
    let mut best: Option<Media> = None;
    for bus in players {
        let status = player_prop(&bus, "PlaybackStatus").await.and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
        let meta = player_prop(&bus, "Metadata").await.unwrap_or(Value::Null);
        let field = |k: &str| meta.pointer(&format!("/{k}/data")).cloned().unwrap_or(Value::Null);
        let artist = match field("xesam:artist") {
            Value::Array(a) => a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", "),
            v => v.as_str().unwrap_or_default().to_string(),
        };
        let art = match field("mpris:artUrl").as_str() {
            Some(u) => cache_art(u).await,
            None => None,
        };
        let m = Media { bus, title: field("xesam:title").as_str().unwrap_or_default().to_string(), artist, art, playing: status == "Playing" };
        let better = match &best {
            None => true,
            Some(b) => m.playing && !b.playing,
        };
        if better {
            best = Some(m);
        }
    }
    best
}

/// Cover art as a local file: file:// URLs as-is (copied, since players delete them), http(s) downloaded once.
async fn cache_art(url: &str) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    let dir = crate::config::config_dir().join("cache/art");
    std::fs::create_dir_all(&dir).ok()?;
    let dest: PathBuf = dir.join(format!("{:x}", h.finish()));
    if dest.exists() {
        return Some(dest.to_string_lossy().into_owned());
    }
    // Keep the cache small.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        if files.len() > 50 {
            files.sort();
            for (_, p) in &files[..files.len() - 50] {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, &dest).ok()?;
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let ok = tokio::process::Command::new("curl")
            .args(["-fsSL", "--max-time", "10", "--max-filesize", "10000000", "-o"])
            .arg(&dest)
            .arg(url)
            .status()
            .await
            .ok()?
            .success();
        if !ok {
            let _ = std::fs::remove_file(&dest);
            return None;
        }
    } else {
        return None;
    }
    Some(dest.to_string_lossy().into_owned())
}

/// Calls an MPRIS player method such as PlayPause, Next or Previous.
pub fn media_call(bus: &str, method: &str) {
    let (bus, method) = (bus.to_string(), method.to_string());
    tokio::spawn(async move {
        busctl(&["call", &bus, "/org/mpris/MediaPlayer2", "org.mpris.MediaPlayer2.Player", &method]).await;
    });
}

#[cfg(test)]
mod tests {
    /// Prints what the now-playing lookup sees on this machine.
    #[tokio::test]
    async fn media_lookup() {
        if std::env::var("DECKD_TEST_MEDIA").is_err() {
            return;
        }
        println!("{:?}", super::media("chromium").await);
        println!("mango: {:?}", super::mango_stats().map(|m| m.len()));
    }
}

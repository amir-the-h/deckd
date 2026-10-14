//! Native PulseAudio/PipeWire volume state, kept fresh from `pactl subscribe`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Volume {
    pub description: String,
    pub percent: u32,
    pub muted: bool,
}

#[derive(Debug, Default)]
pub struct AudioState {
    /// Keyed by node name, e.g. `alsa_output.usb-...analog-stereo`.
    pub devices: HashMap<String, Volume>,
    pub default_sink: String,
    pub default_source: String,
}

impl AudioState {
    /// Resolves `@DEFAULT_SINK@` / `@DEFAULT_SOURCE@` or a node name.
    pub fn get(&self, device: &str) -> Option<&Volume> {
        let name = match device {
            "" | "@DEFAULT_SINK@" => &self.default_sink,
            "@DEFAULT_SOURCE@" => &self.default_source,
            n => n,
        };
        self.devices.get(name)
    }
}

pub type AudioWatch = watch::Receiver<Arc<AudioState>>;

/// Starts the background watcher. Never fails: without pactl the state stays empty.
pub fn start() -> AudioWatch {
    let (tx, rx) = watch::channel(Arc::new(AudioState::default()));
    tokio::spawn(async move {
        loop {
            if let Some(state) = query().await {
                let _ = tx.send(Arc::new(state));
            }
            if let Err(e) = follow(&tx).await {
                tracing::warn!("pactl subscribe: {e}");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
    rx
}

async fn follow(tx: &watch::Sender<Arc<AudioState>>) -> anyhow::Result<()> {
    let mut child = tokio::process::Command::new("pactl")
        .arg("subscribe")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    while let Some(line) = lines.next_line().await? {
        if !(line.contains(" sink ") || line.contains(" source ") || line.contains(" server")) {
            continue;
        }
        // Volume drags produce bursts of events; settle before re-querying.
        let _ = tokio::time::timeout(Duration::from_millis(30), async {
            while let Ok(Some(_)) = lines.next_line().await {}
        })
        .await;
        if let Some(state) = query().await {
            let _ = tx.send(Arc::new(state));
        }
    }
    child.wait().await?;
    Ok(())
}

async fn query() -> Option<AudioState> {
    let mut state = AudioState::default();
    for kind in ["sinks", "sources"] {
        let out = tokio::process::Command::new("pactl").args(["-f", "json", "list", kind]).output().await.ok()?;
        let list: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
        for d in list {
            let Some(name) = d.get("name").and_then(Value::as_str) else { continue };
            // Average the channels, as pavucontrol's locked slider does.
            let pcts: Vec<u32> = d
                .get("volume")
                .and_then(Value::as_object)
                .map(|ch| {
                    ch.values()
                        .filter_map(|c| c.get("value_percent")?.as_str()?.trim_end_matches('%').parse().ok())
                        .collect()
                })
                .unwrap_or_default();
            let percent = if pcts.is_empty() { 0 } else { pcts.iter().sum::<u32>() / pcts.len() as u32 };
            state.devices.insert(
                name.to_string(),
                Volume {
                    description: d.get("description").and_then(Value::as_str).unwrap_or(name).to_string(),
                    percent,
                    muted: d.get("mute").and_then(Value::as_bool).unwrap_or(false),
                },
            );
        }
    }
    for (cmd, slot) in [("get-default-sink", &mut state.default_sink), ("get-default-source", &mut state.default_source)] {
        if let Ok(out) = tokio::process::Command::new("pactl").arg(cmd).output().await {
            *slot = String::from_utf8_lossy(&out.stdout).trim().to_string();
        }
    }
    Some(state)
}

fn pactl_target(kind: &str) -> &'static str {
    if kind == "source" { "source" } else { "sink" }
}

/// Changes volume by `delta` percent, clamped to 0..=100.
pub fn adjust(device: &str, kind: &str, current: u32, delta: i32) {
    let target = (current as i32 + delta).clamp(0, 100);
    let dev = if device.is_empty() { format!("@DEFAULT_{}@", pactl_target(kind).to_uppercase()) } else { device.to_string() };
    run(&["pactl", &format!("set-{}-volume", pactl_target(kind)), &dev, &format!("{target}%")]);
}

pub fn toggle_mute(device: &str, kind: &str) {
    let dev = if device.is_empty() { format!("@DEFAULT_{}@", pactl_target(kind).to_uppercase()) } else { device.to_string() };
    run(&["pactl", &format!("set-{}-mute", pactl_target(kind)), &dev, "toggle"]);
}

fn run(args: &[&str]) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::spawn(async move {
        if let Err(e) = tokio::process::Command::new(&args[0]).args(&args[1..]).status().await {
            tracing::warn!("{}: {e}", args.join(" "));
        }
    });
}

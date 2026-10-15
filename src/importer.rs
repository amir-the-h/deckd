//! Imports StreamController pages into deckd's format.
//!
//! Actions with a native equivalent (page switching, shell commands, Steam)
//! become built-ins. Everything else is kept as `sc:<plugin>::<action>` with its
//! settings intact so it can be remapped to an Elgato-format plugin later.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::actions;
use crate::config::{self, ActionInstance, Label, LabelPos, Page, Profile, Slot, State, Trigger};

pub fn default_sc_dir() -> PathBuf {
    let flatpak = dirs::home_dir().unwrap().join(".var/app/com.core447.StreamController/data");
    if flatpak.exists() {
        flatpak
    } else {
        dirs::data_dir().unwrap().join("StreamController")
    }
}

pub fn import(sc_dir: &Path, columns: u8) -> Result<()> {
    let pages_dir = sc_dir.join("pages");
    let defaults: Value = serde_json::from_str(&std::fs::read_to_string(sc_dir.join("settings/pages.json")).unwrap_or_default())
        .unwrap_or(Value::Null);
    let icons_dir = config::config_dir().join("icons");
    std::fs::create_dir_all(&icons_dir)?;

    let mut unmapped: BTreeMap<String, usize> = BTreeMap::new();
    let mut serials: BTreeSet<String> = BTreeSet::new();
    let mut pages: BTreeMap<String, Page> = BTreeMap::new();

    let mut entries: Vec<_> = std::fs::read_dir(&pages_dir)
        .with_context(|| format!("reading {}", pages_dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    entries.sort();

    for path in entries {
        let name = page_name(&path);
        let sc: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let mut page = Page {
            brightness: sc.pointer("/brightness/value").and_then(Value::as_u64).map(|v| v as u8),
            auto: auto_switch(sc.get("auto-change")),
            ..Default::default()
        };
        let mut ctx = Ctx { icons_dir: &icons_dir, unmapped: &mut unmapped, serials: &mut serials };

        for (pos, key) in sc.get("keys").and_then(Value::as_object).into_iter().flatten() {
            let Some((x, y)) = pos.split_once('x').and_then(|(x, y)| Some((x.parse::<u8>().ok()?, y.parse::<u8>().ok()?)))
            else {
                continue;
            };
            if let Some(slot) = convert_slot(key, &mut ctx) {
                page.keys.insert(y * columns + x, slot);
            }
        }
        for (idx, dial) in sc.get("dials").and_then(Value::as_object).into_iter().flatten() {
            let Ok(i) = idx.parse::<u8>() else { continue };
            if let Some(slot) = convert_slot(dial, &mut ctx) {
                page.dials.insert(i, slot);
            }
        }
        println!("  page {name:<12} {} keys, {} dials", page.keys.len(), page.dials.len());
        pages.insert(name, page);
    }

    // Pages are written for every deck StreamController knew about.
    if let Some(map) = defaults.get("default-pages").and_then(Value::as_object) {
        serials.extend(map.keys().cloned());
    }
    if serials.is_empty() {
        anyhow::bail!("couldn't determine the deck serial; no default-pages in StreamController settings");
    }
    for serial in &serials {
        let dir = config::profile_dir(serial);
        let default_page = defaults
            .pointer(&format!("/default-pages/{serial}"))
            .and_then(Value::as_str)
            .map(|p| page_name(Path::new(p)))
            .unwrap_or_else(|| "Main".into());
        let deck: Value = serde_json::from_str(&std::fs::read_to_string(sc_dir.join(format!("settings/decks/{serial}.json"))).unwrap_or_default())
            .unwrap_or(Value::Null);
        let brightness = deck.pointer("/brightness/value").and_then(Value::as_u64).unwrap_or(75) as u8;
        config::write_json(&dir.join("profile.json"), &Profile { default_page, brightness })?;
        for (name, page) in &pages {
            config::write_json(&dir.join("pages").join(format!("{name}.json")), page)?;
        }
        println!("wrote {} pages to {}", pages.len(), dir.display());
    }

    if !unmapped.is_empty() {
        println!("\nKept as placeholders (need a plugin before they do anything):");
        for (id, n) in &unmapped {
            println!("  {n:>3}x {id}");
        }
    }
    Ok(())
}

struct Ctx<'a> {
    icons_dir: &'a Path,
    unmapped: &'a mut BTreeMap<String, usize>,
    serials: &'a mut BTreeSet<String>,
}

/// StreamController's per-page "auto-change" focus rule. Imported as switch-and-return.
fn auto_switch(v: Option<&Value>) -> Option<config::AutoSwitch> {
    let v = v?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let rule = config::AutoSwitch {
        enabled: v.get("enable").and_then(Value::as_bool).unwrap_or(false),
        class: s("wm_class"),
        title: s("title"),
        stay: false,
    };
    (!rule.class.is_empty() || !rule.title.is_empty()).then_some(rule)
}

fn page_name(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

fn convert_slot(sc: &Value, ctx: &mut Ctx) -> Option<Slot> {
    // Only state 0 is imported; StreamController's multi-state keys are rare.
    let st = sc.pointer("/states/0")?;
    let sc_display = st.get("image-control-action").and_then(Value::as_u64).unwrap_or(0) as usize;
    let converted: Vec<(usize, ActionInstance)> = st
        .get("actions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(i, a)| Some((i, convert_action(a, ctx)?)))
        .collect();
    // The native volume dial already sets the default on push and mutes on touch;
    // StreamController needed separate actions for that, which would now double-fire.
    let has_volume_dial = converted.iter().any(|(_, a)| a.uuid == actions::VOLUME);
    let converted: Vec<(usize, ActionInstance)> = converted
        .into_iter()
        .filter(|(_, a)| !(has_volume_dial && a.uuid.starts_with("drop:")))
        .map(|(i, mut a)| {
            if let Some(id) = a.uuid.strip_prefix("drop:") {
                a.uuid = format!("sc:{id}");
            }
            (i, a)
        })
        .collect();
    let display = converted
        .iter()
        .position(|(i, _)| *i == sc_display)
        .or_else(|| converted.iter().position(|(_, a)| a.uuid.contains('.') && !a.uuid.starts_with("sc:") && !a.uuid.starts_with("deckd.")))
        .unwrap_or(0);
    let mut actions: Vec<ActionInstance> = converted.into_iter().map(|(_, a)| a).collect();

    let mut state = State::default();
    if let Some(p) = st.pointer("/media/path").and_then(Value::as_str) {
        state.image = copy_icon(p, ctx.icons_dir);
    }
    for (pos, key) in [(LabelPos::Top, "top"), (LabelPos::Center, "center"), (LabelPos::Bottom, "bottom")] {
        let Some(l) = st.pointer(&format!("/labels/{key}")) else { continue };
        let text = l.get("text").and_then(Value::as_str).unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        state.labels.insert(
            pos,
            Label {
                text: text.into(),
                size: l.get("font-size").and_then(Value::as_f64).map(|v| v as f32),
                color: l.get("color").and_then(Value::as_array).and_then(|c| rgb_hex(c)),
            },
        );
    }
    if let Some(c) = st.pointer("/background/color").and_then(Value::as_array) {
        state.background = rgb_hex(c);
    }

    if actions.is_empty() && state.image.is_none() && state.labels.is_empty() {
        return None;
    }
    if actions.is_empty() {
        // A purely decorative key still needs somewhere to keep its look.
        actions.push(ActionInstance { uuid: "deckd.none".into(), ..Default::default() });
    }
    let target = &mut actions[display];
    if target.states.is_empty() {
        target.states.push(State::default());
    }
    // Slot appearance set by the user wins over defaults the converter chose.
    let s = &mut target.states[0];
    if state.image.is_some() {
        s.image = state.image;
    }
    s.labels.extend(state.labels);
    if state.background.is_some() {
        s.background = state.background;
    }
    // Without art the key would be blank; show what it is instead.
    let is_plugin = target.uuid.contains('.') && !target.uuid.starts_with("deckd.") && !target.uuid.starts_with("sc:");
    if s.image.is_none() && s.labels.is_empty() && !is_plugin {
        let name = target.settings.get("name").and_then(Value::as_str).map(String::from).or_else(|| {
            target.uuid.strip_prefix("sc:").and_then(|id| id.rsplit("::").next()).map(String::from)
        });
        if let Some(text) = name {
            s.labels.insert(LabelPos::Center, Label { text, size: Some(11.0), ..Default::default() });
        }
    }
    Some(Slot { actions, display })
}

fn convert_action(a: &Value, ctx: &mut Ctx) -> Option<ActionInstance> {
    let id = a.get("id")?.as_str()?;
    let settings = a.get("settings").cloned().unwrap_or(Value::Null);
    let s = |k: &str| settings.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let trigger = trigger_of(a.get("event-assignments"));

    let (uuid, new_settings, states) = match id {
        "com_core447_DeckPlugin::ChangePage" => {
            if let Some(serial) = settings.get("deck_number").and_then(Value::as_str) {
                ctx.serials.insert(serial.to_string());
            }
            (actions::PAGE, json!({ "page": page_name(Path::new(&s("selected_page"))) }), vec![])
        }
        "com_core447_OSPlugin::EasyCommand" => (actions::COMMAND, json!({ "command": s("command") }), vec![]),
        "steam::LaunchGame" => {
            let app = s("app_id");
            let state = State {
                image: steam_art(&app).and_then(|p| copy_icon(&p.to_string_lossy(), ctx.icons_dir)),
                ..Default::default()
            };
            (actions::COMMAND, json!({ "command": format!("steam steam://rungameid/{app}"), "name": s("game_name") }), vec![state])
        }
        "steam::CloseSteam" => (actions::COMMAND, json!({ "command": "steam -shutdown" }), vec![]),
        "steam::ChangeStatus" => (
            actions::COMMAND,
            json!({ "command": format!("steam steam://friends/status/{}", s("committed_status")) }),
            vec![],
        ),
        "com_core447_DeckPlugin::ChangeState" | "me_amankhanna_opendeck::OpenDeckButton" => return None,

        // Elgato-format plugins that cover StreamController ones.
        "com_core447_MediaPlugin::PlayPause" | "dev_ReneLu_SpotifyControl::ElementPlayAction" => ("me.amankhanna.oampris.playpause", json!({}), vec![]),
        "com_core447_MediaPlugin::Next" => ("me.amankhanna.oampris.next", json!({}), vec![]),
        "com_core447_MediaPlugin::Previous" => ("me.amankhanna.oampris.previous", json!({}), vec![]),
        "com_imdevinc_StreamControllerDiscordPlugin::Mute" => ("me.amankhanna.oadiscord.togglemute", json!({}), vec![]),
        "com_imdevinc_StreamControllerDiscordPlugin::Deafen" => ("me.amankhanna.oadiscord.toggledeafen", json!({}), vec![]),
        "com_imdevinc_StreamControllerDiscordPlugin::Push_To_Talk" => ("me.amankhanna.oadiscord.pushtotalk", json!({}), vec![]),
        "com_imdevinc_StreamControllerDiscordPlugin::UserVolume" => ("me.amankhanna.oadiscord.volumecontrol", json!({}), vec![]),
        "me_the_h_VirtualCam::ToggleCapture" => ("me.the-h.utilities.virtualcam-toggle", json!({}), vec![]),
        "com_the_h_ElgatoKeyLight::ToggleLight" => ("me.the-h.utilities.elgato-toggle", json!({}), vec![]),
        "com_the_h_ElgatoKeyLight::SetBrightness" => ("me.the-h.utilities.elgato-brightness", json!({}), vec![]),
        "com_the_h_ElgatoKeyLight::SetTemperature" => ("me.the-h.utilities.elgato-temperature", json!({}), vec![]),
        // Clocks, MangoHud and now-playing are built-ins.
        "com_core447_Clocks::Date" => (actions::CLOCK, json!({ "style": "digital", "format": s("key") }), vec![]),
        "com_core447_Clocks::DigitalClock" => (actions::CLOCK, json!({ "style": "digital" }), vec![]),
        "com_core447_Clocks::AnalogClock" => (actions::CLOCK, json!({ "style": "analog" }), vec![]),
        "dev_eumario_MangoHud::ToggleHud" => (actions::MANGOHUD, json!({ "mode": "toggle" }), vec![]),
        "dev_eumario_MangoHud::ItemDisplay" => {
            let pos = |k: &str| match settings.get(k).and_then(Value::as_u64) {
                Some(0) => "top",
                Some(2) => "bottom",
                _ => "center",
            };
            (
                actions::MANGOHUD,
                json!({ "mode": "stat", "field": s("field"), "label": s("label_text"), "label_pos": pos("label_loc"), "stat_pos": pos("stat_loc") }),
                vec![],
            )
        }
        "com_core447_MediaPlugin::Info" => {
            // Chrome registers on MPRIS as "chromium".
            let player = match s("player_name").to_lowercase().as_str() {
                "chrome" | "google chrome" => "chromium".to_string(),
                p => p.to_string(),
            };
            let flag = |k: &str| settings.get(k).and_then(Value::as_bool).unwrap_or(true);
            (actions::MEDIA, json!({ "player": player, "art": flag("show_thumbnail"), "title": flag("show_label") }), vec![])
        }
        // Hyprland shaders are a built-in now.
        "com_amir_HyprShader::ShaderDial" => (actions::SHADER, json!({ "mode": "dial", "index": settings.get("dial_index").cloned().unwrap_or(json!(0)) }), vec![]),
        "com_amir_HyprShader::AnimatedShaderDial" => {
            (actions::SHADER, json!({ "mode": "dial", "folder": "animated", "index": settings.get("dial_index").cloned().unwrap_or(json!(0)) }), vec![])
        }
        "com_amir_HyprShader::ToggleShader" => (actions::SHADER, json!({ "mode": "toggle", "shader": s("shader_path") }), vec![]),
        // OBS via Multi OBS Controller, which talks to OBS's built-in obs-websocket.
        "com_oparada1988_OBS_Plus::SwitchScene" => ("dev.theca11.multiobs.setscene", json!({ "params1": { "sceneName": s("scene") } }), vec![]),
        "com_oparada1988_OBS_Plus::ToggleRecord" => ("dev.theca11.multiobs.togglerecord", json!({}), vec![]),
        "com_oparada1988_OBS_Plus::ToggleStream" => ("dev.theca11.multiobs.togglestream", json!({}), vec![]),
        "com_oparada1988_OBS_Plus::ToggleReplayBuffer" => ("dev.theca11.multiobs.togglereplaybuffer", json!({}), vec![]),
        "com_oparada1988_OBS_Plus::SaveReplayBuffer" => ("dev.theca11.multiobs.savereplaybuffer", json!({}), vec![]),
        "com_oparada1988_OBS_Plus::OBSStats" => ("dev.theca11.multiobs.stats", json!({}), vec![]),
        // Multi OBS can mute an input but not change its volume, so the dial mutes on push.
        "com_oparada1988_OBS_Plus::InputDial" => ("dev.theca11.multiobs.toggleinputmute", json!({ "params1": { "inputName": s("input") } }), vec![]),
        "com_gapls_AudioControl::AdjustVolume" => {
            let source = s("device-filter") == "source";
            let kind = if source { "source" } else { "sink" };
            (actions::VOLUME, json!({ "device": s("pulse-name"), "kind": kind }), vec![])
        }
        "com_gapls_AudioControl::Mute" | "com_gapls_AudioControl::SetDefaultDevice" => {
            return Some(ActionInstance { uuid: format!("drop:{id}"), settings, trigger, ..Default::default() });
        }
        other => {
            *ctx.unmapped.entry(other.to_string()).or_default() += 1;
            return Some(ActionInstance { uuid: format!("sc:{other}"), settings, trigger, ..Default::default() });
        }
    };
    let mut new_settings = new_settings;
    if uuid.starts_with("dev.theca11.multiobs.") {
        // Multi OBS picks its target before global settings arrive; pin it to the first OBS.
        new_settings["common"] = json!({ "target": "1" });
    }
    Some(ActionInstance { uuid: uuid.into(), settings: new_settings, trigger, states, ..Default::default() })
}

/// StreamController remaps physical events onto an action's "Key Down"; recover tap/hold from that.
fn trigger_of(assign: Option<&Value>) -> Option<Trigger> {
    let map = assign?.as_object()?;
    let source = map.iter().find(|(_, v)| matches!(v.as_str(), Some("Key Down" | "Dial Down"))).map(|(k, _)| k.as_str());
    match source {
        Some("Key Short Up" | "Dial Short Up") => Some(Trigger::Tap),
        Some("Key Hold Start" | "Dial Hold Start") => Some(Trigger::Hold),
        Some("Dial Turn CW") => Some(Trigger::Cw),
        Some("Dial Turn CCW") => Some(Trigger::Ccw),
        _ => None,
    }
}

fn steam_art(app: &str) -> Option<PathBuf> {
    let root = dirs::data_dir()?.join("Steam/appcache/librarycache");
    let flat = ["library_600x900.jpg", "header.jpg", "logo.png"].map(|n| root.join(format!("{app}_{n}")));
    if let Some(p) = flat.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    // Newer Steam clients keep art under librarycache/<app>/, sometimes one level deeper.
    let dir = root.join(app);
    let mut dirs = vec![dir.clone()];
    dirs.extend(std::fs::read_dir(&dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    for name in ["library_600x900.jpg", "library_header.jpg", "header.jpg"] {
        if let Some(p) = dirs.iter().map(|d| d.join(name)).find(|p| p.exists()) {
            return Some(p);
        }
    }
    None
}

/// Copies an icon into deckd's own folder so the config survives uninstalling StreamController.
fn copy_icon(src: &str, icons_dir: &Path) -> Option<String> {
    let src = Path::new(src);
    if !src.exists() {
        return None;
    }
    let parent = src.parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let file = src.file_name()?.to_string_lossy();
    let dest = icons_dir.join(format!("{parent}-{file}"));
    if !dest.exists() {
        std::fs::copy(src, &dest).ok()?;
    }
    Some(dest.to_string_lossy().into_owned())
}

fn rgb_hex(c: &[Value]) -> Option<String> {
    let v: Vec<u8> = c.iter().filter_map(|x| x.as_u64().map(|n| n as u8)).collect();
    (v.len() >= 3).then(|| format!("#{:02x}{:02x}{:02x}", v[0], v[1], v[2]))
}

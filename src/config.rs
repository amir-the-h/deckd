//! On-disk configuration.
//!
//! Layout under `~/.config/deckd/`:
//!   settings.json                      global settings
//!   profiles/<serial>/profile.json     per-device settings (default page, brightness)
//!   profiles/<serial>/pages/<name>.json one file per page
//!
//! The action model follows the Elgato SDK (an action instance has a UUID,
//! settings and a list of visual states) and extends it the way StreamController
//! does: a slot may hold several actions, each optionally bound to a trigger
//! (tap vs. hold), with one of them driving the visuals.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DECKD_CONFIG") {
        return PathBuf::from(dir);
    }
    dirs::config_dir().unwrap_or_else(|| PathBuf::from("~/.config")).join("deckd")
}

pub fn profile_dir(serial: &str) -> PathBuf {
    config_dir().join("profiles").join(serial)
}

/// The same deck reports a different serial over hidraw (`A00WA43...`) than over
/// libusb (`WA43...`), so fall back to a profile whose name is a suffix of it.
fn find_profile_dir(serial: &str) -> PathBuf {
    let exact = profile_dir(serial);
    if exact.exists() {
        return exact;
    }
    std::fs::read_dir(config_dir().join("profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| serial.ends_with(&*n.to_string_lossy())))
        .unwrap_or(exact)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub default_page: String,
    pub brightness: u8,
}

impl Default for Profile {
    fn default() -> Self {
        Self { default_page: "Main".into(), brightness: 75 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Page {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<u8, Slot>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dials: BTreeMap<u8, Slot>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Slot {
    pub actions: Vec<ActionInstance>,
    /// Index into `actions` of the action whose state is drawn.
    #[serde(skip_serializing_if = "is_zero")]
    pub display: usize,
}

impl Slot {
    pub fn display_action(&self) -> Option<&ActionInstance> {
        self.actions.get(self.display).or(self.actions.first())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionInstance {
    pub uuid: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub settings: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Trigger>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<State>,
    #[serde(skip_serializing_if = "is_zero")]
    pub state: usize,
    /// Opaque per-instance id handed to plugins. Generated on load.
    #[serde(skip)]
    pub context: String,
}

impl Default for ActionInstance {
    fn default() -> Self {
        Self {
            uuid: String::new(),
            settings: Value::Null,
            trigger: None,
            states: Vec::new(),
            state: 0,
            context: String::new(),
        }
    }
}

impl ActionInstance {
    pub fn current_state(&self) -> Option<&State> {
        self.states.get(self.state).or(self.states.first())
    }
}

/// When an action fires relative to the physical press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Immediately on press.
    Down,
    /// On release, if released before the hold threshold.
    Tap,
    /// Once the press has lasted the hold threshold.
    Hold,
    /// When a dial is turned clockwise.
    Cw,
    /// When a dial is turned counter-clockwise.
    Ccw,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// File path or data URI (png, jpg, svg, gif first frame).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Background colour as `#rrggbb`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<LabelPos, Label>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelPos {
    Top,
    Center,
    Bottom,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Label {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

pub fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub struct DeviceConfig {
    pub dir: PathBuf,
    pub profile: Profile,
    pub pages: BTreeMap<String, Page>,
}

pub fn load_device(serial: &str) -> Result<DeviceConfig> {
    let dir = find_profile_dir(serial);
    let profile: Profile = read_json(&dir.join("profile.json"))?;
    let mut pages = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("pages")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                let mut page: Page = read_json(&path)?;
                for slot in page.keys.values_mut().chain(page.dials.values_mut()) {
                    for action in &mut slot.actions {
                        action.context = uuid::Uuid::new_v4().simple().to_string();
                    }
                }
                pages.insert(name, page);
            }
        }
    }
    if pages.is_empty() {
        pages.insert(profile.default_page.clone(), Page::default());
    }
    Ok(DeviceConfig { dir, profile, pages })
}

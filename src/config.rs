//! On-disk configuration.
//!
//! Layout under `~/.config/deckd/`:
//!   settings.json                      global settings
//!   profiles/<serial>/profile.json     per-device settings (default page, brightness)
//!   profiles/<serial>/pages/<name>.json one file per page
//!
//! The action model follows the Elgato SDK (an action instance has a UUID,
//! settings and a list of visual states) and extends it the way StreamController
//! does: a slot may hold several actions, each bound to the gestures that fire
//! it (tap, hold, turn, touch…), with one of them driving the visuals.

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

/// The same deck reports a different serial over hidraw (`A00XX00...`) than over
/// libusb (`XX00...`), so fall back to a profile whose name is a suffix of it.
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
    /// Switch to this page while a matching window has focus.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoSwitch>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<u8, Slot>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub dials: BTreeMap<u8, Slot>,
}

/// Focus rule for automatic page switching. Patterns are case-insensitive
/// regexes; empty ones are ignored, and all given ones must match.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoSwitch {
    pub enabled: bool,
    /// Window class (Wayland app id), e.g. `obs` or `chrome-app\.zoom\.us`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub class: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Only match windows of a game started by a launcher (Steam, Epic, Heroic, Lutris…),
    /// whatever their class.
    #[serde(alias = "steam_game", skip_serializing_if = "std::ops::Not::not")]
    pub game: bool,
    /// Stay on the page after the window loses focus instead of going back.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stay: bool,
}

impl Default for AutoSwitch {
    fn default() -> Self {
        Self { enabled: true, class: String::new(), title: String::new(), game: false, stay: false }
    }
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

    fn claims(&self, f: impl Fn(Trigger) -> bool) -> bool {
        self.actions.iter().any(|a| a.triggers.iter().any(|&t| f(t)))
    }

    /// Whether `a` gets the raw press and release, as in Elgato's software. True for an action
    /// without triggers, unless another action in the slot is bound to a press gesture; then it
    /// fires on tap instead, so a hold doesn't also run it.
    pub fn raw_press(&self, a: &ActionInstance) -> bool {
        a.triggers.is_empty() && !self.claims(Trigger::is_press)
    }

    /// Whether `gesture` fires `a`. An action without triggers takes whatever no other action in
    /// the slot is bound to.
    pub fn fires(&self, a: &ActionInstance, gesture: Gesture) -> bool {
        use Trigger::*;
        let holding = || self.claims(|t| matches!(t, Hold | Long));
        if a.triggers.is_empty() {
            return match gesture {
                Gesture::Down => !self.claims(Trigger::is_press),
                // A press too long to be a tap still counts as one when nothing is bound to holding.
                Gesture::Tap => self.claims(Trigger::is_press),
                Gesture::Long => self.claims(Trigger::is_press) && !holding(),
                Gesture::Double | Gesture::Hold => false,
                Gesture::Turn(cw) => !self.claims(|t| t == Turn || t == if cw { Cw } else { Ccw }),
                Gesture::Step(_) => false,
                Gesture::Touch => !self.claims(|t| t == Touch),
                Gesture::LongTouch => !self.claims(|t| matches!(t, Touch | LongTouch)),
            };
        }
        a.triggers.iter().any(|&t| match (t, gesture) {
            (Down, Gesture::Down) | (Tap, Gesture::Tap) | (Double, Gesture::Double) => true,
            (Hold, Gesture::Hold) | (Long, Gesture::Long) | (Turn, Gesture::Turn(_)) => true,
            (Tap, Gesture::Long) => !holding(),
            (Cw, Gesture::Step(cw)) | (Ccw, Gesture::Step(cw)) => cw == (t == Cw),
            (Touch, Gesture::Touch) | (LongTouch, Gesture::LongTouch) => true,
            // A long touch is still a touch when nothing is bound to it.
            (Touch, Gesture::LongTouch) => !self.claims(|t| t == LongTouch),
            _ => false,
        })
    }

    /// Whether a tap must wait to see if a second one follows.
    pub fn wants_double(&self) -> bool {
        self.claims(|t| t == Trigger::Double)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ActionInstance {
    pub uuid: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub settings: Value,
    /// Gestures that fire this action; none means the slot's leftover input (see `Slot::fires`).
    /// Older pages hold a single `trigger`.
    #[serde(alias = "trigger", deserialize_with = "one_or_many", skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<Trigger>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<State>,
    #[serde(skip_serializing_if = "is_zero")]
    pub state: usize,
    /// Opaque per-instance id handed to plugins. Stable across restarts, like
    /// Elgato's, so plugins can key state on it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub context: String,
}

impl Default for ActionInstance {
    fn default() -> Self {
        Self {
            uuid: String::new(),
            settings: Value::Null,
            triggers: Vec::new(),
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

/// A gesture an action can be bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    /// Immediately on press.
    Down,
    /// On release, if released before the hold threshold.
    Tap,
    /// On the second of two quick taps.
    Double,
    /// Once the press has lasted the hold threshold.
    Hold,
    /// On release, after a press that lasted the hold threshold.
    Long,
    /// When a dial is turned either way; the action gets the turn itself.
    Turn,
    /// When a dial is turned clockwise; fires like a press per turn.
    Cw,
    /// When a dial is turned counter-clockwise; fires like a press per turn.
    Ccw,
    /// When a dial's touch strip segment is tapped.
    Touch,
    /// When a dial's touch strip segment is long-pressed.
    #[serde(rename = "longtouch")]
    LongTouch,
}

impl Trigger {
    fn is_press(self) -> bool {
        matches!(self, Trigger::Down | Trigger::Tap | Trigger::Double | Trigger::Hold | Trigger::Long)
    }
}

/// What the user did to a slot, matched against triggers by `Slot::fires`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    Down,
    Tap,
    Double,
    Hold,
    Long,
    /// A turn, delivered as a turn (`true` = clockwise).
    Turn(bool),
    /// A turn, delivered as a press to direction-bound actions.
    Step(bool),
    Touch,
    LongTouch,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Trigger>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Trigger),
        Many(Vec<Trigger>),
        None(()),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(t) => vec![t],
        OneOrMany::Many(v) => v,
        OneOrMany::None(()) => Vec::new(),
    })
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
                if assign_contexts(&mut page) {
                    write_json(&path, &page)?;
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

/// Gives every action instance without one a fresh context. Returns whether any changed.
pub fn assign_contexts(page: &mut Page) -> bool {
    let mut changed = false;
    for slot in page.keys.values_mut().chain(page.dials.values_mut()) {
        for action in &mut slot.actions {
            if action.context.is_empty() {
                action.context = uuid::Uuid::new_v4().simple().to_string();
                changed = true;
            }
        }
    }
    changed
}

/// Page names become file names; keep them to something safe.
pub fn valid_page_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(json: &str) -> Slot {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn reads_old_single_trigger() {
        let s = slot(r#"{"actions":[{"uuid":"a","trigger":"hold"},{"uuid":"b","triggers":["tap","longtouch"]},{"uuid":"c"}]}"#);
        assert_eq!(s.actions[0].triggers, [Trigger::Hold]);
        assert_eq!(s.actions[1].triggers, [Trigger::Tap, Trigger::LongTouch]);
        assert!(s.actions[2].triggers.is_empty());
        let out = serde_json::to_value(&s.actions[0]).unwrap();
        assert_eq!(out["triggers"], serde_json::json!(["hold"]));
        assert!(out.get("trigger").is_none());
    }

    #[test]
    fn unbound_action_takes_leftover_input() {
        let s = slot(r#"{"actions":[{"uuid":"hold","triggers":["hold"]},{"uuid":"rest"}]}"#);
        let (hold, rest) = (&s.actions[0], &s.actions[1]);
        // Pressing is claimed, so the unbound action acts on tap rather than raw down/up.
        assert!(!s.raw_press(rest));
        assert!(!s.fires(rest, Gesture::Down) && s.fires(rest, Gesture::Tap));
        assert!(!s.fires(rest, Gesture::Long), "a hold is bound, so a long press isn't a tap");
        assert!(s.fires(hold, Gesture::Hold) && !s.fires(hold, Gesture::Tap));
        // Turning and touching aren't claimed.
        assert!(s.fires(rest, Gesture::Turn(true)) && s.fires(rest, Gesture::Touch));
        assert!(!s.fires(hold, Gesture::Turn(true)) && !s.fires(hold, Gesture::Touch));
    }

    #[test]
    fn dial_gestures_split_between_actions() {
        let s = slot(r#"{"actions":[{"uuid":"vol","triggers":["turn"]},{"uuid":"scene","triggers":["touch"]},{"uuid":"cw","triggers":["cw"]},{"uuid":"rest"}]}"#);
        let [vol, scene, cw, rest] = [0, 1, 2, 3].map(|i| &s.actions[i]);
        assert!(s.fires(vol, Gesture::Turn(false)) && !s.fires(vol, Gesture::Touch));
        assert!(s.fires(scene, Gesture::Touch) && s.fires(scene, Gesture::LongTouch));
        assert!(s.fires(cw, Gesture::Step(true)) && !s.fires(cw, Gesture::Step(false)));
        assert!(!s.fires(rest, Gesture::Turn(true)) && !s.fires(rest, Gesture::Touch));
        assert!(s.raw_press(rest) && s.fires(rest, Gesture::Down));
    }

    #[test]
    fn tap_covers_slow_presses_unless_holding_is_bound() {
        let s = slot(r#"{"actions":[{"uuid":"a","triggers":["tap"]},{"uuid":"b","triggers":["double"]}]}"#);
        assert!(s.fires(&s.actions[0], Gesture::Long));
        assert!(s.wants_double() && s.fires(&s.actions[1], Gesture::Double) && !s.fires(&s.actions[0], Gesture::Double));
    }
}

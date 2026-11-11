//! Game speed: turn a dial to change a game's speed, push it to pause. Works with any game
//! that has speed hotkeys, by pressing them on the virtual keyboard (see `uinput`).
//!
//! Games come in two kinds: ones with a key per speed (1, 2, 3…) and ones with
//! faster/slower keys. With a key per speed deckd knows the speed it picked; neither kind
//! reports back, so using the keyboard in game can leave the strip out of step until the
//! next turn.

use serde_json::Value;

use crate::actions::Fire;

/// Fast-forward, and the pause bars shown while paused.
pub const ICON: &str = "M4 18l8.5-6L4 6v12zm9-12v12l8.5-6L13 6z";
pub const PAUSED_ICON: &str = "M6 19h4V5H6v14zm8-14v14h4V5h-4z";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Speed {
    /// Index into the speed keys (levels mode only).
    pub level: usize,
    pub paused: bool,
}

/// The game's controls, read from the action's settings.
struct Keys {
    speeds: Vec<String>,
    names: Vec<String>,
    faster: String,
    slower: String,
    pause: String,
}

impl Keys {
    fn from(settings: &Value) -> Self {
        let s = |k: &str| settings.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
        let list = |k: &str| s(k).split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect::<Vec<_>>();
        let step = s("mode") == "step";
        let or = |v: String, d: &str| if v.is_empty() { d.to_string() } else { v };
        Keys {
            speeds: if step { vec![] } else { Some(list("speeds")).filter(|l| !l.is_empty()).unwrap_or_else(|| vec!["1".into(), "2".into(), "3".into()]) },
            names: list("names"),
            faster: or(s("faster"), "plus"),
            slower: or(s("slower"), "-"),
            pause: or(s("pause"), "space"),
        }
    }
}

/// Where the speed starts before deckd has changed it: the `start` setting (1-based),
/// or the first speed.
pub fn initial(settings: &Value) -> Speed {
    let start = settings.get("start").and_then(|v| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())).unwrap_or(1);
    let max = Keys::from(settings).speeds.len().max(1);
    Speed { level: (start.max(1) as usize - 1).min(max - 1), paused: false }
}

/// Updates the speed for an input and returns the keys to press, in order.
pub fn step(settings: &Value, st: &mut Speed, fire: Fire) -> Vec<String> {
    let k = Keys::from(settings);
    match fire {
        Fire::Rotate(0) => vec![],
        Fire::Rotate(t) if k.speeds.is_empty() => {
            let key = if t > 0 { k.faster } else { k.slower };
            vec![key; t.unsigned_abs().min(5) as usize]
        }
        Fire::Rotate(t) => {
            let last = k.speeds.len() - 1;
            let level = (st.level.min(last) as i64 + t as i64).clamp(0, last as i64) as usize;
            // At either end, turning further does nothing, unless that unpauses.
            if level == st.level && !st.paused {
                return vec![];
            }
            // Picking a speed unpauses in games with a key per speed.
            *st = Speed { level, paused: false };
            vec![k.speeds[level].clone()]
        }
        Fire::Press | Fire::Touch | Fire::LongTouch => {
            st.paused = !st.paused;
            vec![k.pause]
        }
    }
}

/// What the strip or key says: "Paused", or the current speed.
pub fn status(settings: &Value, st: &Speed) -> String {
    let k = Keys::from(settings);
    if st.paused {
        return "Paused".into();
    }
    if k.speeds.is_empty() {
        return "Playing".into();
    }
    let level = st.level.min(k.speeds.len() - 1);
    k.names.get(level).cloned().unwrap_or_else(|| format!("Speed {}", level + 1))
}

/// Presses `keys` one after another on the virtual keyboard. Blocking.
pub fn press(keys: &[String]) {
    for spec in keys {
        let Some(codes) = crate::uinput::parse_chord(spec) else {
            tracing::warn!("game speed: unknown key {spec:?}");
            return;
        };
        if let Err(e) = crate::uinput::tap(&codes) {
            tracing::warn!("game speed: {e:#}");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn levels() {
        let s = json!({ "speeds": "1, 2, 3", "names": "1×, 2×, 5×" });
        let mut st = initial(&s);
        assert_eq!(st, Speed { level: 0, paused: false });
        assert_eq!(step(&s, &mut st, Fire::Rotate(1)), ["2"]);
        assert_eq!(step(&s, &mut st, Fire::Rotate(4)), ["3"]);
        assert_eq!(status(&s, &st), "5×");
        assert!(step(&s, &mut st, Fire::Rotate(1)).is_empty());
        assert_eq!(step(&s, &mut st, Fire::Press), ["space"]);
        assert_eq!(status(&s, &st), "Paused");
        // Turning at the top end while paused presses the speed again, which unpauses.
        assert_eq!(step(&s, &mut st, Fire::Rotate(1)), ["3"]);
        assert!(!st.paused);
        assert_eq!(step(&s, &mut st, Fire::Rotate(-5)), ["1"]);
    }

    #[test]
    fn faster_slower() {
        let s = json!({ "mode": "step", "faster": "kp_add", "pause": "p" });
        let mut st = initial(&s);
        assert_eq!(step(&s, &mut st, Fire::Rotate(2)), ["kp_add", "kp_add"]);
        assert_eq!(step(&s, &mut st, Fire::Rotate(-1)), ["-"]);
        assert_eq!(step(&s, &mut st, Fire::Touch), ["p"]);
        assert_eq!(status(&s, &st), "Paused");
    }

    #[test]
    fn start_and_defaults() {
        assert_eq!(initial(&json!({ "start": "2" })).level, 1);
        assert_eq!(initial(&json!({ "start": 9 })).level, 2);
        assert_eq!(status(&json!({}), &Speed::default()), "Speed 1");
    }
}

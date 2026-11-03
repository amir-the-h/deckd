//! Auto-click: clicks the mouse repeatedly, through a virtual mouse (see `uinput`).

use std::fs::File;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::uinput::{write_events, EV_KEY, EV_SYN, SYN_REPORT};

pub const ICON: &str = "M9 11.24V7.5a2.5 2.5 0 0 1 5 0v3.74c1.21-.81 2-2.18 2-3.74C16 5.01 13.99 3 11.5 3S7 5.01 7 7.5c0 1.56.79 2.93 2 3.74zm9.84 4.63-4.54-2.26c-.17-.07-.35-.11-.54-.11H13v-6c0-.83-.67-1.5-1.5-1.5S10 6.67 10 7.5v10.74l-3.43-.72c-.08-.01-.15-.03-.24-.03-.31 0-.59.13-.79.33l-.79.8 4.94 4.94c.27.27.65.44 1.06.44h6.79c.75 0 1.33-.55 1.44-1.28l.75-5.27c.01-.07.02-.14.02-.2 0-.62-.38-1.16-.91-1.38z";

const REL_X: u16 = 0;
const REL_Y: u16 = 1;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

/// The virtual mouse, created the first time something clicks.
static MOUSE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

fn create_mouse() -> Result<File> {
    // Pointer motion axes make libinput treat the device as a mouse; deckd never moves it.
    crate::uinput::create("deckd virtual mouse", [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE], &[REL_X, REL_Y])
}

/// Presses (`down`) or releases a mouse button. Returns whether the device was just
/// created, since the compositor needs a moment before it listens to a new device.
fn button(code: u16, down: bool) -> Result<bool> {
    let mut guard = MOUSE.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
    let created = guard.is_none();
    if created {
        *guard = Some(create_mouse()?);
    }
    let f = guard.as_mut().expect("mouse was just created");
    write_events(f, &[(EV_KEY, code, down as i32), (EV_SYN, SYN_REPORT, 0)]).context("write to /dev/uinput")?;
    Ok(created)
}

/// Auto-click settings: which button, how often, and how many clicks (0 = until stopped).
pub struct Clicker {
    pub code: u16,
    pub interval: Duration,
    pub count: u64,
}

impl Clicker {
    pub fn from_settings(settings: &Value) -> Self {
        let num = |k: &str| settings.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
        Clicker {
            code: match settings.get("button").and_then(Value::as_str) {
                Some("right") => BTN_RIGHT,
                Some("middle") => BTN_MIDDLE,
                _ => BTN_LEFT,
            },
            interval: Duration::from_millis(num("interval").unwrap_or(100).max(10)),
            count: num("count").unwrap_or(0),
        }
    }

    /// Clicks until done or the task is aborted.
    pub async fn run(self) -> Result<()> {
        // Long enough for apps to register the press, short enough for fast intervals.
        let hold = (self.interval / 2).min(Duration::from_millis(20));
        let mut n = 0;
        while self.count == 0 || n < self.count {
            if tokio::task::spawn_blocking(move || button(self.code, true)).await?? {
                // Give the compositor time to pick up the new device, then click again.
                tokio::time::sleep(Duration::from_millis(300)).await;
                tokio::task::spawn_blocking(move || button(self.code, true)).await??;
            }
            tokio::time::sleep(hold).await;
            tokio::task::spawn_blocking(move || button(self.code, false)).await??;
            n += 1;
            tokio::time::sleep(self.interval - hold).await;
        }
        Ok(())
    }
}

/// Lets go of `code`, so stopping mid-click never leaves the button held down.
pub fn release(code: u16) {
    let Some(m) = MOUSE.get() else { return };
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = guard.as_mut() {
        let _ = write_events(f, &[(EV_KEY, code, 0), (EV_SYN, SYN_REPORT, 0)]);
    }
}

//! Virtual input devices on /dev/uinput, so deckd can click and type on any compositor
//! (and X11) without helper tools. deckd needs write access to /dev/uinput;
//! dist/70-deckd.rules grants it to the logged-in user.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const SYN_REPORT: u16 = 0;

// ioctl numbers from <linux/uinput.h>; the same on x86_64 and aarch64.
const UI_DEV_CREATE: u64 = 0x5501;
const UI_DEV_SETUP: u64 = 0x405c_5503;
const UI_SET_EVBIT: u64 = 0x4004_5564;
const UI_SET_KEYBIT: u64 = 0x4004_5565;
const UI_SET_RELBIT: u64 = 0x4004_5566;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

/// Creates a virtual device that can send `keys` (key or button codes) and `rel` axes.
pub fn create(name: &str, keys: impl IntoIterator<Item = u16>, rel: &[u16]) -> Result<File> {
    let f = OpenOptions::new().write(true).open("/dev/uinput").context("open /dev/uinput")?;
    let fd = f.as_raw_fd();
    let mut setup = UinputSetup { bustype: 0x06 /* BUS_VIRTUAL */, vendor: 0, product: 0, version: 1, name: [0; 80], ff_effects_max: 0 };
    let n = name.len().min(79);
    setup.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    let ok = unsafe {
        ioctl(fd, UI_SET_EVBIT, EV_KEY as i32) >= 0
            && keys.into_iter().all(|k| ioctl(fd, UI_SET_KEYBIT, k as i32) >= 0)
            && (rel.is_empty() || ioctl(fd, UI_SET_EVBIT, EV_REL as i32) >= 0)
            && rel.iter().all(|r| ioctl(fd, UI_SET_RELBIT, *r as i32) >= 0)
            && ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup) >= 0
            && ioctl(fd, UI_DEV_CREATE) >= 0
    };
    if !ok {
        bail!("set up uinput device: {}", std::io::Error::last_os_error());
    }
    Ok(f)
}

pub fn write_events(f: &mut File, events: &[(u16, u16, i32)]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(events.len() * 24);
    for &(kind, code, value) in events {
        // struct input_event: timeval (left zero; the kernel stamps it), type, code, value.
        buf.extend_from_slice(&[0u8; 16]);
        buf.extend_from_slice(&kind.to_ne_bytes());
        buf.extend_from_slice(&code.to_ne_bytes());
        buf.extend_from_slice(&value.to_ne_bytes());
    }
    f.write_all(&buf)
}

// ------------------------------------------------------------------ keyboard

/// The virtual keyboard, created the first time something types.
static KEYBOARD: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Holds `codes` down together for `hold`, then releases them in reverse order.
/// Blocking; the focused window receives the keys.
pub fn chord(codes: &[u16], hold: Duration) -> Result<()> {
    chord_then(codes, hold, Duration::ZERO)
}

/// A short key press for games, which read keys once a frame: held long enough to be
/// seen, then the keyboard stays idle a moment so back-to-back taps register apart.
pub fn tap(codes: &[u16]) -> Result<()> {
    chord_then(codes, Duration::from_millis(50), Duration::from_millis(50))
}

/// Parses a key or chord such as `space`, `3`, `shift+1` or `ctrl+kp_add`.
pub fn parse_chord(spec: &str) -> Option<Vec<u16>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    // A lone "+" (or one ending a chord, as in "shift++") is the key itself.
    let (mods, last) = match spec.strip_suffix("++") {
        Some(m) => (m, "+"),
        None if spec == "+" => ("", "+"),
        None => spec.rsplit_once('+').unwrap_or(("", spec)),
    };
    let mut codes: Vec<u16> = mods.split('+').filter(|m| !m.trim().is_empty()).map(key_code).collect::<Option<_>>()?;
    match last.trim() {
        // US layout: "+" is shift and "=".
        "+" | "plus" => codes.extend([42, 13]),
        k => codes.push(key_code(k)?),
    }
    Some(codes)
}

fn chord_then(codes: &[u16], hold: Duration, gap: Duration) -> Result<()> {
    let mut guard = KEYBOARD.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        // KEY_ESC..=KEY_MICMUTE: a full keyboard, so libinput treats it as one.
        *guard = Some(create("deckd virtual keyboard", 1..=248, &[])?);
        // The compositor needs a moment before it listens to a new device.
        std::thread::sleep(Duration::from_millis(300));
    }
    let f = guard.as_mut().expect("keyboard was just created");
    let send = |f: &mut File, code: u16, down: bool| write_events(f, &[(EV_KEY, code, down as i32), (EV_SYN, SYN_REPORT, 0)]);
    let pressed = codes.iter().try_for_each(|&c| send(f, c, true));
    if pressed.is_ok() {
        std::thread::sleep(hold);
    }
    // Release even after a failed press, so nothing is left held down.
    for &c in codes.iter().rev() {
        let _ = send(f, c, false);
    }
    if !gap.is_zero() {
        std::thread::sleep(gap);
    }
    pressed.context("write to /dev/uinput")
}

/// Linux key code for an X keysym name such as `Shift_R`, `F12` or `a` (case-insensitive,
/// as MangoHud and xkb accept them). Letters and symbols assume a US layout.
pub fn key_code(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_lowercase();
    if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<u16>().ok()) {
        return match f {
            1..=10 => Some(58 + f),
            11 | 12 => Some(76 + f),
            13..=24 => Some(170 + f),
            _ => None,
        };
    }
    if n.len() == 1 {
        let c = n.as_bytes()[0];
        const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
        return match c {
            b'a'..=b'z' => Some(LETTERS[(c - b'a') as usize]),
            b'1'..=b'9' => Some((c - b'1') as u16 + 2),
            b'0' => Some(11),
            b'-' => Some(12),
            b'=' => Some(13),
            b'[' => Some(26),
            b']' => Some(27),
            b';' => Some(39),
            b'\'' => Some(40),
            b'`' => Some(41),
            b'\\' => Some(43),
            b',' => Some(51),
            b'.' => Some(52),
            b'/' => Some(53),
            _ => None,
        };
    }
    if let Some(k) = n.strip_prefix("kp_") {
        if let Ok(d) = k.parse::<usize>() {
            return [82, 79, 80, 81, 75, 76, 77, 71, 72, 73].get(d).copied();
        }
    }
    Some(match n.as_str() {
        "shift_l" | "shift" => 42,
        "shift_r" => 54,
        "control_l" | "ctrl" | "control" => 29,
        "control_r" => 97,
        "alt_l" | "meta_l" | "alt" => 56,
        "alt_r" | "meta_r" | "iso_level3_shift" => 100,
        "super_l" | "super" => 125,
        "super_r" => 126,
        "escape" | "esc" => 1,
        "minus" => 12,
        "equal" => 13,
        "backspace" => 14,
        "tab" => 15,
        "bracketleft" => 26,
        "bracketright" => 27,
        "return" | "enter" => 28,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "backslash" => 43,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        "space" => 57,
        "caps_lock" => 58,
        "num_lock" => 69,
        "scroll_lock" => 70,
        "kp_multiply" => 55,
        "kp_subtract" => 74,
        "kp_add" => 78,
        "kp_decimal" => 83,
        "kp_enter" => 96,
        "kp_divide" => 98,
        "print" | "sys_req" => 99,
        "home" => 102,
        "up" => 103,
        "prior" | "page_up" | "pgup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "next" | "page_down" | "pgdn" => 109,
        "insert" => 110,
        "delete" => 111,
        "pause" | "break" => 119,
        "menu" => 127,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::key_code;

    #[test]
    fn keysyms() {
        assert_eq!(key_code("Shift_R"), Some(54));
        assert_eq!(key_code("F12"), Some(88));
        assert_eq!(key_code("f1"), Some(59));
        assert_eq!(key_code("F13"), Some(183));
        assert_eq!(key_code("a"), Some(30));
        assert_eq!(key_code("Z"), Some(44));
        assert_eq!(key_code("0"), Some(11));
        assert_eq!(key_code("KP_0"), Some(82));
        assert_eq!(key_code("Pause"), Some(119));
        assert_eq!(key_code("nope"), None);
        assert_eq!(key_code("-"), Some(12));
    }

    #[test]
    fn chords() {
        use super::parse_chord;
        assert_eq!(parse_chord("space"), Some(vec![57]));
        assert_eq!(parse_chord("Shift+1"), Some(vec![42, 2]));
        assert_eq!(parse_chord("ctrl + kp_add"), Some(vec![29, 78]));
        assert_eq!(parse_chord("+"), Some(vec![42, 13]));
        assert_eq!(parse_chord("ctrl++"), Some(vec![29, 42, 13]));
        assert_eq!(parse_chord("-"), Some(vec![12]));
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("shift+nope"), None);
    }
}

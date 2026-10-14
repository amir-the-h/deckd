//! Built-in actions implemented natively in the daemon (no plugin process).

use serde_json::Value;

pub const PAGE: &str = "deckd.page";
pub const COMMAND: &str = "deckd.command";
pub const DIAL: &str = "deckd.dial";
pub const BRIGHTNESS: &str = "deckd.brightness";
/// Handled by the device actor, which owns the audio state.
pub const VOLUME: &str = "deckd.volume";

pub fn is_builtin(uuid: &str) -> bool {
    uuid.starts_with("deckd.")
}

/// Something a built-in action asks the device to do.
#[derive(Debug)]
pub enum Effect {
    SwitchPage(String),
    Back,
    Brightness { delta: i32 },
}

/// What happened to the input the action is bound to.
#[derive(Debug, Clone, Copy)]
pub enum Fire {
    Press,
    Rotate(i8),
    Touch,
    LongTouch,
}

pub fn run(uuid: &str, settings: &Value, fire: Fire) -> Option<Effect> {
    let s = |k: &str| settings.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    match (uuid, fire) {
        (PAGE, Fire::Press | Fire::Touch) => match s("page")? {
            "@back" => Some(Effect::Back),
            page => Some(Effect::SwitchPage(page.to_string())),
        },
        (COMMAND, Fire::Press | Fire::Touch) => {
            spawn_shell(s("command")?);
            None
        }
        (DIAL, fire) => {
            let cmd = match fire {
                Fire::Rotate(t) if t > 0 => s("cw"),
                Fire::Rotate(_) => s("ccw"),
                Fire::Press => s("press"),
                Fire::Touch => s("touch"),
                Fire::LongTouch => s("long_touch").or(s("touch")),
            }?;
            let ticks = if let Fire::Rotate(t) = fire { t.unsigned_abs() } else { 1 };
            spawn_shell(&cmd.replace("{ticks}", &ticks.to_string()));
            None
        }
        (BRIGHTNESS, Fire::Press | Fire::Touch) => {
            Some(Effect::Brightness { delta: settings.get("delta").and_then(Value::as_i64).unwrap_or(10) as i32 })
        }
        (BRIGHTNESS, Fire::Rotate(t)) => Some(Effect::Brightness { delta: t as i32 * 5 }),
        _ => None,
    }
}

/// Runs a shell command fully detached so it outlives the daemon and never blocks it.
pub fn spawn_shell(cmd: &str) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    tracing::info!("exec: {cmd}");
    let result = unsafe {
        Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .pre_exec(|| {
                libc_setsid();
                Ok(())
            })
            .spawn()
    };
    match result {
        // Reap in the background so we don't accumulate zombies.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("failed to run {cmd:?}: {e}"),
    }
}

fn libc_setsid() {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}

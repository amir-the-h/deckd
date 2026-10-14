//! Recovers Stream Decks left without a kernel driver.
//!
//! Apps that use libusb (StreamController) detach usbhid from the deck and
//! don't reattach it on exit, leaving no hidraw node. A USB reset makes the
//! kernel re-probe the device, which needs only the uaccess ACL on
//! /dev/bus/usb, not root.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

const ELGATO_VID: &str = "0fd9";
const USBDEVFS_RESET: u64 = 0x5514; // _IO('U', 20)
const USBDEVFS_IOCTL: u64 = 0xC010_5512; // _IOWR('U', 18, struct usbdevfs_ioctl)
const USBDEVFS_CONNECT: i32 = 0x5517; // _IO('U', 23)

#[repr(C)]
struct UsbdevfsIoctl {
    ifno: i32,
    ioctl_code: i32,
    data: *mut std::ffi::c_void,
}

#[derive(Default)]
pub struct Recovery {
    last_attempt: HashMap<String, Instant>,
}

impl Recovery {
    pub fn check(&mut self) {
        let Ok(entries) = std::fs::read_dir("/sys/bus/usb/devices") else { return };
        for entry in entries.flatten() {
            let dev = entry.path();
            if read(&dev.join("idVendor")).as_deref() != Some(ELGATO_VID) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // Interface 0 of the deck is the HID interface.
            let iface = dev.join(format!("{name}:1.0"));
            if !iface.exists() || iface.join("driver").exists() {
                continue;
            }
            if self.last_attempt.get(&name).is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                continue;
            }
            self.last_attempt.insert(name.clone(), Instant::now());
            let (Some(bus), Some(num)) = (read(&dev.join("busnum")), read(&dev.join("devnum"))) else { continue };
            let node = format!("/dev/bus/usb/{:03}/{:03}", bus.parse::<u32>().unwrap_or(0), num.parse::<u32>().unwrap_or(0));
            // A libusb app that is still running owns the device; leave it alone.
            if let Some(pid) = holder(&node) {
                tracing::debug!("{name} is held by pid {pid}; not resetting");
                continue;
            }
            // Reattach usbhid first; a reset helps if the firmware is wedged as well.
            let result = attach_driver(&node).or_else(|_| reset(&node).and_then(|_| {
                std::thread::sleep(Duration::from_millis(500));
                attach_driver(&node)
            }));
            match result {
                Ok(()) => tracing::info!("reattached HID driver to {name} ({node})"),
                Err(e) => tracing::warn!("{name} has no HID driver and reattaching failed: {e}; replug it"),
            }
        }
    }
}

/// Finds a process with `node` open. Only sees our own user's processes, which
/// is where a desktop Stream Deck app would run.
fn holder(node: &str) -> Option<u32> {
    let me = std::process::id();
    for proc in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = proc.file_name().to_string_lossy().parse::<u32>() else { continue };
        if pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else { continue };
        if fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l == Path::new(node))) {
            return Some(pid);
        }
    }
    None
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

unsafe extern "C" {
    fn ioctl(fd: i32, request: u64, ...) -> i32;
}

fn attach_driver(node: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new().write(true).open(node)?;
    let mut cmd = UsbdevfsIoctl { ifno: 0, ioctl_code: USBDEVFS_CONNECT, data: std::ptr::null_mut() };
    if unsafe { ioctl(f.as_raw_fd(), USBDEVFS_IOCTL, &mut cmd as *mut UsbdevfsIoctl) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn reset(node: &str) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new().write(true).open(node)?;
    let rc = unsafe { ioctl(f.as_raw_fd(), USBDEVFS_RESET, 0) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

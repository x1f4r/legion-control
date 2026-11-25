//! Process-bound desktop keep-screen-on implementation.
//!
//! - **Linux**: holds an idle inhibition directly on a session D-Bus connection owned by this
//!   process — trying `org.freedesktop.portal.Inhibit` (idle flag `8`) first and falling back to
//!   `org.freedesktop.ScreenSaver.Inhibit`. Zero helper processes are spawned, so if the app exits
//!   or is killed (`SIGKILL`) the D-Bus socket closes and the desktop releases the inhibition at
//!   once.
//! - **Windows**: calls `SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED)` on an
//!   app-owned thread via `legion_os::display_awake::DisplayAwakeHold`, cleared on release/drop
//!   and released automatically by the OS if the process ends.
//! - **macOS**: runs `caffeinate -d -w <pid>`, tied to this process's PID by the OS.

use std::sync::Mutex;

#[cfg(target_os = "macos")]
use std::process::{Child, Command, Stdio};

/// Desktop Portal `org.freedesktop.portal.Inhibit` flag for inhibiting idle / screen blanking
/// (`1 = Logout`, `2 = UserSwitch`, `4 = Suspend`, `8 = Idle`).
#[cfg(any(target_os = "linux", test))]
pub const PORTAL_INHIBIT_IDLE: u32 = 8;

/// Holds an active desktop display-idle inhibition tied to the lifetime of this process.
#[derive(Default)]
pub struct DesktopKeepScreenOn {
    #[cfg(target_os = "linux")]
    hold: Mutex<Option<LinuxInhibitHold>>,
    #[cfg(target_os = "macos")]
    hold: Mutex<Option<Child>>,
    #[cfg(target_os = "windows")]
    hold: Mutex<Option<legion_os::display_awake::DisplayAwakeHold>>,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    _mobile: Mutex<()>,
}

impl DesktopKeepScreenOn {
    /// Acquires or releases the desktop screen-awake hold (best-effort, silent if unavailable).
    pub fn set(&self, on: bool) {
        #[cfg(target_os = "linux")]
        {
            let mut guard = self
                .hold
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if on {
                if guard.is_some() {
                    return;
                }
                *guard = acquire_linux_dbus_hold();
            } else if let Some(hold) = guard.take() {
                hold.release();
            }
        }

        #[cfg(target_os = "macos")]
        {
            let mut guard = self
                .hold
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if on {
                if let Some(child) = guard.as_mut()
                    && child.try_wait().ok().flatten().is_none()
                {
                    return;
                }
                *guard = spawn_macos_caffeinate();
            } else if let Some(mut child) = guard.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }

        #[cfg(target_os = "windows")]
        {
            let mut guard = self
                .hold
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if on {
                let hold =
                    guard.get_or_insert_with(legion_os::display_awake::DisplayAwakeHold::new);
                hold.set(true);
            } else if let Some(mut hold) = guard.take() {
                hold.set(false);
            }
        }

        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = (&self._mobile, on);
        }
    }

    /// Releases any active display-idle inhibition immediately.
    pub fn release(&self) {
        self.set(false);
    }

    /// Reports whether this holder currently owns an active inhibition.
    pub fn is_active(&self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        {
            let guard = self
                .hold
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.is_some()
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            let _ = &self._mobile;
            false
        }
    }
}

impl Drop for DesktopKeepScreenOn {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(target_os = "macos")]
fn spawn_macos_caffeinate() -> Option<Child> {
    let pid = std::process::id().to_string();
    Command::new("caffeinate")
        .args(["-d", "-w", &pid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

#[cfg(target_os = "linux")]
enum LinuxInhibitHold {
    Portal {
        conn: zbus::blocking::Connection,
        request_path: zbus::zvariant::OwnedObjectPath,
    },
    ScreenSaver {
        conn: zbus::blocking::Connection,
        cookie: u32,
    },
}

#[cfg(target_os = "linux")]
impl LinuxInhibitHold {
    fn release(self) {
        match self {
            LinuxInhibitHold::Portal { conn, request_path } => {
                let _ = conn.call_method(
                    Some("org.freedesktop.portal.Desktop"),
                    request_path.as_ref(),
                    Some("org.freedesktop.portal.Request"),
                    "Close",
                    &(),
                );
                drop(conn);
            }
            LinuxInhibitHold::ScreenSaver { conn, cookie } => {
                let _ = conn.call_method(
                    Some("org.freedesktop.ScreenSaver"),
                    "/org/freedesktop/ScreenSaver",
                    Some("org.freedesktop.ScreenSaver"),
                    "UnInhibit",
                    &(cookie,),
                );
                drop(conn);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn acquire_linux_dbus_hold() -> Option<LinuxInhibitHold> {
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedObjectPath, Value};

    let conn = zbus::blocking::Connection::session().ok()?;

    // 1. Desktop Portal Inhibit (idle flag 8) — works across Wayland, GNOME, KDE, Hyprland, etc.
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("reason", Value::from("Playing video"));
    if let Ok(reply) = conn.call_method(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        Some("org.freedesktop.portal.Inhibit"),
        "Inhibit",
        &("", PORTAL_INHIBIT_IDLE, options),
    ) && let Ok(request_path) = reply.body().deserialize::<OwnedObjectPath>()
    {
        return Some(LinuxInhibitHold::Portal { conn, request_path });
    }

    // 2. Fallback: org.freedesktop.ScreenSaver.Inhibit.
    if let Ok(reply) = conn.call_method(
        Some("org.freedesktop.ScreenSaver"),
        "/org/freedesktop/ScreenSaver",
        Some("org.freedesktop.ScreenSaver"),
        "Inhibit",
        &("Legion", "Playing video"),
    ) && let Ok(cookie) = reply.body().deserialize::<u32>()
    {
        return Some(LinuxInhibitHold::ScreenSaver { conn, cookie });
    }

    None
}

/// Pure strategy selector used by unit tests to verify the Linux D-Bus portal-first /
/// ScreenSaver-fallback order without requiring a live desktop portal in headless CI.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
enum SelectedLinuxInhibit {
    Portal(String),
    ScreenSaver(u32),
}

#[cfg(test)]
fn select_linux_inhibit<P, S>(portal: P, screensaver: S) -> Option<SelectedLinuxInhibit>
where
    P: FnOnce(u32) -> Option<String>,
    S: FnOnce() -> Option<u32>,
{
    if let Some(handle) = portal(PORTAL_INHIBIT_IDLE) {
        return Some(SelectedLinuxInhibit::Portal(handle));
    }
    screensaver().map(SelectedLinuxInhibit::ScreenSaver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_inhibit_prefers_portal_idle_and_falls_back_to_screensaver() {
        let mut screensaver_called = false;
        let first = select_linux_inhibit(
            |flags| {
                assert_eq!(flags, PORTAL_INHIBIT_IDLE);
                Some("/org/freedesktop/portal/desktop/request/1/legion".to_owned())
            },
            || {
                screensaver_called = true;
                Some(42)
            },
        );
        assert_eq!(
            first,
            Some(SelectedLinuxInhibit::Portal(
                "/org/freedesktop/portal/desktop/request/1/legion".to_owned()
            ))
        );
        assert!(
            !screensaver_called,
            "ScreenSaver fallback must not be called when Desktop Portal succeeds"
        );

        let fallback = select_linux_inhibit(
            |flags| {
                assert_eq!(flags, PORTAL_INHIBIT_IDLE);
                None
            },
            || Some(77),
        );
        assert_eq!(fallback, Some(SelectedLinuxInhibit::ScreenSaver(77)));

        let neither = select_linux_inhibit(|_| None, || None);
        assert_eq!(neither, None);
    }

    #[test]
    fn desktop_keep_screen_on_releases_cleanly_on_set_false_and_drop() {
        let keeper = DesktopKeepScreenOn::default();
        assert!(!keeper.is_active());
        keeper.set(true);
        keeper.set(false);
        assert!(!keeper.is_active());
        keeper.set(true);
        drop(keeper);
    }
}

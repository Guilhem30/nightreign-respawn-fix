//! `respawn_gate_fix.log`: timestamped lines, appended across sessions.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use windows::Win32::System::SystemInformation::GetLocalTime;

/// Appends timestamped lines to the log; does nothing if it cannot be opened.
pub struct Log(Option<File>);

impl Log {
    /// Opens (or creates) the log for appending.
    pub fn open(path: &Path) -> Self {
        Self(OpenOptions::new().create(true).append(true).open(path).ok())
    }

    pub fn line(&mut self, msg: &str) {
        // SAFETY: `GetLocalTime` only fills in and returns a struct.
        let t = unsafe { GetLocalTime() };
        if let Some(f) = &mut self.0 {
            let _ = writeln!(
                f,
                "{:02}:{:02}:{:02}.{:03} {msg}",
                t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
            );
            let _ = f.flush();
        }
    }
}

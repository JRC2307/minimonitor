use std::process::{Child, Command};

/// Holds the `caffeinate` child while keep-awake is on.
pub struct Caffeinate {
    child: Option<Child>,
}

impl Caffeinate {
    pub fn new() -> Self {
        Self { child: None }
    }

    pub fn is_on(&self) -> bool {
        self.child.is_some()
    }

    /// Turn keep-awake on/off. Returns the resulting state.
    pub fn set(&mut self, on: bool) -> bool {
        if on {
            if self.child.is_none() {
                self.child = Command::new("caffeinate").arg("-dimsu").spawn().ok();
            }
        } else if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.is_on()
    }
}

impl Drop for Caffeinate {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
    }
}

/// Flush the macOS DNS cache. Requires root, so this runs via osascript and
/// pops the native administrator-password dialog.
pub fn flush_dns() -> Result<(), String> {
    let script = "do shell script \"dscacheutil -flushcache; killall -HUP mDNSResponder\" \
                  with administrator privileges";
    let status = Command::new("osascript")
        .args(["-e", script])
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("flush-dns exited with {status}"))
    }
}

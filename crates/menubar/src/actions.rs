use std::io::ErrorKind;
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

// ---------------------------------------------------------------------------
// Display resolution presets
//
// Switch the mini's connected display to a resolution/orientation matched to the
// device used to remote in via RustDesk. Shells out to `displayplacer`
// (brew install displayplacer), consistent with the caffeinate/osascript pattern.
// ---------------------------------------------------------------------------

const DISPLAYPLACER_MISSING: &str =
    "displayplacer not installed — run: brew install displayplacer";

/// Homebrew install locations, searched in order. Needed because the app runs
/// under launchd, whose PATH is just `/usr/bin:/bin:/usr/sbin:/sbin` and does
/// not include Homebrew's bin — so a bare `displayplacer` would never resolve.
const DISPLAYPLACER_CANDIDATES: [&str; 2] = [
    "/opt/homebrew/bin/displayplacer", // Apple Silicon
    "/usr/local/bin/displayplacer",    // Intel
];

/// Pick the first candidate path that exists, else fall back to the bare name
/// (resolved via PATH — works when run from a shell or tests).
fn resolve_bin(candidates: &[&str], bare: &str, exists: impl Fn(&str) -> bool) -> String {
    candidates
        .iter()
        .find(|c| exists(c))
        .map(|c| (*c).to_owned())
        .unwrap_or_else(|| bare.to_owned())
}

fn displayplacer_bin() -> String {
    resolve_bin(&DISPLAYPLACER_CANDIDATES, "displayplacer", |p| {
        std::path::Path::new(p).exists()
    })
}

/// A display preset matched to a RustDesk client device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreset {
    /// The mini's own ViewSonic panel, full native landscape.
    MiniNative,
    /// 16:10 to match the MacBook Air's aspect.
    MacBookAir,
    /// True portrait for an iPhone held vertically (1920x1080 rotated 90°).
    IPhonePortrait,
}

impl DisplayPreset {
    pub const ALL: [DisplayPreset; 3] = [
        DisplayPreset::MiniNative,
        DisplayPreset::MacBookAir,
        DisplayPreset::IPhonePortrait,
    ];

    /// Target `(width, height, degree)`. The dimensions are the resolution as
    /// displayplacer reports/expects it *for that rotation* — so portrait uses
    /// the rotated 1080x1920 with degree 90 (passing 1920x1080 with degree 90
    /// makes displayplacer fail to match the rotated mode list).
    pub fn target(self) -> (u32, u32, u32) {
        match self {
            DisplayPreset::MiniNative => (1920, 1080, 0),
            DisplayPreset::MacBookAir => (1440, 900, 0),
            DisplayPreset::IPhonePortrait => (1080, 1920, 90),
        }
    }

    /// Stable menu-item id (`display:...`).
    pub fn menu_id(self) -> &'static str {
        match self {
            DisplayPreset::MiniNative => "display:native",
            DisplayPreset::MacBookAir => "display:mba",
            DisplayPreset::IPhonePortrait => "display:iphone",
        }
    }

    /// Human label shown in the menu and status line (effective resolution).
    pub fn label(self) -> &'static str {
        match self {
            DisplayPreset::MiniNative => "Mini screen (1920×1080)",
            DisplayPreset::MacBookAir => "MacBook Air (1440×900)",
            DisplayPreset::IPhonePortrait => "iPhone portrait (1080×1920)",
        }
    }

    /// Resolve a `display:*` menu id back to a preset.
    pub fn from_menu_id(id: &str) -> Option<DisplayPreset> {
        DisplayPreset::ALL.into_iter().find(|p| p.menu_id() == id)
    }
}

/// The current state of a display as reported by `displayplacer list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayMode {
    pub screen_id: String,
    pub width: u32,
    pub height: u32,
    pub hz: String,
    pub color_depth: String,
    pub degree: u32,
}

impl DisplayMode {
    /// The preset whose target matches this mode, if any.
    pub fn active_preset(&self) -> Option<DisplayPreset> {
        let cur = (self.width, self.height, self.degree);
        DisplayPreset::ALL.into_iter().find(|p| p.target() == cur)
    }
}

/// Parse the header block of `displayplacer list` for the main/first display,
/// extracting the persistent screen id and current mode.
pub fn parse_displayplacer_list(output: &str) -> Result<DisplayMode, String> {
    let mut screen_id = None;
    let mut resolution = None;
    let mut hz = None;
    let mut color_depth = None;
    let mut degree = None;

    for line in output.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("Persistent screen id:") {
            screen_id.get_or_insert(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Resolution:") {
            resolution.get_or_insert(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Hertz:") {
            hz.get_or_insert(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Color Depth:") {
            color_depth.get_or_insert(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("Rotation:") {
            degree.get_or_insert(v.trim().to_owned());
        }
    }

    let screen_id = screen_id.ok_or("no persistent screen id in displayplacer output")?;
    let resolution = resolution.ok_or("no resolution in displayplacer output")?;
    let (w, h) = resolution
        .split_once('x')
        .ok_or_else(|| format!("malformed resolution: {resolution}"))?;
    let width = w
        .trim()
        .parse()
        .map_err(|_| format!("malformed width: {w}"))?;
    let height = h
        .trim()
        .parse()
        .map_err(|_| format!("malformed height: {h}"))?;
    let degree = degree
        .unwrap_or_else(|| "0".to_owned())
        .parse()
        .unwrap_or(0);

    Ok(DisplayMode {
        screen_id,
        width,
        height,
        hz: hz.unwrap_or_else(|| "60".to_owned()),
        color_depth: color_depth.unwrap_or_else(|| "8".to_owned()),
        degree,
    })
}

/// Build the single-argument string passed to `displayplacer`, reusing the
/// detected screen id / hz / color depth and applying the preset's mode.
pub fn build_apply_command(mode: &DisplayMode, preset: DisplayPreset) -> String {
    let (w, h, degree) = preset.target();
    format!(
        "id:{} res:{}x{} hz:{} color_depth:{} enabled:true scaling:off origin:(0,0) degree:{}",
        mode.screen_id, w, h, mode.hz, mode.color_depth, degree
    )
}

/// Read the current display mode via `displayplacer list`.
pub fn current_mode() -> Result<DisplayMode, String> {
    let output = Command::new(displayplacer_bin())
        .arg("list")
        .output()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                DISPLAYPLACER_MISSING.to_owned()
            } else {
                e.to_string()
            }
        })?;
    if !output.status.success() {
        return Err(format!("displayplacer list exited with {}", output.status));
    }
    parse_displayplacer_list(&String::from_utf8_lossy(output.stdout.as_slice()))
}

/// Apply a display preset. Returns the resulting mode on success.
pub fn apply_preset(preset: DisplayPreset) -> Result<DisplayMode, String> {
    let mode = current_mode()?;
    let arg = build_apply_command(&mode, preset);
    let status = Command::new(displayplacer_bin())
        .arg(&arg)
        .status()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                DISPLAYPLACER_MISSING.to_owned()
            } else {
                e.to_string()
            }
        })?;
    if !status.success() {
        return Err(format!("displayplacer exited with {status}"));
    }
    let (width, height, degree) = preset.target();
    Ok(DisplayMode {
        width,
        height,
        degree,
        ..mode
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
Persistent screen id: 42F51F97-F0CF-465A-A6CD-608A3D4E0DA5
Contextual screen id: 3
Serial screen id: s16843009
Type: 23 inch external screen
Resolution: 1280x720
Hertz: 60
Color Depth: 8
Scaling: off
Origin: (0,0) - main display
Rotation: 0
Enabled: true
Resolutions for rotation 0:
  mode 21: res:1280x720 hz:60 color_depth:8 <-- current mode
  mode 38: res:1920x1080 hz:60 color_depth:8
";

    #[test]
    fn parses_screen_id_and_current_mode() {
        let mode = parse_displayplacer_list(SAMPLE).unwrap();
        assert_eq!(mode.screen_id, "42F51F97-F0CF-465A-A6CD-608A3D4E0DA5");
        assert_eq!(mode.width, 1280);
        assert_eq!(mode.height, 720);
        assert_eq!(mode.hz, "60");
        assert_eq!(mode.color_depth, "8");
        assert_eq!(mode.degree, 0);
    }

    #[test]
    fn parse_errors_without_screen_id() {
        assert!(parse_displayplacer_list("Resolution: 1280x720\n").is_err());
    }

    #[test]
    fn builds_command_for_each_preset() {
        let mode = parse_displayplacer_list(SAMPLE).unwrap();
        let id = "42F51F97-F0CF-465A-A6CD-608A3D4E0DA5";

        assert_eq!(
            build_apply_command(&mode, DisplayPreset::MiniNative),
            format!("id:{id} res:1920x1080 hz:60 color_depth:8 enabled:true scaling:off origin:(0,0) degree:0")
        );
        assert_eq!(
            build_apply_command(&mode, DisplayPreset::MacBookAir),
            format!("id:{id} res:1440x900 hz:60 color_depth:8 enabled:true scaling:off origin:(0,0) degree:0")
        );
        assert_eq!(
            build_apply_command(&mode, DisplayPreset::IPhonePortrait),
            format!("id:{id} res:1080x1920 hz:60 color_depth:8 enabled:true scaling:off origin:(0,0) degree:90")
        );
    }

    #[test]
    fn active_preset_matches_target() {
        let native = DisplayMode {
            screen_id: "x".into(),
            width: 1920,
            height: 1080,
            hz: "60".into(),
            color_depth: "8".into(),
            degree: 0,
        };
        assert_eq!(native.active_preset(), Some(DisplayPreset::MiniNative));

        let portrait = DisplayMode {
            width: 1080,
            height: 1920,
            degree: 90,
            ..native.clone()
        };
        assert_eq!(portrait.active_preset(), Some(DisplayPreset::IPhonePortrait));

        let mba = DisplayMode {
            width: 1440,
            height: 900,
            ..native.clone()
        };
        assert_eq!(mba.active_preset(), Some(DisplayPreset::MacBookAir));

        let off_list = DisplayMode {
            width: 1280,
            height: 720,
            ..native
        };
        assert_eq!(off_list.active_preset(), None);
    }

    #[test]
    fn resolve_bin_prefers_existing_then_falls_back() {
        let candidates = ["/opt/homebrew/bin/displayplacer", "/usr/local/bin/displayplacer"];
        // Only the Intel path exists.
        assert_eq!(
            resolve_bin(&candidates, "displayplacer", |p| p == "/usr/local/bin/displayplacer"),
            "/usr/local/bin/displayplacer"
        );
        // First match wins when several exist.
        assert_eq!(
            resolve_bin(&candidates, "displayplacer", |_| true),
            "/opt/homebrew/bin/displayplacer"
        );
        // None exist → bare name (PATH lookup).
        assert_eq!(
            resolve_bin(&candidates, "displayplacer", |_| false),
            "displayplacer"
        );
    }

    #[test]
    fn menu_id_round_trips() {
        for preset in DisplayPreset::ALL {
            assert_eq!(DisplayPreset::from_menu_id(preset.menu_id()), Some(preset));
        }
        assert_eq!(DisplayPreset::from_menu_id("display:nope"), None);
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

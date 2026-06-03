use tao::window::Icon as TaoIcon;
use tray_icon::Icon as TrayAppIcon;

pub fn percentage(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        used as f32 / total as f32 * 100.0
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut i = 0;
    while value >= 1024.0 && i < UNITS.len() - 1 {
        value /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[i])
    }
}

pub fn format_bytes_pair(used: u64, total: u64) -> String {
    format!("{} / {}", format_bytes(used), format_bytes(total))
}

pub fn format_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_sec))
}

pub fn truncate_name(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    let truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

pub fn slugify(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

pub fn capture_label() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("epoch {now}")
}

pub fn make_tray_icon() -> TrayAppIcon {
    TrayAppIcon::from_rgba(make_icon_rgba(), 64, 64).expect("tray icon")
}

pub fn make_window_icon() -> TaoIcon {
    TaoIcon::from_rgba(make_icon_rgba(), 64, 64).expect("window icon")
}

fn make_icon_rgba() -> Vec<u8> {
    let w = 64usize;
    let h = 64usize;
    let mut rgba = vec![0u8; w * h * 4];
    fill_rect(&mut rgba, w, 0, 0, w, h, [12, 16, 20, 255]);
    fill_rect(&mut rgba, w, 6, 6, 52, 52, [24, 31, 39, 255]);
    let accent = [237, 241, 246, 255];
    fill_rect(&mut rgba, w, 12, 12, 8, 40, accent);
    fill_rect(&mut rgba, w, 20, 20, 8, 16, accent);
    fill_rect(&mut rgba, w, 28, 12, 8, 40, accent);
    fill_rect(&mut rgba, w, 40, 12, 8, 40, accent);
    fill_rect(&mut rgba, w, 48, 12, 8, 40, accent);
    fill_rect(&mut rgba, w, 40, 28, 16, 8, accent);
    rgba
}

fn fill_rect(
    rgba: &mut [u8],
    width: usize,
    x: usize,
    y: usize,
    rw: usize,
    rh: usize,
    color: [u8; 4],
) {
    for yy in y..(y + rh) {
        for xx in x..(x + rw) {
            let i = (yy * width + xx) * 4;
            rgba[i..i + 4].copy_from_slice(&color);
        }
    }
}

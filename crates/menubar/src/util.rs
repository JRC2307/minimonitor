pub use minimonitor_core::util::{
    format_bytes, format_bytes_pair, format_rate, percentage, slugify, truncate_name,
};

use tao::window::Icon as TaoIcon;
use tray_icon::Icon as TrayAppIcon;

pub fn make_tray_icon() -> TrayAppIcon {
    TrayAppIcon::from_rgba(make_tray_glyph_rgba(), 32, 32).expect("tray icon")
}

pub fn make_window_icon() -> TaoIcon {
    TaoIcon::from_rgba(make_icon_rgba(), 64, 64).expect("window icon")
}

/// A crisp bar-chart glyph for the menu bar: solid white bars on a transparent
/// background. Rendered as a macOS *template* icon (see `build_tray`), so the
/// system tints it for light/dark mode and it stays clearly visible either way.
fn make_tray_glyph_rgba() -> Vec<u8> {
    let s = 32usize;
    let mut rgba = vec![0u8; s * s * 4]; // fully transparent
    let white = [255, 255, 255, 255];
    // Four ascending bars, evenly spaced, with a baseline.
    let bars = [(4usize, 18usize), (11, 12), (18, 22), (25, 8)];
    for (x, bar_h) in bars {
        let top = 28 - bar_h;
        fill_rect(&mut rgba, s, x, top, 5, bar_h, white);
    }
    fill_rect(&mut rgba, s, 3, 28, 27, 3, white); // baseline
    rgba
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

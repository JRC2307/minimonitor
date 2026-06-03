#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("MiniMonitor currently targets macOS only.");
}

#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod inspector;
#[cfg(target_os = "macos")]
mod services;
#[cfg(target_os = "macos")]
mod tray;
#[cfg(target_os = "macos")]
mod util;

#[cfg(target_os = "macos")]
fn main() {
    app::run();
}

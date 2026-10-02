#![windows_subsystem = "windows"]
#![cfg_attr(not(windows), allow(dead_code))]

mod config;
#[cfg(windows)]
mod win;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    eprintln!("ペタッとは Windows 専用です");
}

// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#[cfg(not(target_os = "macos"))]
compile_error!("This scaffold currently targets macOS only.");

fn main() {
    tauri_app_lib::run()
}

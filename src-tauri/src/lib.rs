#![allow(dead_code)]

mod audio_capture_service;
mod history_store;
mod hotkey_service;
mod permission_service;
mod settings_store;
mod status_notifier;
mod text_insertion_service;
mod transcription;

use std::sync::Arc;
use std::sync::Mutex;

use audio_capture_service::AudioCaptureService;
use history_store::HistoryStore;
use hotkey_service::HotkeyService;
use permission_service::PermissionService;
use settings_store::SettingsStore;
use status_notifier::{AppStatus, StatusNotifier};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconEvent},
    AppHandle, Manager,
};
use text_insertion_service::TextInsertionService;
use transcription::openai::{OpenAiTranscriptionConfig, OpenAiTranscriptionProvider};
use transcription::{TranscriptionOptions, TranscriptionOrchestrator};

#[derive(Debug)]
struct AppServices {
    _hotkey_service: HotkeyService,
    _audio_capture_service: AudioCaptureService,
    transcription_orchestrator: TranscriptionOrchestrator,
    _text_insertion_service: TextInsertionService,
    _settings_store: SettingsStore,
    _history_store: HistoryStore,
    _permission_service: PermissionService,
}

impl Default for AppServices {
    fn default() -> Self {
        let provider = OpenAiTranscriptionProvider::new(OpenAiTranscriptionConfig::from_env());
        let transcription_orchestrator = TranscriptionOrchestrator::new(Arc::new(provider));

        Self {
            _hotkey_service: HotkeyService::new(),
            _audio_capture_service: AudioCaptureService::new(),
            transcription_orchestrator,
            _text_insertion_service: TextInsertionService::new(),
            _settings_store: SettingsStore::new(),
            _history_store: HistoryStore::new(),
            _permission_service: PermissionService::new(),
        }
    }
}

#[derive(Debug, Default)]
struct AppState {
    status_notifier: Mutex<StatusNotifier>,
    services: AppServices,
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> AppStatus {
    state
        .status_notifier
        .lock()
        .map(|notifier| notifier.current())
        .unwrap_or(AppStatus::Error)
}

#[tauri::command]
fn set_status(status: AppStatus, state: tauri::State<'_, AppState>) {
    if let Ok(mut notifier) = state.status_notifier.lock() {
        notifier.set(status);
    }
}

#[tauri::command]
async fn transcribe_audio(
    audio_bytes: Vec<u8>,
    options: Option<TranscriptionOptions>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    if let Ok(mut notifier) = state.status_notifier.lock() {
        notifier.set(AppStatus::Transcribing);
    }

    let result = state
        .services
        .transcription_orchestrator
        .transcribe(audio_bytes, options.unwrap_or_default())
        .await;

    match result {
        Ok(transcription) => {
            if let Ok(mut notifier) = state.status_notifier.lock() {
                notifier.set(AppStatus::Idle);
            }

            Ok(transcription.text)
        }
        Err(error) => {
            if let Ok(mut notifier) = state.status_notifier.lock() {
                notifier.set(AppStatus::Error);
            }

            Err(error.to_string())
        }
    }
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}

fn toggle_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        match window.is_visible() {
            Ok(true) => {
                let _ = window.hide();
            }
            _ => {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    }
}

fn handle_tray_menu_event(app: &AppHandle, menu_id: &str) {
    match menu_id {
        "show_window" => show_main_window(app),
        "hide_window" => hide_main_window(app),
        "quit" => app.exit(0),
        _ => {}
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let show_item =
                MenuItem::with_id(app, "show_window", "Open Voice", true, None::<&str>)?;
            let hide_item =
                MenuItem::with_id(app, "hide_window", "Hide Voice", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Voice", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

            tauri::tray::TrayIconBuilder::with_id("voice-tray")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_main_window(&tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| {
                    handle_tray_menu_event(app, event.id().as_ref());
                })
                .build(app)?;

            hide_main_window(app.handle());

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_status,
            transcribe_audio
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

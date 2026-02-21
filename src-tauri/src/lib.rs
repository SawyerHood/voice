#![allow(dead_code)]

mod api_key_store;
mod audio_capture_service;
mod history_store;
mod hotkey_service;
mod permission_service;
mod settings_store;
mod status_notifier;
mod text_insertion_service;
mod transcription;
mod voice_pipeline;

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use api_key_store::ApiKeyStore;
use async_trait::async_trait;
use audio_capture_service::{
    AudioCaptureService, AudioInputStreamErrorEvent, MicrophoneInfo, RecordedAudio,
    AUDIO_INPUT_STREAM_ERROR_EVENT,
};
use history_store::HistoryStore;
use hotkey_service::{HotkeyService, RecordingTransition};
use permission_service::PermissionService;
use serde::Serialize;
use settings_store::{SettingsStore, VoiceSettings, VoiceSettingsUpdate};
use status_notifier::{AppStatus, StatusNotifier};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconEvent},
    AppHandle, Emitter, Listener, Manager,
};
use text_insertion_service::TextInsertionService;
use transcription::openai::{OpenAiTranscriptionConfig, OpenAiTranscriptionProvider};
use transcription::{TranscriptionOptions, TranscriptionOrchestrator};
use voice_pipeline::{PipelineError, VoicePipeline, VoicePipelineDelegate};

const EVENT_STATUS_CHANGED: &str = "voice://status-changed";
const EVENT_TRANSCRIPT_READY: &str = "voice://transcript-ready";
const EVENT_PIPELINE_ERROR: &str = "voice://pipeline-error";
const AUDIO_STREAM_ERROR_RESET_DELAY_MS: u64 = 1_500;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TranscriptReadyEvent {
    text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PipelineErrorEvent {
    stage: String,
    message: String,
}

#[derive(Debug)]
struct AppServices {
    audio_capture_service: AudioCaptureService,
    transcription_orchestrator: TranscriptionOrchestrator,
    text_insertion_service: TextInsertionService,
    settings_store: SettingsStore,
    api_key_store: ApiKeyStore,
    _history_store: HistoryStore,
    _permission_service: PermissionService,
}

impl Default for AppServices {
    fn default() -> Self {
        let provider = OpenAiTranscriptionProvider::new(OpenAiTranscriptionConfig::from_env());
        let transcription_orchestrator = TranscriptionOrchestrator::new(Arc::new(provider));

        Self {
            audio_capture_service: AudioCaptureService::new(),
            transcription_orchestrator,
            text_insertion_service: TextInsertionService::new(),
            settings_store: SettingsStore::new(),
            api_key_store: ApiKeyStore::new(),
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

#[derive(Debug, Clone)]
struct PipelineRuntimeState {
    execution_lock: Arc<tokio::sync::Mutex<()>>,
    next_session_id: Arc<AtomicU64>,
    active_session_id: Arc<AtomicU64>,
}

impl Default for PipelineRuntimeState {
    fn default() -> Self {
        Self {
            execution_lock: Arc::new(tokio::sync::Mutex::new(())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            active_session_id: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl PipelineRuntimeState {
    fn begin_session(&self) -> u64 {
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_session_id.store(session_id, Ordering::Relaxed);
        session_id
    }

    fn is_session_active(&self, session_id: u64) -> bool {
        self.active_session_id.load(Ordering::Relaxed) == session_id
    }
}

#[derive(Clone)]
struct AppPipelineDelegate {
    app: AppHandle,
    session_id: Option<u64>,
}

impl AppPipelineDelegate {
    fn new(app: AppHandle) -> Self {
        Self {
            app,
            session_id: None,
        }
    }

    fn for_session(app: AppHandle, session_id: u64) -> Self {
        Self {
            app,
            session_id: Some(session_id),
        }
    }

    fn is_session_active(&self) -> bool {
        match self.session_id {
            Some(session_id) => self
                .app
                .state::<PipelineRuntimeState>()
                .is_session_active(session_id),
            None => true,
        }
    }
}

#[async_trait]
impl VoicePipelineDelegate for AppPipelineDelegate {
    fn set_status(&self, status: AppStatus) {
        if self.is_session_active() {
            set_status_for_app(&self.app, status);
        }
    }

    fn emit_transcript(&self, transcript: &str) {
        if self.is_session_active() {
            emit_transcript_event(&self.app, transcript);
        }
    }

    fn emit_error(&self, error: &PipelineError) {
        if self.is_session_active() {
            emit_pipeline_error_event(&self.app, error);
        }
    }

    fn on_recording_started(&self, success: bool) {
        let hotkey_service = self.app.state::<HotkeyService>();
        hotkey_service.acknowledge_transition(RecordingTransition::Started, success);
    }

    fn on_recording_stopped(&self, success: bool) {
        let hotkey_service = self.app.state::<HotkeyService>();
        hotkey_service.acknowledge_transition(RecordingTransition::Stopped, success);
    }

    fn start_recording(&self) -> Result<(), String> {
        let state = self.app.state::<AppState>();
        state
            .services
            .audio_capture_service
            .start_recording(self.app.clone(), None)
    }

    fn stop_recording(&self) -> Result<Vec<u8>, String> {
        let state = self.app.state::<AppState>();
        state
            .services
            .audio_capture_service
            .stop_recording(self.app.clone())
            .map(|recorded| recorded.wav_bytes)
    }

    async fn transcribe(&self, wav_bytes: Vec<u8>) -> Result<String, String> {
        let orchestrator = {
            let state = self.app.state::<AppState>();
            state.services.transcription_orchestrator.clone()
        };

        orchestrator
            .transcribe(wav_bytes, TranscriptionOptions::default())
            .await
            .map(|transcription| transcription.text)
            .map_err(|error| error.to_string())
    }

    fn insert_text(&self, transcript: &str) -> Result<(), String> {
        if !self.is_session_active() {
            return Ok(());
        }

        let state = self.app.state::<AppState>();
        state
            .services
            .text_insertion_service
            .insert_text(transcript)
    }
}

fn get_status_from_state(state: &AppState) -> AppStatus {
    state
        .status_notifier
        .lock()
        .map(|notifier| notifier.current())
        .unwrap_or(AppStatus::Error)
}

fn set_status_for_state(app: &AppHandle, state: &AppState, status: AppStatus) {
    if let Ok(mut notifier) = state.status_notifier.lock() {
        notifier.set(status);
    }

    let _ = app.emit(EVENT_STATUS_CHANGED, status);
}

fn set_status_for_app(app: &AppHandle, status: AppStatus) {
    let state = app.state::<AppState>();
    set_status_for_state(app, &state, status);
}

fn emit_transcript_event(app: &AppHandle, transcript: &str) {
    let payload = TranscriptReadyEvent {
        text: transcript.to_string(),
    };
    let _ = app.emit(EVENT_TRANSCRIPT_READY, payload);
}

fn emit_pipeline_error_event(app: &AppHandle, error: &PipelineError) {
    let payload = PipelineErrorEvent {
        stage: error.stage.as_str().to_string(),
        message: error.message.clone(),
    };

    let _ = app.emit(EVENT_PIPELINE_ERROR, payload);
}

fn parse_audio_stream_error_message(payload: &str) -> String {
    serde_json::from_str::<AudioInputStreamErrorEvent>(payload)
        .ok()
        .map(|event| event.message.trim().to_string())
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "Microphone stream failed unexpectedly".to_string())
}

fn handle_audio_input_stream_error(app: &AppHandle, message: String) {
    let runtime_state = app.state::<PipelineRuntimeState>();
    runtime_state.begin_session();

    let hotkey_service = app.state::<HotkeyService>();
    hotkey_service.force_stop_recording(app);

    let state = app.state::<AppState>();
    if let Err(error) = state
        .services
        .audio_capture_service
        .abort_recording(app.clone())
    {
        eprintln!("Failed to abort recording after stream error: {error}");
    }

    let pipeline_error = PipelineError {
        stage: voice_pipeline::PipelineErrorStage::RecordingRuntime,
        message,
    };
    emit_pipeline_error_event(app, &pipeline_error);
    set_status_for_state(app, &state, AppStatus::Error);

    let reset_app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(AUDIO_STREAM_ERROR_RESET_DELAY_MS)).await;
        let state = reset_app.state::<AppState>();
        if get_status_from_state(&state) == AppStatus::Error {
            set_status_for_state(&reset_app, &state, AppStatus::Idle);
        }
    });
}

fn register_pipeline_handlers(app: &AppHandle) {
    let start_app = app.clone();
    app.listen(hotkey_service::EVENT_RECORDING_STARTED, move |_| {
        let app = start_app.clone();
        let runtime_state = app.state::<PipelineRuntimeState>().inner().clone();
        tauri::async_runtime::spawn(async move {
            let _guard = runtime_state.execution_lock.lock().await;
            let session_id = runtime_state.begin_session();
            let delegate = AppPipelineDelegate::for_session(app.clone(), session_id);
            VoicePipeline::default()
                .handle_hotkey_started(&delegate)
                .await;
        });
    });

    let stop_app = app.clone();
    app.listen(hotkey_service::EVENT_RECORDING_STOPPED, move |_| {
        let app = stop_app.clone();
        let runtime_state = app.state::<PipelineRuntimeState>().inner().clone();
        tauri::async_runtime::spawn(async move {
            let _guard = runtime_state.execution_lock.lock().await;
            let session_id = runtime_state.begin_session();
            let delegate = AppPipelineDelegate::for_session(app.clone(), session_id);
            let hotkey_service = app.state::<HotkeyService>();

            if !hotkey_service.is_recording() {
                hotkey_service.acknowledge_transition(RecordingTransition::Stopped, false);
                return;
            }

            VoicePipeline::default()
                .handle_hotkey_stopped(&delegate)
                .await;
        });
    });

    let stream_error_app = app.clone();
    app.listen(AUDIO_INPUT_STREAM_ERROR_EVENT, move |event| {
        let message = parse_audio_stream_error_message(event.payload());
        handle_audio_input_stream_error(&stream_error_app, message);
    });
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> AppStatus {
    get_status_from_state(&state)
}

#[tauri::command]
fn set_status(app: AppHandle, status: AppStatus, state: tauri::State<'_, AppState>) {
    set_status_for_state(&app, &state, status);
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> VoiceSettings {
    state.services.settings_store.current()
}

#[tauri::command]
fn update_settings(
    app: AppHandle,
    update: VoiceSettingsUpdate,
    state: tauri::State<'_, AppState>,
) -> Result<VoiceSettings, String> {
    state.services.settings_store.update(&app, update)
}

#[tauri::command]
fn get_api_key(
    provider: String,
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    state.services.api_key_store.get_api_key(provider.as_str())
}

#[tauri::command]
fn set_api_key(
    provider: String,
    key: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    state
        .services
        .api_key_store
        .set_api_key(provider.as_str(), key.as_str())
}

#[tauri::command]
fn delete_api_key(provider: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state
        .services
        .api_key_store
        .delete_api_key(provider.as_str())
}

#[tauri::command]
fn list_microphones(state: tauri::State<'_, AppState>) -> Result<Vec<MicrophoneInfo>, String> {
    state.services.audio_capture_service.list_microphones()
}

#[tauri::command]
fn start_recording(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    microphone_id: Option<String>,
) -> Result<(), String> {
    let result = state
        .services
        .audio_capture_service
        .start_recording(app.clone(), microphone_id.as_deref());

    if result.is_ok() {
        set_status_for_state(&app, &state, AppStatus::Listening);
    }

    result
}

#[tauri::command]
fn stop_recording(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<RecordedAudio, String> {
    let recorded = state
        .services
        .audio_capture_service
        .stop_recording(app.clone())?;

    set_status_for_state(&app, &state, AppStatus::Idle);
    Ok(recorded)
}

#[tauri::command]
fn get_audio_level(state: tauri::State<'_, AppState>) -> f32 {
    state.services.audio_capture_service.get_audio_level()
}

#[tauri::command]
fn insert_text(text: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.services.text_insertion_service.insert_text(&text)
}

#[tauri::command]
fn copy_to_clipboard(text: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    state
        .services
        .text_insertion_service
        .copy_to_clipboard(&text)
}

#[tauri::command]
async fn transcribe_audio(
    app: AppHandle,
    audio_bytes: Vec<u8>,
    options: Option<TranscriptionOptions>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    set_status_for_state(&app, &state, AppStatus::Transcribing);

    let result = state
        .services
        .transcription_orchestrator
        .transcribe(audio_bytes, options.unwrap_or_default())
        .await;

    match result {
        Ok(transcription) => {
            set_status_for_state(&app, &state, AppStatus::Idle);

            Ok(transcription.text)
        }
        Err(error) => {
            let message = error.to_string();
            let delegate = AppPipelineDelegate::new(app.clone());
            let pipeline_message = message.clone();

            tauri::async_runtime::spawn(async move {
                VoicePipeline::default()
                    .handle_stage_error(
                        &delegate,
                        voice_pipeline::PipelineErrorStage::Transcription,
                        pipeline_message,
                    )
                    .await;
            });

            Err(message)
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
        .manage(HotkeyService::new())
        .manage(PipelineRuntimeState::default())
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            app.handle()
                .plugin(tauri_plugin_global_shortcut::Builder::new().build())?;

            let hotkey_service = app.state::<HotkeyService>();
            hotkey_service
                .register_default_shortcut(app.handle())
                .map_err(std::io::Error::other)?;

            let app_state = app.state::<AppState>();
            if let Err(error) = app_state.services.settings_store.load(app.handle()) {
                eprintln!("Failed to load persisted settings: {error}");
            }

            register_pipeline_handlers(app.handle());
            set_status_for_app(app.handle(), AppStatus::Idle);

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
            get_settings,
            update_settings,
            get_api_key,
            set_api_key,
            delete_api_key,
            list_microphones,
            start_recording,
            stop_recording,
            get_audio_level,
            insert_text,
            copy_to_clipboard,
            transcribe_audio,
            hotkey_service::get_hotkey_config,
            hotkey_service::get_hotkey_recording_state,
            hotkey_service::set_hotkey_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::PipelineRuntimeState;

    #[test]
    fn later_session_invalidates_previous_session() {
        let runtime = PipelineRuntimeState::default();

        let first = runtime.begin_session();
        let second = runtime.begin_session();

        assert!(!runtime.is_session_active(first));
        assert!(runtime.is_session_active(second));
    }
}

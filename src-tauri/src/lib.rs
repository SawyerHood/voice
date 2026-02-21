#![allow(dead_code)]

mod api_key_store;
mod audio_capture_service;
mod history_store;
mod hotkey_service;
mod logging;
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
use history_store::{HistoryEntry, HistoryStore};
use hotkey_service::{HotkeyService, RecordingTransition};
use logging::LoggingState;
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
use tracing::{debug, error, info, warn};
use transcription::openai::{OpenAiTranscriptionConfig, OpenAiTranscriptionProvider};
use transcription::{TranscriptionOptions, TranscriptionOrchestrator};
use voice_pipeline::{PipelineError, PipelineTranscript, VoicePipeline, VoicePipelineDelegate};

const EVENT_STATUS_CHANGED: &str = "voice://status-changed";
const EVENT_TRANSCRIPT_READY: &str = "voice://transcript-ready";
const EVENT_PIPELINE_ERROR: &str = "voice://pipeline-error";
const AUDIO_STREAM_ERROR_RESET_DELAY_MS: u64 = 1_500;
const DEFAULT_HISTORY_PAGE_SIZE: usize = 50;

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
    permission_service: PermissionService,
}

impl Default for AppServices {
    fn default() -> Self {
        let provider = OpenAiTranscriptionProvider::new(OpenAiTranscriptionConfig::from_env());
        let transcription_orchestrator = TranscriptionOrchestrator::new(Arc::new(provider));
        info!("initializing app services");

        Self {
            audio_capture_service: AudioCaptureService::new(),
            transcription_orchestrator,
            text_insertion_service: TextInsertionService::new(),
            settings_store: SettingsStore::new(),
            api_key_store: ApiKeyStore::new(),
            permission_service: PermissionService::new(),
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
        debug!(session_id, "pipeline session started");
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
            debug!(?status, session_id = ?self.session_id, "updating app status");
            set_status_for_app(&self.app, status);
        } else {
            debug!(
                ?status,
                session_id = ?self.session_id,
                "ignoring status update for inactive session"
            );
        }
    }

    fn emit_transcript(&self, transcript: &str) {
        if self.is_session_active() {
            info!(
                session_id = ?self.session_id,
                transcript_chars = transcript.chars().count(),
                "pipeline transcript ready"
            );
            emit_transcript_event(&self.app, transcript);
        } else {
            debug!(
                session_id = ?self.session_id,
                "ignoring transcript for inactive session"
            );
        }
    }

    fn emit_error(&self, error: &PipelineError) {
        if self.is_session_active() {
            error!(
                session_id = ?self.session_id,
                stage = error.stage.as_str(),
                message = %error.message,
                "pipeline error emitted"
            );
            emit_pipeline_error_event(&self.app, error);
        } else {
            debug!(
                session_id = ?self.session_id,
                stage = error.stage.as_str(),
                "ignoring pipeline error for inactive session"
            );
        }
    }

    fn on_recording_started(&self, success: bool) {
        debug!(session_id = ?self.session_id, success, "recording start acknowledged");
        let hotkey_service = self.app.state::<HotkeyService>();
        hotkey_service.acknowledge_transition(RecordingTransition::Started, success);
    }

    fn on_recording_stopped(&self, success: bool) {
        debug!(session_id = ?self.session_id, success, "recording stop acknowledged");
        let hotkey_service = self.app.state::<HotkeyService>();
        hotkey_service.acknowledge_transition(RecordingTransition::Stopped, success);
    }

    fn start_recording(&self) -> Result<(), String> {
        info!(session_id = ?self.session_id, "pipeline requested recording start");
        let state = self.app.state::<AppState>();
        state
            .services
            .audio_capture_service
            .start_recording(self.app.clone(), None)
    }

    fn stop_recording(&self) -> Result<Vec<u8>, String> {
        info!(session_id = ?self.session_id, "pipeline requested recording stop");
        let state = self.app.state::<AppState>();
        state
            .services
            .audio_capture_service
            .stop_recording(self.app.clone())
            .map(|recorded| recorded.wav_bytes)
    }

    async fn transcribe(&self, wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
        let orchestrator = {
            let state = self.app.state::<AppState>();
            state.services.transcription_orchestrator.clone()
        };
        let provider_name = orchestrator.active_provider_name().to_string();
        let provider_name_for_error = provider_name.clone();
        info!(
            session_id = ?self.session_id,
            provider = %provider_name,
            audio_bytes = wav_bytes.len(),
            "starting transcription request"
        );

        orchestrator
            .transcribe(wav_bytes, TranscriptionOptions::default())
            .await
            .map(|transcription| PipelineTranscript {
                text: transcription.text,
                duration_secs: transcription.duration_secs,
                language: transcription.language,
                provider: provider_name.clone(),
            })
            .map(|transcript| {
                info!(
                    session_id = ?self.session_id,
                    provider = %transcript.provider,
                    transcript_chars = transcript.text.chars().count(),
                    "transcription request completed"
                );
                transcript
            })
            .map_err(|error| {
                error!(
                    session_id = ?self.session_id,
                    provider = %provider_name_for_error,
                    error = %error,
                    "transcription request failed"
                );
                error.to_string()
            })
    }

    fn insert_text(&self, transcript: &str) -> Result<(), String> {
        if !self.is_session_active() {
            warn!(
                session_id = ?self.session_id,
                "skipping text insertion for inactive session"
            );
            return Ok(());
        }

        info!(
            session_id = ?self.session_id,
            transcript_chars = transcript.chars().count(),
            "inserting transcript text"
        );
        let state = self.app.state::<AppState>();
        state
            .services
            .text_insertion_service
            .insert_text(transcript)
    }

    fn save_history_entry(&self, transcript: &PipelineTranscript) -> Result<(), String> {
        if !self.is_session_active() {
            warn!(
                session_id = ?self.session_id,
                "skipping history persistence for inactive session"
            );
            return Ok(());
        }

        let history_store = self.app.state::<HistoryStore>();
        let entry = HistoryEntry::new(
            transcript.text.clone(),
            transcript.duration_secs,
            transcript.language.clone(),
            transcript.provider.clone(),
        );
        debug!(
            session_id = ?self.session_id,
            provider = %entry.provider,
            transcript_chars = entry.text.chars().count(),
            "persisting transcript history entry"
        );

        history_store.add_entry(entry)
    }
}

fn get_status_from_state(state: &AppState) -> AppStatus {
    state
        .status_notifier
        .lock()
        .map(|notifier| notifier.current())
        .unwrap_or_else(|_| {
            error!("status notifier lock poisoned while reading status");
            AppStatus::Error
        })
}

fn set_status_for_state(app: &AppHandle, state: &AppState, status: AppStatus) {
    if let Ok(mut notifier) = state.status_notifier.lock() {
        notifier.set(status);
    } else {
        error!("status notifier lock poisoned while setting status");
    }

    if let Err(error) = app.emit(EVENT_STATUS_CHANGED, status) {
        warn!(?status, %error, "failed to emit status changed event");
    }
}

fn set_status_for_app(app: &AppHandle, status: AppStatus) {
    let state = app.state::<AppState>();
    set_status_for_state(app, &state, status);
}

fn emit_transcript_event(app: &AppHandle, transcript: &str) {
    let payload = TranscriptReadyEvent {
        text: transcript.to_string(),
    };
    if let Err(error) = app.emit(EVENT_TRANSCRIPT_READY, payload) {
        warn!(%error, "failed to emit transcript ready event");
    }
}

fn emit_pipeline_error_event(app: &AppHandle, error: &PipelineError) {
    let payload = PipelineErrorEvent {
        stage: error.stage.as_str().to_string(),
        message: error.message.clone(),
    };

    if let Err(emit_error) = app.emit(EVENT_PIPELINE_ERROR, payload) {
        warn!(
            stage = error.stage.as_str(),
            event_error = %emit_error,
            "failed to emit pipeline error event"
        );
    }
}

fn parse_audio_stream_error_message(payload: &str) -> String {
    serde_json::from_str::<AudioInputStreamErrorEvent>(payload)
        .ok()
        .map(|event| event.message.trim().to_string())
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            warn!("audio stream error payload was invalid; using fallback message");
            "Microphone stream failed unexpectedly".to_string()
        })
}

fn handle_audio_input_stream_error_with_hooks<
    BeginSession,
    ForceStopRecording,
    AbortRecording,
    EmitPipelineError,
    SetStatus,
    ScheduleReset,
>(
    message: String,
    mut begin_session: BeginSession,
    mut force_stop_recording: ForceStopRecording,
    mut abort_recording: AbortRecording,
    mut emit_pipeline_error: EmitPipelineError,
    mut set_status: SetStatus,
    schedule_reset: ScheduleReset,
) where
    BeginSession: FnMut(),
    ForceStopRecording: FnMut(),
    AbortRecording: FnMut() -> Result<(), String>,
    EmitPipelineError: FnMut(&PipelineError),
    SetStatus: FnMut(AppStatus),
    ScheduleReset: FnOnce(),
{
    error!(message = %message, "handling runtime audio stream error");
    begin_session();
    force_stop_recording();

    if let Err(error) = abort_recording() {
        error!(%error, "failed to abort recording after stream error");
    }

    let pipeline_error = PipelineError {
        stage: voice_pipeline::PipelineErrorStage::RecordingRuntime,
        message,
    };
    emit_pipeline_error(&pipeline_error);
    set_status(AppStatus::Error);
    schedule_reset();
}

fn handle_audio_input_stream_error(app: &AppHandle, message: String) {
    error!(message = %message, "audio stream error received by app");
    let reset_app = app.clone();
    handle_audio_input_stream_error_with_hooks(
        message,
        || {
            let runtime_state = app.state::<PipelineRuntimeState>();
            runtime_state.begin_session();
        },
        || {
            let hotkey_service = app.state::<HotkeyService>();
            hotkey_service.force_stop_recording(app);
        },
        || {
            let state = app.state::<AppState>();
            state
                .services
                .audio_capture_service
                .abort_recording(app.clone())
                .map(|_| ())
        },
        |error| emit_pipeline_error_event(app, error),
        |status| {
            let state = app.state::<AppState>();
            set_status_for_state(app, &state, status);
        },
        move || {
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_millis(AUDIO_STREAM_ERROR_RESET_DELAY_MS)).await;
                let state = reset_app.state::<AppState>();
                if get_status_from_state(&state) == AppStatus::Error {
                    info!(
                        delay_ms = AUDIO_STREAM_ERROR_RESET_DELAY_MS,
                        "resetting status to idle after stream error"
                    );
                    set_status_for_state(&reset_app, &state, AppStatus::Idle);
                }
            });
        },
    );
}

fn spawn_pipeline_stage_error_reset<D>(
    pipeline: VoicePipeline,
    delegate: D,
    stage: voice_pipeline::PipelineErrorStage,
    message: String,
) -> tauri::async_runtime::JoinHandle<()>
where
    D: VoicePipelineDelegate + Send + Sync + 'static,
{
    error!(stage = stage.as_str(), message = %message, "scheduling stage error reset");
    tauri::async_runtime::spawn(async move {
        pipeline.handle_stage_error(&delegate, stage, message).await;
    })
}

fn register_pipeline_handlers(app: &AppHandle) {
    info!("registering pipeline event handlers");
    let start_app = app.clone();
    app.listen(hotkey_service::EVENT_RECORDING_STARTED, move |event| {
        debug!(
            event_id = event.id(),
            "received recording started hotkey event"
        );
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
    app.listen(hotkey_service::EVENT_RECORDING_STOPPED, move |event| {
        debug!(
            event_id = event.id(),
            "received recording stopped hotkey event"
        );
        let app = stop_app.clone();
        let runtime_state = app.state::<PipelineRuntimeState>().inner().clone();
        tauri::async_runtime::spawn(async move {
            let _guard = runtime_state.execution_lock.lock().await;
            let session_id = runtime_state.begin_session();
            let delegate = AppPipelineDelegate::for_session(app.clone(), session_id);
            let hotkey_service = app.state::<HotkeyService>();

            if !hotkey_service.is_recording() {
                warn!(
                    session_id,
                    "received stop event while hotkey service was not recording"
                );
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
        error!(
            event_id = event.id(),
            message = %message,
            "received audio input stream error event"
        );
        handle_audio_input_stream_error(&stream_error_app, message);
    });
}

#[tauri::command]
fn get_status(state: tauri::State<'_, AppState>) -> AppStatus {
    let status = get_status_from_state(&state);
    debug!(?status, "status requested");
    status
}

#[tauri::command]
fn set_status(app: AppHandle, status: AppStatus, state: tauri::State<'_, AppState>) {
    info!(?status, "status set requested");
    set_status_for_state(&app, &state, status);
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> VoiceSettings {
    let settings = state.services.settings_store.current();
    debug!("settings requested");
    settings
}

#[tauri::command]
fn update_settings(
    app: AppHandle,
    update: VoiceSettingsUpdate,
    state: tauri::State<'_, AppState>,
) -> Result<VoiceSettings, String> {
    info!("settings update requested");
    let updated = state.services.settings_store.update(&app, update);
    match &updated {
        Ok(settings) => {
            info!(
                recording_mode = %settings.recording_mode,
                auto_insert = settings.auto_insert,
                "settings updated"
            );
        }
        Err(error) => {
            error!(%error, "settings update failed");
        }
    }
    updated
}

#[tauri::command]
fn get_api_key(
    provider: String,
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    debug!(provider = %provider, "api key lookup requested");
    let result = state.services.api_key_store.get_api_key(provider.as_str());
    match &result {
        Ok(Some(_)) => debug!(provider = %provider, "api key lookup returned stored key"),
        Ok(None) => debug!(provider = %provider, "api key lookup returned no key"),
        Err(error) => error!(provider = %provider, %error, "api key lookup failed"),
    }
    result
}

#[tauri::command]
fn set_api_key(
    provider: String,
    key: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    info!(provider = %provider, "api key set requested");
    let result = state
        .services
        .api_key_store
        .set_api_key(provider.as_str(), key.as_str());
    if let Err(error) = &result {
        error!(provider = %provider, %error, "api key set failed");
    }
    result
}

#[tauri::command]
fn delete_api_key(provider: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!(provider = %provider, "api key delete requested");
    let result = state
        .services
        .api_key_store
        .delete_api_key(provider.as_str());
    if let Err(error) = &result {
        error!(provider = %provider, %error, "api key delete failed");
    }
    result
}

#[tauri::command]
fn list_microphones(state: tauri::State<'_, AppState>) -> Result<Vec<MicrophoneInfo>, String> {
    let result = state.services.audio_capture_service.list_microphones();
    match &result {
        Ok(microphones) => debug!(count = microphones.len(), "listed microphones"),
        Err(error) => error!(%error, "failed to list microphones"),
    }
    result
}

#[tauri::command]
fn start_recording(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    microphone_id: Option<String>,
) -> Result<(), String> {
    info!(
        microphone_id = ?microphone_id.as_deref(),
        "manual recording start requested"
    );
    let result = state
        .services
        .audio_capture_service
        .start_recording(app.clone(), microphone_id.as_deref());

    if result.is_ok() {
        set_status_for_state(&app, &state, AppStatus::Listening);
        info!("manual recording started");
    } else if let Err(error) = &result {
        error!(%error, "manual recording start failed");
    }

    result
}

#[tauri::command]
fn stop_recording(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<RecordedAudio, String> {
    info!("manual recording stop requested");
    let recorded = state
        .services
        .audio_capture_service
        .stop_recording(app.clone())
        .map_err(|error| {
            error!(%error, "manual recording stop failed");
            error
        })?;

    set_status_for_state(&app, &state, AppStatus::Idle);
    info!(
        duration_ms = recorded.duration_ms,
        sample_rate_hz = recorded.sample_rate_hz,
        channels = recorded.channels,
        "manual recording stopped"
    );
    Ok(recorded)
}

#[tauri::command]
fn get_audio_level(state: tauri::State<'_, AppState>) -> f32 {
    state.services.audio_capture_service.get_audio_level()
}

#[tauri::command]
fn insert_text(text: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!(
        chars = text.chars().count(),
        "manual text insertion requested"
    );
    state.services.text_insertion_service.insert_text(&text)
}

#[tauri::command]
fn copy_to_clipboard(text: String, state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!(
        chars = text.chars().count(),
        "manual clipboard copy requested"
    );
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
    info!(
        audio_bytes = audio_bytes.len(),
        "command transcription requested"
    );
    set_status_for_state(&app, &state, AppStatus::Transcribing);

    let result = state
        .services
        .transcription_orchestrator
        .transcribe(audio_bytes, options.unwrap_or_default())
        .await;

    match result {
        Ok(transcription) => {
            set_status_for_state(&app, &state, AppStatus::Idle);
            info!(
                transcript_chars = transcription.text.chars().count(),
                language = ?transcription.language,
                "command transcription completed"
            );

            Ok(transcription.text)
        }
        Err(error) => {
            let message = error.to_string();
            error!(%message, "command transcription failed");
            let delegate = AppPipelineDelegate::new(app.clone());
            let _ = spawn_pipeline_stage_error_reset(
                VoicePipeline::default(),
                delegate,
                voice_pipeline::PipelineErrorStage::Transcription,
                message.clone(),
            );

            Err(message)
        }
    }
}

#[tauri::command]
fn list_history(
    history_store: tauri::State<'_, HistoryStore>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<HistoryEntry>, String> {
    let page_limit = limit.unwrap_or(DEFAULT_HISTORY_PAGE_SIZE);
    let page_offset = offset.unwrap_or(0);
    debug!(
        limit = page_limit,
        offset = page_offset,
        "history list requested"
    );
    history_store.list_entries(page_limit, page_offset)
}

#[tauri::command]
fn get_history_entry(
    history_store: tauri::State<'_, HistoryStore>,
    id: String,
) -> Result<Option<HistoryEntry>, String> {
    debug!(id = %id, "history lookup requested");
    history_store.get_entry(&id)
}

#[tauri::command]
fn delete_history_entry(
    history_store: tauri::State<'_, HistoryStore>,
    id: String,
) -> Result<bool, String> {
    info!(id = %id, "history delete requested");
    history_store.delete_entry(&id)
}

#[tauri::command]
fn clear_history(history_store: tauri::State<'_, HistoryStore>) -> Result<(), String> {
    info!("history clear requested");
    history_store.clear_history()
}

#[tauri::command]
fn export_logs(log_state: tauri::State<'_, LoggingState>) -> Result<String, String> {
    info!(
        log_file = %log_state.log_file_path().display(),
        "diagnostic log export requested"
    );
    logging::export_log_contents(&log_state)
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        info!("showing main window");
        if let Err(error) = window.show() {
            warn!(%error, "failed to show main window");
        }
        if let Err(error) = window.set_focus() {
            warn!(%error, "failed to focus main window");
        }
    } else {
        warn!("main window was not found while attempting to show");
    }
}

fn hide_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        info!("hiding main window");
        if let Err(error) = window.hide() {
            warn!(%error, "failed to hide main window");
        }
    } else {
        warn!("main window was not found while attempting to hide");
    }
}

fn toggle_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        match window.is_visible() {
            Ok(true) => {
                info!("toggling main window to hidden");
                if let Err(error) = window.hide() {
                    warn!(%error, "failed to hide main window while toggling");
                }
            }
            _ => {
                info!("toggling main window to visible");
                if let Err(error) = window.show() {
                    warn!(%error, "failed to show main window while toggling");
                }
                if let Err(error) = window.set_focus() {
                    warn!(%error, "failed to focus main window while toggling");
                }
            }
        }
    } else {
        warn!("main window was not found while toggling visibility");
    }
}

fn handle_tray_menu_event(app: &AppHandle, menu_id: &str) {
    info!(menu_id, "tray menu event received");
    match menu_id {
        "show_window" => show_main_window(app),
        "hide_window" => hide_main_window(app),
        "quit" => {
            info!("quitting app from tray menu");
            app.exit(0);
        }
        _ => warn!(menu_id, "unknown tray menu event"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    info!("starting tauri app builder");
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::default())
        .manage(HotkeyService::new())
        .manage(PipelineRuntimeState::default())
        .setup(|app| {
            let logging_state = logging::initialize(app.handle()).map_err(std::io::Error::other)?;
            app.manage(logging_state);

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            info!("setup started");

            let history_store = HistoryStore::new(app.handle()).map_err(std::io::Error::other)?;
            app.manage(history_store);
            info!("history store initialized");

            app.handle()
                .plugin(tauri_plugin_global_shortcut::Builder::new().build())?;
            info!("global shortcut plugin initialized");

            let hotkey_service = app.state::<HotkeyService>();
            hotkey_service
                .register_default_shortcut(app.handle())
                .map_err(std::io::Error::other)?;
            info!("default hotkey registered");

            let app_state = app.state::<AppState>();
            let permission_state = app_state
                .services
                .permission_service
                .microphone_permission();
            info!(?permission_state, "microphone permission check completed");
            if let Err(error) = app_state.services.settings_store.load(app.handle()) {
                warn!(%error, "failed to load persisted settings");
            } else {
                info!("persisted settings loaded");
            }

            register_pipeline_handlers(app.handle());
            set_status_for_app(app.handle(), AppStatus::Idle);
            info!("pipeline handlers registered and status initialized");

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
            info!("tray icon initialized");

            hide_main_window(app.handle());
            info!("setup complete");

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                info!("main window close requested; hiding instead");
                if let Err(error) = window.hide() {
                    warn!(%error, "failed to hide main window on close request");
                }
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
            list_history,
            get_history_entry,
            delete_history_entry,
            clear_history,
            export_logs,
            hotkey_service::get_hotkey_config,
            hotkey_service::get_hotkey_recording_state,
            hotkey_service::set_hotkey_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::sync::{oneshot, Notify};

    use crate::{
        status_notifier::AppStatus,
        voice_pipeline::{
            PipelineError, PipelineErrorStage, PipelineTranscript, VoicePipeline,
            VoicePipelineDelegate,
        },
    };

    use super::{
        handle_audio_input_stream_error_with_hooks, spawn_pipeline_stage_error_reset,
        PipelineRuntimeState,
    };

    #[derive(Debug, Default)]
    struct SessionEventLog {
        statuses: Mutex<Vec<(u64, AppStatus)>>,
        transcripts: Mutex<Vec<(u64, String)>>,
        insertions: Mutex<Vec<(u64, String)>>,
        errors: Mutex<Vec<(u64, PipelineError)>>,
    }

    impl SessionEventLog {
        fn statuses_for(&self, session_id: u64) -> Vec<AppStatus> {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .iter()
                .filter_map(|(id, status)| (*id == session_id).then_some(*status))
                .collect()
        }

        fn transcripts(&self) -> Vec<(u64, String)> {
            self.transcripts
                .lock()
                .expect("transcript lock should not be poisoned")
                .clone()
        }

        fn insertions(&self) -> Vec<(u64, String)> {
            self.insertions
                .lock()
                .expect("insertion lock should not be poisoned")
                .clone()
        }

        fn errors(&self) -> Vec<(u64, PipelineError)> {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .clone()
        }
    }

    #[derive(Debug)]
    struct SessionAwareDelegate {
        runtime: PipelineRuntimeState,
        session_id: u64,
        event_log: Arc<SessionEventLog>,
        transcript: String,
        transcribe_started_tx: Mutex<Option<oneshot::Sender<()>>>,
        transcribe_blocker: Option<Arc<Notify>>,
    }

    impl SessionAwareDelegate {
        fn new(
            runtime: PipelineRuntimeState,
            session_id: u64,
            event_log: Arc<SessionEventLog>,
            transcript: &str,
        ) -> Self {
            Self {
                runtime,
                session_id,
                event_log,
                transcript: transcript.to_string(),
                transcribe_started_tx: Mutex::new(None),
                transcribe_blocker: None,
            }
        }

        fn with_transcription_gate(
            mut self,
            started_tx: oneshot::Sender<()>,
            blocker: Arc<Notify>,
        ) -> Self {
            self.transcribe_started_tx = Mutex::new(Some(started_tx));
            self.transcribe_blocker = Some(blocker);
            self
        }

        fn is_active(&self) -> bool {
            self.runtime.is_session_active(self.session_id)
        }
    }

    #[async_trait]
    impl VoicePipelineDelegate for SessionAwareDelegate {
        fn set_status(&self, status: AppStatus) {
            if self.is_active() {
                self.event_log
                    .statuses
                    .lock()
                    .expect("status lock should not be poisoned")
                    .push((self.session_id, status));
            }
        }

        fn emit_transcript(&self, transcript: &str) {
            if self.is_active() {
                self.event_log
                    .transcripts
                    .lock()
                    .expect("transcript lock should not be poisoned")
                    .push((self.session_id, transcript.to_string()));
            }
        }

        fn emit_error(&self, error: &PipelineError) {
            if self.is_active() {
                self.event_log
                    .errors
                    .lock()
                    .expect("error lock should not be poisoned")
                    .push((self.session_id, error.clone()));
            }
        }

        fn start_recording(&self) -> Result<(), String> {
            Ok(())
        }

        fn stop_recording(&self) -> Result<Vec<u8>, String> {
            Ok(vec![1, 2, 3])
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
            if let Some(started_tx) = self
                .transcribe_started_tx
                .lock()
                .expect("transcription gate lock should not be poisoned")
                .take()
            {
                let _ = started_tx.send(());
            }

            if let Some(blocker) = &self.transcribe_blocker {
                blocker.notified().await;
            }

            Ok(PipelineTranscript {
                text: self.transcript.clone(),
                duration_secs: None,
                language: None,
                provider: "test".to_string(),
            })
        }

        fn insert_text(&self, transcript: &str) -> Result<(), String> {
            if self.is_active() {
                self.event_log
                    .insertions
                    .lock()
                    .expect("insertion lock should not be poisoned")
                    .push((self.session_id, transcript.to_string()));
            }

            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct TranscriptionFailureDelegate {
        statuses: Mutex<Vec<AppStatus>>,
        errors: Mutex<Vec<PipelineError>>,
        transcripts: Mutex<Vec<String>>,
        insertions: Mutex<Vec<String>>,
    }

    impl TranscriptionFailureDelegate {
        fn statuses(&self) -> Vec<AppStatus> {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone()
        }

        fn errors(&self) -> Vec<PipelineError> {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .clone()
        }

        fn transcripts(&self) -> Vec<String> {
            self.transcripts
                .lock()
                .expect("transcript lock should not be poisoned")
                .clone()
        }

        fn insertions(&self) -> Vec<String> {
            self.insertions
                .lock()
                .expect("insertion lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl VoicePipelineDelegate for TranscriptionFailureDelegate {
        fn set_status(&self, status: AppStatus) {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .push(status);
        }

        fn emit_transcript(&self, transcript: &str) {
            self.transcripts
                .lock()
                .expect("transcript lock should not be poisoned")
                .push(transcript.to_string());
        }

        fn emit_error(&self, error: &PipelineError) {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .push(error.clone());
        }

        fn start_recording(&self) -> Result<(), String> {
            Ok(())
        }

        fn stop_recording(&self) -> Result<Vec<u8>, String> {
            Ok(vec![4, 5, 6])
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
            Err("provider unavailable".to_string())
        }

        fn insert_text(&self, transcript: &str) -> Result<(), String> {
            self.insertions
                .lock()
                .expect("insertion lock should not be poisoned")
                .push(transcript.to_string());
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    struct SharedStageErrorDelegate {
        statuses: Arc<Mutex<Vec<AppStatus>>>,
        errors: Arc<Mutex<Vec<PipelineError>>>,
    }

    impl SharedStageErrorDelegate {
        fn statuses(&self) -> Vec<AppStatus> {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone()
        }

        fn errors(&self) -> Vec<PipelineError> {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl VoicePipelineDelegate for SharedStageErrorDelegate {
        fn set_status(&self, status: AppStatus) {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .push(status);
        }

        fn emit_transcript(&self, _transcript: &str) {}

        fn emit_error(&self, error: &PipelineError) {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .push(error.clone());
        }

        fn start_recording(&self) -> Result<(), String> {
            Ok(())
        }

        fn stop_recording(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
            Ok(PipelineTranscript {
                text: String::new(),
                duration_secs: None,
                language: None,
                provider: "test".to_string(),
            })
        }

        fn insert_text(&self, _transcript: &str) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn later_session_invalidates_previous_session() {
        let runtime = PipelineRuntimeState::default();

        let first = runtime.begin_session();
        let second = runtime.begin_session();

        assert!(!runtime.is_session_active(first));
        assert!(runtime.is_session_active(second));
    }

    #[tokio::test]
    async fn overlapping_pipeline_sessions_ignore_stale_mutations() {
        let runtime = PipelineRuntimeState::default();
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let event_log = Arc::new(SessionEventLog::default());

        let first_session_id = runtime.begin_session();
        let (first_started_tx, first_started_rx) = oneshot::channel();
        let first_blocker = Arc::new(Notify::new());
        let first_delegate = SessionAwareDelegate::new(
            runtime.clone(),
            first_session_id,
            Arc::clone(&event_log),
            "first transcript",
        )
        .with_transcription_gate(first_started_tx, Arc::clone(&first_blocker));

        let first_task = {
            let pipeline = pipeline.clone();
            tokio::spawn(async move {
                pipeline.handle_hotkey_stopped(&first_delegate).await;
            })
        };

        first_started_rx
            .await
            .expect("first pipeline should reach transcription");

        let second_session_id = runtime.begin_session();
        let second_delegate = SessionAwareDelegate::new(
            runtime.clone(),
            second_session_id,
            Arc::clone(&event_log),
            "second transcript",
        );

        pipeline.handle_hotkey_stopped(&second_delegate).await;
        first_blocker.notify_waiters();
        first_task
            .await
            .expect("first pipeline task should finish cleanly");

        assert_eq!(
            event_log.statuses_for(first_session_id),
            vec![AppStatus::Transcribing]
        );
        assert_eq!(
            event_log.statuses_for(second_session_id),
            vec![AppStatus::Transcribing, AppStatus::Idle]
        );
        assert_eq!(
            event_log.transcripts(),
            vec![(second_session_id, "second transcript".to_string())]
        );
        assert_eq!(
            event_log.insertions(),
            vec![(second_session_id, "second transcript".to_string())]
        );
        assert!(event_log.errors().is_empty());
    }

    #[tokio::test]
    async fn transcription_failure_emits_error_resets_idle_and_skips_insertion() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = TranscriptionFailureDelegate::default();

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::Transcription,
                message: "provider unavailable".to_string(),
            }]
        );
        assert!(delegate.transcripts().is_empty());
        assert!(delegate.insertions().is_empty());
    }

    #[test]
    fn stream_error_propagation_aborts_recording_emits_error_and_resets_status() {
        let call_order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses: Arc<Mutex<Vec<AppStatus>>> = Arc::new(Mutex::new(Vec::new()));
        let errors: Arc<Mutex<Vec<PipelineError>>> = Arc::new(Mutex::new(Vec::new()));

        let begin_call_order = Arc::clone(&call_order);
        let stop_call_order = Arc::clone(&call_order);
        let abort_call_order = Arc::clone(&call_order);
        let emit_call_order = Arc::clone(&call_order);
        let status_call_order = Arc::clone(&call_order);
        let reset_call_order = Arc::clone(&call_order);
        let status_log = Arc::clone(&statuses);
        let reset_status_log = Arc::clone(&statuses);
        let error_log = Arc::clone(&errors);

        handle_audio_input_stream_error_with_hooks(
            "stream disconnected".to_string(),
            move || {
                begin_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("begin_session");
            },
            move || {
                stop_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("force_stop_recording");
            },
            move || {
                abort_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("abort_recording");
                Ok(())
            },
            move |error| {
                emit_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("emit_pipeline_error");
                error_log
                    .lock()
                    .expect("error lock should not be poisoned")
                    .push(error.clone());
            },
            move |status| {
                status_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("set_status");
                status_log
                    .lock()
                    .expect("status lock should not be poisoned")
                    .push(status);
            },
            move || {
                reset_call_order
                    .lock()
                    .expect("call-order lock should not be poisoned")
                    .push("schedule_reset");
                reset_status_log
                    .lock()
                    .expect("status lock should not be poisoned")
                    .push(AppStatus::Idle);
            },
        );

        assert_eq!(
            call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .clone(),
            vec![
                "begin_session",
                "force_stop_recording",
                "abort_recording",
                "emit_pipeline_error",
                "set_status",
                "schedule_reset"
            ]
        );
        assert_eq!(
            statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone(),
            vec![AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            errors
                .lock()
                .expect("error lock should not be poisoned")
                .clone(),
            vec![PipelineError {
                stage: PipelineErrorStage::RecordingRuntime,
                message: "stream disconnected".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn command_path_stage_error_recovery_resets_status_to_idle() {
        let delegate = SharedStageErrorDelegate::default();
        let observer = delegate.clone();

        let task = spawn_pipeline_stage_error_reset(
            VoicePipeline::new(Duration::ZERO),
            delegate,
            PipelineErrorStage::Transcription,
            "command transcription failed".to_string(),
        );
        task.await.expect("stage-error task should complete");

        assert_eq!(observer.statuses(), vec![AppStatus::Error, AppStatus::Idle]);
        assert_eq!(
            observer.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::Transcription,
                message: "command transcription failed".to_string(),
            }]
        );
    }
}

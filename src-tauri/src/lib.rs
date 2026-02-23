mod api_key_store;
mod audio_capture_service;
mod auth_store;
mod history_store;
mod hotkey_service;
mod logging;
mod oauth;
mod permission_service;
mod settings_store;
mod stats_store;
mod status_notifier;
mod text_insertion_service;
mod transcription;
mod voice_pipeline;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use api_key_store::ApiKeyStore;
use async_trait::async_trait;
use audio_capture_service::{
    AudioCaptureService, AudioInputChunk, AudioInputChunkCallback, AudioInputStreamErrorEvent,
    MicrophoneInfo, RecordedAudio, AUDIO_INPUT_STREAM_ERROR_EVENT, AUDIO_LEVEL_EVENT,
};
use auth_store::{AuthMethod, AuthStore};
use history_store::{HistoryEntry, HistoryStore};
use hotkey_service::{
    HotkeyConfig, HotkeyService, RecordingMode, RecordingTransition, StopProcessingDecision,
};
use logging::LoggingState;
use permission_service::{PermissionService, PermissionSnapshot, PermissionState, PermissionType};
use serde::Serialize;
use settings_store::{
    SettingsStore, VoiceSettings, VoiceSettingsUpdate, RECORDING_MODE_DOUBLE_TAP_TOGGLE,
    RECORDING_MODE_HOLD_TO_TALK, RECORDING_MODE_TOGGLE,
};
use stats_store::{StatsStore, UsageStatsReport};
use status_notifier::{AppStatus, StatusNotifier};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconEvent},
    AppHandle, Emitter, EventTarget, Listener, LogicalPosition, Manager, Monitor, PhysicalPosition,
    WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use text_insertion_service::TextInsertionService;
use tracing::{debug, error, info, warn};
use transcription::chatgpt::{ChatGptTranscriptionConfig, ChatGptTranscriptionProvider};
use transcription::openai::{OpenAiTranscriptionConfig, OpenAiTranscriptionProvider};
use transcription::realtime::{
    OpenAiRealtimeTranscriptionClient, OpenAiRealtimeTranscriptionConfig,
    RealtimeTranscriptionSession,
};
use transcription::{TranscriptionOptions, TranscriptionOrchestrator};
use voice_pipeline::{PipelineError, PipelineTranscript, VoicePipeline, VoicePipelineDelegate};

const EVENT_STATUS_CHANGED: &str = "voice://status-changed";
const EVENT_TRANSCRIPT_READY: &str = "voice://transcript-ready";
const EVENT_TRANSCRIPTION_DELTA: &str = "voice://transcription-delta";
const EVENT_PIPELINE_ERROR: &str = "voice://pipeline-error";
const EVENT_OVERLAY_AUDIO_LEVEL: &str = "voice://overlay-audio-level";
const AUDIO_STREAM_ERROR_RESET_DELAY_MS: u64 = 1_500;
const MIN_RECORDING_DURATION_MS: u64 = 200;
const DEFAULT_HISTORY_PAGE_SIZE: usize = 50;
const OVERLAY_WINDOW_LABEL: &str = "recording-overlay";
// Keep these values aligned with src/Overlay.css so the overlay shadow remains inside the window.
const OVERLAY_PILL_WIDTH: f64 = 300.0;
const OVERLAY_PILL_HEIGHT: f64 = 56.0;
const OVERLAY_SHADOW_SAFE_TOP: f64 = 24.0;
const OVERLAY_SHADOW_SAFE_SIDE: f64 = 36.0;
const OVERLAY_SHADOW_SAFE_BOTTOM: f64 = 52.0;
const OVERLAY_WINDOW_WIDTH: f64 = OVERLAY_PILL_WIDTH + (OVERLAY_SHADOW_SAFE_SIDE * 2.0);
const OVERLAY_WINDOW_HEIGHT: f64 =
    OVERLAY_PILL_HEIGHT + OVERLAY_SHADOW_SAFE_TOP + OVERLAY_SHADOW_SAFE_BOTTOM;
const OVERLAY_WINDOW_TOP_MARGIN: f64 = 12.0;
const LEGACY_APP_IDENTIFIER: &str = "com.sawyerhood.voice";

fn count_words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

fn sanitize_recording_duration_secs(duration_secs: f64) -> f64 {
    if duration_secs.is_finite() && duration_secs > 0.0 {
        duration_secs
    } else {
        0.0
    }
}

fn should_discard_recording(duration_ms: u64) -> bool {
    duration_ms < MIN_RECORDING_DURATION_MS
}

fn recording_mode_from_settings_value(value: &str) -> Result<RecordingMode, String> {
    match value.trim().to_lowercase().as_str() {
        RECORDING_MODE_HOLD_TO_TALK => Ok(RecordingMode::HoldToTalk),
        RECORDING_MODE_TOGGLE => Ok(RecordingMode::Toggle),
        RECORDING_MODE_DOUBLE_TAP_TOGGLE => Ok(RecordingMode::DoubleTapToggle),
        normalized => Err(format!(
            "Unsupported recording mode `{normalized}`. Expected `{RECORDING_MODE_HOLD_TO_TALK}`, `{RECORDING_MODE_TOGGLE}`, or `{RECORDING_MODE_DOUBLE_TAP_TOGGLE}`"
        )),
    }
}

fn recording_mode_to_settings_value(mode: RecordingMode) -> &'static str {
    match mode {
        RecordingMode::HoldToTalk => RECORDING_MODE_HOLD_TO_TALK,
        RecordingMode::Toggle => RECORDING_MODE_TOGGLE,
        RecordingMode::DoubleTapToggle => RECORDING_MODE_DOUBLE_TAP_TOGGLE,
    }
}

fn resolve_hotkey_config_for_settings(
    update: &VoiceSettingsUpdate,
    fallback_hotkey: &HotkeyConfig,
) -> Result<HotkeyConfig, String> {
    let mode = match update.recording_mode.as_deref() {
        Some(mode) => recording_mode_from_settings_value(mode)?,
        None => fallback_hotkey.mode,
    };

    Ok(HotkeyConfig {
        shortcut: update
            .hotkey_shortcut
            .clone()
            .unwrap_or_else(|| fallback_hotkey.shortcut.clone()),
        mode,
    })
}

fn apply_hotkey_from_settings_with_fallback<FApplyConfig, FApplyDefault>(
    settings: &VoiceSettings,
    mut apply_config: FApplyConfig,
    mut apply_default: FApplyDefault,
) -> Result<(), String>
where
    FApplyConfig: FnMut(HotkeyConfig) -> Result<(), String>,
    FApplyDefault: FnMut() -> Result<(), String>,
{
    let config = match recording_mode_from_settings_value(settings.recording_mode.as_str()) {
        Ok(mode) => HotkeyConfig {
            shortcut: settings.hotkey_shortcut.clone(),
            mode,
        },
        Err(error) => {
            warn!(
                %error,
                "invalid persisted recording mode; falling back to default hotkey configuration"
            );
            apply_default()?;
            return Ok(());
        }
    };

    if let Err(error) = apply_config(config) {
        warn!(
            %error,
            "failed to apply persisted hotkey configuration; falling back to defaults"
        );
        apply_default()?;
    }

    Ok(())
}

fn apply_settings_transaction_with_hooks<
    FApplyHotkey,
    FApplyLaunchAtLogin,
    FPersistSettings,
    FRollbackLaunchAtLogin,
    FRollbackHotkey,
>(
    update: VoiceSettingsUpdate,
    previous_hotkey: HotkeyConfig,
    requested_hotkey: HotkeyConfig,
    previous_launch_at_login: bool,
    requested_launch_at_login: bool,
    mut apply_hotkey: FApplyHotkey,
    mut apply_launch_at_login: FApplyLaunchAtLogin,
    mut persist_settings: FPersistSettings,
    mut rollback_launch_at_login: FRollbackLaunchAtLogin,
    mut rollback_hotkey: FRollbackHotkey,
) -> Result<VoiceSettings, String>
where
    FApplyHotkey: FnMut(HotkeyConfig) -> Result<HotkeyConfig, String>,
    FApplyLaunchAtLogin: FnMut(bool) -> Result<(), String>,
    FPersistSettings: FnMut(VoiceSettingsUpdate) -> Result<VoiceSettings, String>,
    FRollbackLaunchAtLogin: FnMut(bool) -> Result<(), String>,
    FRollbackHotkey: FnMut(HotkeyConfig) -> Result<HotkeyConfig, String>,
{
    let applied_hotkey = apply_hotkey(requested_hotkey)?;
    if let Err(launch_error) = apply_launch_at_login(requested_launch_at_login) {
        return match rollback_hotkey(previous_hotkey.clone()) {
            Ok(_) => Err(format!(
                "Failed to apply launch-at-login setting: {launch_error}"
            )),
            Err(rollback_error) => Err(format!(
                "Failed to apply launch-at-login setting: {launch_error}. Failed to roll back hotkey config: {rollback_error}"
            )),
        };
    }

    let mut update_with_applied_hotkey = update;
    update_with_applied_hotkey.hotkey_shortcut = Some(applied_hotkey.shortcut.clone());
    update_with_applied_hotkey.recording_mode =
        Some(recording_mode_to_settings_value(applied_hotkey.mode).to_string());
    if update_with_applied_hotkey.launch_at_login.is_some() {
        update_with_applied_hotkey.launch_at_login = Some(requested_launch_at_login);
    }

    match persist_settings(update_with_applied_hotkey) {
        Ok(settings) => Ok(settings),
        Err(persist_error) => {
            let mut rollback_failures = Vec::new();

            if let Err(rollback_error) = rollback_launch_at_login(previous_launch_at_login) {
                rollback_failures.push(format!(
                    "Failed to roll back launch-at-login state: {rollback_error}"
                ));
            }

            if let Err(rollback_error) = rollback_hotkey(previous_hotkey) {
                rollback_failures.push(format!(
                    "Failed to roll back hotkey config: {rollback_error}"
                ));
            }

            if rollback_failures.is_empty() {
                Err(format!("Failed to persist settings: {persist_error}"))
            } else {
                Err(format!(
                    "Failed to persist settings: {persist_error}. {}",
                    rollback_failures.join(". ")
                ))
            }
        }
    }
}

fn load_startup_settings_with_fallback<FLoadSettings>(
    mut load_settings: FLoadSettings,
) -> VoiceSettings
where
    FLoadSettings: FnMut() -> Result<VoiceSettings, String>,
{
    match load_settings() {
        Ok(settings) => {
            info!("persisted settings loaded");
            settings
        }
        Err(error) => {
            warn!(%error, "failed to load persisted settings");
            VoiceSettings::default()
        }
    }
}

fn copy_directory_contents(source_dir: &Path, destination_dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(destination_dir)?;

    for entry in fs::read_dir(source_dir)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination_dir.join(entry.file_name());
        let entry_type = entry.file_type()?;

        if entry_type.is_dir() {
            copy_directory_contents(&source_path, &destination_path)?;
        } else if entry_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        }
    }

    Ok(())
}

fn migrate_legacy_app_data_dir(new_app_data_dir: &Path) {
    let Some(parent_dir) = new_app_data_dir.parent() else {
        warn!(
            path = %new_app_data_dir.display(),
            "app data directory is missing parent path; skipping legacy migration"
        );
        return;
    };

    let legacy_app_data_dir = parent_dir.join(LEGACY_APP_IDENTIFIER);
    if !legacy_app_data_dir.is_dir() {
        return;
    }

    if new_app_data_dir.exists() {
        info!(
            path = %new_app_data_dir.display(),
            "new app data directory already exists; skipping legacy migration"
        );
        return;
    }

    match copy_directory_contents(&legacy_app_data_dir, new_app_data_dir) {
        Ok(()) => info!(
            from = %legacy_app_data_dir.display(),
            to = %new_app_data_dir.display(),
            "copied legacy app data directory contents"
        ),
        Err(error) => warn!(
            %error,
            from = %legacy_app_data_dir.display(),
            to = %new_app_data_dir.display(),
            "failed to migrate legacy app data directory contents"
        ),
    }
}

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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChatGptAuthStatus {
    account_id: String,
    expires_at: u64,
}

#[derive(Debug)]
struct AppServices {
    audio_capture_service: AudioCaptureService,
    transcription_orchestrator: TranscriptionOrchestrator,
    chatgpt_transcription_provider: ChatGptTranscriptionProvider,
    realtime_transcription_client: OpenAiRealtimeTranscriptionClient,
    text_insertion_service: TextInsertionService,
    settings_store: SettingsStore,
    api_key_store: ApiKeyStore,
    auth_store: AuthStore,
    permission_service: PermissionService,
}

impl AppServices {
    fn new(app_data_dir: PathBuf) -> Self {
        let api_key_store = ApiKeyStore::new(app_data_dir.clone());
        let auth_store = AuthStore::new(app_data_dir.clone());
        let mut openai_config = OpenAiTranscriptionConfig::from_env();
        openai_config.api_key_store_app_data_dir = Some(app_data_dir.clone());
        let provider = OpenAiTranscriptionProvider::new(openai_config.clone());
        let transcription_orchestrator = TranscriptionOrchestrator::new(Arc::new(provider));
        let chatgpt_transcription_provider = ChatGptTranscriptionProvider::new(
            ChatGptTranscriptionConfig::from_env(),
            auth_store.clone(),
        );
        let mut realtime_config = OpenAiRealtimeTranscriptionConfig::from_env();
        if realtime_config.transcription_model.trim().is_empty() {
            realtime_config.transcription_model = openai_config.model.clone();
        }
        realtime_config.api_key = openai_config.api_key.clone();
        realtime_config.api_key_store_app_data_dir = Some(app_data_dir.clone());
        let realtime_transcription_client = OpenAiRealtimeTranscriptionClient::new(realtime_config);
        info!("initializing app services");

        Self {
            audio_capture_service: AudioCaptureService::new(),
            transcription_orchestrator,
            chatgpt_transcription_provider,
            realtime_transcription_client,
            text_insertion_service: TextInsertionService::new(),
            settings_store: SettingsStore::new(),
            api_key_store,
            auth_store,
            permission_service: PermissionService::new(),
        }
    }

    fn current_auth_method(&self) -> Result<AuthMethod, String> {
        self.auth_store.effective_auth_method(&self.api_key_store)
    }
}

#[derive(Debug)]
struct AppState {
    status_notifier: Mutex<StatusNotifier>,
    services: AppServices,
}

impl AppState {
    fn new(app_data_dir: PathBuf) -> Self {
        Self {
            status_notifier: Mutex::new(StatusNotifier::default()),
            services: AppServices::new(app_data_dir),
        }
    }
}

#[derive(Debug, Clone)]
struct PipelineRuntimeState {
    execution_lock: Arc<tokio::sync::Mutex<()>>,
    next_session_id: Arc<AtomicU64>,
    active_session_id: Arc<AtomicU64>,
    realtime_session: Arc<Mutex<Option<RealtimeTranscriptionSession>>>,
}

impl Default for PipelineRuntimeState {
    fn default() -> Self {
        Self {
            execution_lock: Arc::new(tokio::sync::Mutex::new(())),
            next_session_id: Arc::new(AtomicU64::new(0)),
            active_session_id: Arc::new(AtomicU64::new(0)),
            realtime_session: Arc::new(Mutex::new(None)),
        }
    }
}

impl PipelineRuntimeState {
    fn begin_session(&self) -> u64 {
        let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.active_session_id.store(session_id, Ordering::Relaxed);
        self.clear_realtime_session();
        debug!(session_id, "pipeline session started");
        session_id
    }

    fn active_session_id(&self) -> Option<u64> {
        let session_id = self.active_session_id.load(Ordering::Relaxed);
        (session_id > 0).then_some(session_id)
    }

    fn is_session_active(&self, session_id: u64) -> bool {
        self.active_session_id.load(Ordering::Relaxed) == session_id
    }

    fn clear_realtime_session(&self) {
        match self.realtime_session.lock() {
            Ok(mut guard) => {
                if let Some(session) = guard.take() {
                    session.close();
                }
            }
            Err(_) => {
                error!("failed to clear realtime session because runtime lock was poisoned");
            }
        }
    }
}

#[derive(Clone)]
struct AppPipelineDelegate {
    app: AppHandle,
    session_id: Option<u64>,
    realtime_session: Arc<Mutex<Option<RealtimeTranscriptionSession>>>,
    recording_duration_secs: Arc<Mutex<Option<f64>>>,
}

impl AppPipelineDelegate {
    fn new(app: AppHandle) -> Self {
        let realtime_session = {
            let runtime_state = app.state::<PipelineRuntimeState>();
            Arc::clone(&runtime_state.realtime_session)
        };
        Self {
            app,
            session_id: None,
            realtime_session,
            recording_duration_secs: Arc::new(Mutex::new(None)),
        }
    }

    fn for_session(app: AppHandle, session_id: u64) -> Self {
        let realtime_session = {
            let runtime_state = app.state::<PipelineRuntimeState>();
            Arc::clone(&runtime_state.realtime_session)
        };
        Self {
            app,
            session_id: Some(session_id),
            realtime_session,
            recording_duration_secs: Arc::new(Mutex::new(None)),
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

    fn current_settings(&self) -> VoiceSettings {
        let state = self.app.state::<AppState>();
        state.services.settings_store.current()
    }

    fn build_delta_callback(&self) -> transcription::TranscriptionDeltaCallback {
        let app_for_delta = self.app.clone();
        let session_id_for_delta = self.session_id;
        Arc::new(move |delta| {
            if let Some(session_id) = session_id_for_delta {
                let runtime_state = app_for_delta.state::<PipelineRuntimeState>();
                if !runtime_state.is_session_active(session_id) {
                    return;
                }
            }
            emit_transcription_delta_event(&app_for_delta, &delta);
        })
    }

    fn store_realtime_session(&self, session: Option<RealtimeTranscriptionSession>) {
        if self.session_id.is_some() && !self.is_session_active() {
            if let Some(stale_session) = session {
                stale_session.close();
            }
            debug!(
                session_id = ?self.session_id,
                "ignoring realtime session store for inactive session"
            );
            return;
        }

        if let Ok(mut guard) = self.realtime_session.lock() {
            *guard = session;
        } else {
            warn!(
                session_id = ?self.session_id,
                "failed to store realtime session because session lock was poisoned"
            );
        }
    }

    fn take_realtime_session(&self) -> Option<RealtimeTranscriptionSession> {
        if self.session_id.is_some() && !self.is_session_active() {
            debug!(
                session_id = ?self.session_id,
                "ignoring realtime session access for inactive session"
            );
            return None;
        }

        match self.realtime_session.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => {
                warn!(
                    session_id = ?self.session_id,
                    "failed to access realtime session because session lock was poisoned"
                );
                None
            }
        }
    }

    fn clear_realtime_session(&self) {
        if let Some(session) = self.take_realtime_session() {
            session.close();
        }
    }

    fn store_recording_duration_secs(&self, duration_secs: Option<f64>) {
        match self.recording_duration_secs.lock() {
            Ok(mut guard) => {
                *guard = duration_secs.map(sanitize_recording_duration_secs);
            }
            Err(_) => {
                warn!(
                    session_id = ?self.session_id,
                    "failed to store recording duration because lock was poisoned"
                );
            }
        }
    }

    fn take_recording_duration_secs(&self) -> Option<f64> {
        match self.recording_duration_secs.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => {
                warn!(
                    session_id = ?self.session_id,
                    "failed to read recording duration because lock was poisoned"
                );
                None
            }
        }
    }

    fn clear_recording_duration_secs(&self) {
        self.store_recording_duration_secs(None);
    }

    fn record_usage_stats_for_transcript(&self, transcript: &str) {
        let word_count = count_words(transcript);
        let recording_duration_secs = self.take_recording_duration_secs().unwrap_or(0.0);
        let stats_store = self.app.state::<StatsStore>();

        if let Err(error) = stats_store.record_transcription(word_count, recording_duration_secs) {
            warn!(
                session_id = ?self.session_id,
                word_count,
                recording_duration_secs,
                %error,
                "failed to persist usage stats"
            );
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
        if !success {
            self.clear_realtime_session();
            self.clear_recording_duration_secs();
        }
        let hotkey_service = self.app.state::<HotkeyService>();
        hotkey_service.acknowledge_transition(RecordingTransition::Stopped, success);
    }

    fn start_recording(&self) -> Result<(), String> {
        let settings = self.current_settings();
        info!(
            session_id = ?self.session_id,
            microphone_id = ?settings.microphone_id.as_deref(),
            "pipeline requested recording start"
        );
        let state = self.app.state::<AppState>();
        ensure_microphone_permission_for_recording(&state)?;

        self.clear_realtime_session();
        self.clear_recording_duration_secs();

        let auth_method = state
            .services
            .current_auth_method()
            .map_err(|error| format!("Failed to resolve active auth method: {error}"))?;

        let realtime_session = if auth_method == AuthMethod::ApiKey
            && state
                .services
                .realtime_transcription_client
                .model_supports_realtime()
        {
            let options = TranscriptionOptions {
                language: settings.language.clone(),
                on_delta: Some(self.build_delta_callback()),
                ..TranscriptionOptions::default()
            };
            match state
                .services
                .realtime_transcription_client
                .begin_session(options)
            {
                Ok(session) => {
                    info!(
                        session_id = ?self.session_id,
                        model = %state.services.realtime_transcription_client.model(),
                        "realtime transcription session prepared"
                    );
                    Some(session)
                }
                Err(error) => {
                    warn!(
                        session_id = ?self.session_id,
                        error = %error,
                        "unable to start realtime transcription session; will fall back to REST upload"
                    );
                    None
                }
            }
        } else if auth_method != AuthMethod::ApiKey {
            debug!(
                session_id = ?self.session_id,
                auth_method = auth_method.as_str(),
                "active auth method does not support realtime transcription; using REST upload fallback"
            );
            None
        } else {
            debug!(
                session_id = ?self.session_id,
                model = %state.services.realtime_transcription_client.model(),
                "configured model does not support realtime transcription; using REST upload fallback"
            );
            None
        };

        let chunk_callback: Option<AudioInputChunkCallback> =
            realtime_session.as_ref().map(|session| {
                let audio_sender = session.audio_sender();
                let append_error_logged = Arc::new(AtomicBool::new(false));
                let append_error_logged_for_callback = Arc::clone(&append_error_logged);
                let session_id = self.session_id;
                Arc::new(move |chunk: AudioInputChunk| {
                    if let Err(error) = audio_sender
                        .append_pcm16_mono(chunk.pcm16_mono_samples, chunk.sample_rate_hz)
                    {
                        if !append_error_logged_for_callback.swap(true, Ordering::Relaxed) {
                            warn!(
                                session_id = ?session_id,
                                error = %error,
                                "failed to forward audio chunk to realtime transcription session"
                            );
                        }
                    }
                }) as AudioInputChunkCallback
            });

        let start_result = state.services.audio_capture_service.start_recording(
            self.app.clone(),
            settings.microphone_id.as_deref(),
            chunk_callback,
        );

        if start_result.is_ok() {
            self.store_realtime_session(realtime_session);
            start_result
        } else {
            if let Some(session) = realtime_session {
                session.close();
            }
            start_result
        }
    }

    fn stop_recording(&self) -> Result<Vec<u8>, String> {
        info!(session_id = ?self.session_id, "pipeline requested recording stop");
        let state = self.app.state::<AppState>();
        let result = state
            .services
            .audio_capture_service
            .stop_recording(self.app.clone())
            .map(|recorded| {
                if should_discard_recording(recorded.duration_ms) {
                    debug!(
                        session_id = ?self.session_id,
                        duration_ms = recorded.duration_ms,
                        min_duration_ms = MIN_RECORDING_DURATION_MS,
                        "recording too short, discarding"
                    );
                    self.clear_realtime_session();
                    self.clear_recording_duration_secs();
                    return Vec::new();
                }
                let duration_secs = recorded.duration_ms as f64 / 1000.0;
                self.store_recording_duration_secs(Some(duration_secs));
                recorded.wav_bytes
            });
        if result.is_err() {
            self.clear_realtime_session();
            self.clear_recording_duration_secs();
        }
        result
    }

    async fn transcribe(&self, wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
        let settings = self.current_settings();
        let options = TranscriptionOptions {
            language: settings.language,
            on_delta: Some(self.build_delta_callback()),
            ..TranscriptionOptions::default()
        };
        let state = self.app.state::<AppState>();
        let auth_method = state
            .services
            .current_auth_method()
            .map_err(|error| format!("Failed to resolve active auth method: {error}"))?;
        let orchestrator = state.services.transcription_orchestrator.clone();
        let chatgpt_provider = state.services.chatgpt_transcription_provider.clone();
        let provider_name = match auth_method {
            AuthMethod::ApiKey => "openai",
            AuthMethod::ChatgptOauth => "chatgpt-oauth",
            AuthMethod::None => "none",
        }
        .to_string();
        let provider_name_for_error = provider_name.clone();

        if auth_method == AuthMethod::ApiKey {
            if let Some(realtime_session) = self.take_realtime_session() {
                info!(
                    session_id = ?self.session_id,
                    provider = "openai-realtime",
                    "awaiting realtime transcription completion"
                );

                match realtime_session.commit_and_wait().await {
                    Ok(transcription) => {
                        let transcript = PipelineTranscript {
                            text: transcription.text,
                            duration_secs: transcription.duration_secs,
                            language: transcription.language,
                            provider: "openai-realtime".to_string(),
                        };
                        info!(
                            session_id = ?self.session_id,
                            provider = %transcript.provider,
                            transcript_chars = transcript.text.chars().count(),
                            "realtime transcription completed"
                        );
                        return Ok(transcript);
                    }
                    Err(error) => {
                        warn!(
                            session_id = ?self.session_id,
                            error = %error,
                            provider = "openai-realtime",
                            "realtime transcription failed; falling back to REST upload"
                        );
                    }
                }
            }
        } else {
            self.clear_realtime_session();
        }

        if auth_method == AuthMethod::None {
            return Err(
                "No authentication configured. Add an OpenAI API key or login with ChatGPT."
                    .to_string(),
            );
        }

        info!(
            session_id = ?self.session_id,
            provider = %provider_name,
            audio_bytes = wav_bytes.len(),
            "starting REST transcription fallback request"
        );

        let transcription = match auth_method {
            AuthMethod::ApiKey => orchestrator.transcribe(wav_bytes, options).await,
            AuthMethod::ChatgptOauth => {
                chatgpt_provider
                    .transcribe_via_webview(&self.app, wav_bytes, options)
                    .await
            }
            AuthMethod::None => unreachable!("auth method none is handled above"),
        };

        transcription
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
        let auto_insert = state.services.settings_store.current().auto_insert;

        let insertion_result = if auto_insert {
            ensure_accessibility_permission_for_insertion(&state)?;
            state
                .services
                .text_insertion_service
                .insert_text(transcript)
        } else {
            state
                .services
                .text_insertion_service
                .copy_to_clipboard(transcript)
        };

        if insertion_result.is_ok() {
            self.record_usage_stats_for_transcript(transcript);
        }

        insertion_result
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

    set_overlay_visible_for_status(app, status);

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

fn emit_transcription_delta_event(app: &AppHandle, delta: &str) {
    if let Err(error) = app.emit(EVENT_TRANSCRIPTION_DELTA, delta.to_string()) {
        warn!(%error, "failed to emit transcription delta event");
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

fn should_show_overlay_for_status(status: AppStatus) -> bool {
    matches!(status, AppStatus::Listening | AppStatus::Transcribing)
}

fn overlay_position_from_work_area(
    work_area_position: PhysicalPosition<i32>,
    work_area_width: u32,
    scale_factor: f64,
) -> LogicalPosition<f64> {
    let work_area_x = f64::from(work_area_position.x) / scale_factor;
    let work_area_y = f64::from(work_area_position.y) / scale_factor;
    let work_area_width = f64::from(work_area_width) / scale_factor;

    LogicalPosition::new(
        work_area_x + ((work_area_width - OVERLAY_WINDOW_WIDTH) / 2.0).max(0.0),
        work_area_y + OVERLAY_WINDOW_TOP_MARGIN,
    )
}

fn resolve_overlay_monitor(app: &AppHandle) -> Option<Monitor> {
    if let Ok(cursor) = app.cursor_position() {
        if let Ok(Some(cursor_monitor)) = app.monitor_from_point(cursor.x, cursor.y) {
            return Some(cursor_monitor);
        }
    }

    if let Some(main_window) = app.get_webview_window("main") {
        if let Ok(Some(main_monitor)) = main_window.current_monitor() {
            return Some(main_monitor);
        }
    }

    app.primary_monitor().ok().flatten()
}

fn position_overlay_window(window: &WebviewWindow, app: &AppHandle) {
    if let Some(monitor) = resolve_overlay_monitor(app) {
        let position = overlay_position_from_work_area(
            monitor.work_area().position,
            monitor.work_area().size.width,
            monitor.scale_factor(),
        );
        if let Err(error) = window.set_position(position) {
            warn!(%error, "failed to position recording overlay");
        }
    }
}

fn create_recording_overlay_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    let window = WebviewWindowBuilder::new(
        app,
        OVERLAY_WINDOW_LABEL,
        WebviewUrl::App("overlay.html".into()),
    )
    .title("Voice Recording Overlay")
    .inner_size(OVERLAY_WINDOW_WIDTH, OVERLAY_WINDOW_HEIGHT)
    .min_inner_size(OVERLAY_WINDOW_WIDTH, OVERLAY_WINDOW_HEIGHT)
    .max_inner_size(OVERLAY_WINDOW_WIDTH, OVERLAY_WINDOW_HEIGHT)
    .resizable(false)
    .decorations(false)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .visible_on_all_workspaces(true)
    .focusable(false)
    .focused(false)
    .visible(false)
    .transparent(true)
    .build()
    .map_err(|error| format!("failed to create recording overlay window: {error}"))?;

    if let Err(error) = window.set_focusable(false) {
        warn!(%error, "failed to set recording overlay as non-focusable");
    }
    if let Err(error) = window.set_ignore_cursor_events(false) {
        warn!(%error, "failed to enable recording overlay cursor events");
    }

    position_overlay_window(&window, app);
    Ok(window)
}

fn setup_recording_overlay_window(app: &AppHandle) {
    if app.get_webview_window(OVERLAY_WINDOW_LABEL).is_some() {
        return;
    }

    match create_recording_overlay_window(app) {
        Ok(_) => info!("recording overlay window initialized"),
        Err(error) => warn!(%error, "recording overlay window initialization failed"),
    }
}

fn set_overlay_visible_for_status(app: &AppHandle, status: AppStatus) {
    let Some(window) = app.get_webview_window(OVERLAY_WINDOW_LABEL) else {
        return;
    };

    if should_show_overlay_for_status(status) {
        position_overlay_window(&window, app);
        if let Err(error) = window.show() {
            warn!(%error, "failed to show recording overlay window");
        }
    } else if let Err(error) = window.hide() {
        warn!(%error, "failed to hide recording overlay window");
    }
}

fn register_overlay_audio_forwarder(app: &AppHandle) {
    let overlay_app = app.clone();
    app.listen(AUDIO_LEVEL_EVENT, move |event| {
        let level = serde_json::from_str::<f32>(event.payload()).unwrap_or_else(|error| {
            warn!(%error, payload = event.payload(), "invalid audio-level payload");
            0.0
        });
        if let Err(error) = overlay_app.emit_to(
            EventTarget::webview_window(OVERLAY_WINDOW_LABEL),
            EVENT_OVERLAY_AUDIO_LEVEL,
            level,
        ) {
            warn!(%error, "failed to forward audio level to recording overlay");
        }
    });
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

fn get_launch_at_login_state(app: &AppHandle) -> Result<bool, String> {
    app.autolaunch()
        .is_enabled()
        .map_err(|error| format!("Failed to get launch-at-login state: {error}"))
}

fn set_launch_at_login_state(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let autolaunch = app.autolaunch();
    let result = if enabled {
        autolaunch.enable()
    } else {
        autolaunch.disable()
    };

    result.map_err(|error| format!("Failed to set launch-at-login state: {error}"))
}

fn ensure_microphone_permission_for_recording(state: &AppState) -> Result<(), String> {
    ensure_permission_for_action(
        state.services.permission_service.microphone_permission(),
        PermissionType::Microphone,
        "start recording",
    )
}

fn ensure_accessibility_permission_for_insertion(state: &AppState) -> Result<(), String> {
    ensure_permission_for_action(
        state.services.permission_service.accessibility_permission(),
        PermissionType::Accessibility,
        "insert text",
    )
}

fn ensure_permission_for_action(
    permission_state: PermissionState,
    permission_type: PermissionType,
    action: &str,
) -> Result<(), String> {
    if permission_state == PermissionState::Granted {
        return Ok(());
    }

    Err(permission_preflight_error_message(
        permission_type,
        permission_state,
        action,
    ))
}

fn permission_preflight_error_message(
    permission_type: PermissionType,
    permission_state: PermissionState,
    action: &str,
) -> String {
    match permission_type {
        PermissionType::Microphone => match permission_state {
            PermissionState::NotDetermined => format!(
                "Microphone access has not been granted yet, so Voice cannot {action}. Click Grant Access in the app and allow Voice in System Settings > Privacy & Security > Microphone."
            ),
            PermissionState::Denied => format!(
                "Microphone access is denied, so Voice cannot {action}. Enable Voice in System Settings > Privacy & Security > Microphone, then try again."
            ),
            PermissionState::Granted => String::new(),
        },
        PermissionType::Accessibility => match permission_state {
            PermissionState::NotDetermined | PermissionState::Denied => format!(
                "Accessibility access is required to {action}. Enable Voice in System Settings > Privacy & Security > Accessibility, then try again."
            ),
            PermissionState::Granted => String::new(),
        },
    }
}

fn cancel_recording_with_hooks<BeginSession, ForceStopRecording, AbortRecording, SetStatus>(
    mut begin_session: BeginSession,
    mut force_stop_recording: ForceStopRecording,
    mut abort_recording: AbortRecording,
    mut set_status: SetStatus,
) -> Result<bool, String>
where
    BeginSession: FnMut(),
    ForceStopRecording: FnMut(),
    AbortRecording: FnMut() -> Result<bool, String>,
    SetStatus: FnMut(AppStatus),
{
    begin_session();
    force_stop_recording();
    let abort_result = abort_recording();
    set_status(AppStatus::Idle);
    abort_result
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

fn active_pipeline_session_id(runtime_state: &PipelineRuntimeState) -> Option<u64> {
    runtime_state.active_session_id()
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

            handle_pending_stop_transition(&app, &delegate).await;
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
            let hotkey_service = app.state::<HotkeyService>();
            let stop_decision = hotkey_service.stop_processing_decision();
            drop(hotkey_service);

            match stop_decision {
                StopProcessingDecision::Process => {
                    let Some(session_id) = active_pipeline_session_id(&runtime_state) else {
                        warn!(
                            "received stop event with no active pipeline session; acknowledging stop as failed"
                        );
                        let hotkey_service = app.state::<HotkeyService>();
                        hotkey_service
                            .acknowledge_transition(RecordingTransition::Stopped, false);
                        return;
                    };
                    let delegate = AppPipelineDelegate::for_session(app.clone(), session_id);
                    VoicePipeline::default()
                        .handle_hotkey_stopped(&delegate)
                        .await;
                }
                StopProcessingDecision::AcknowledgeOnly => {
                    warn!("received stop event while hotkey service was not recording");
                    let hotkey_service = app.state::<HotkeyService>();
                    hotkey_service.acknowledge_transition(RecordingTransition::Stopped, false);
                }
                StopProcessingDecision::DeferUntilStarted => {
                    debug!("deferring stop processing until start transition is acknowledged");
                }
                StopProcessingDecision::Ignore => {
                    debug!("ignoring redundant stop event");
                }
            }
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

async fn handle_pending_stop_transition(app: &AppHandle, delegate: &AppPipelineDelegate) {
    let stop_decision = {
        let hotkey_service = app.state::<HotkeyService>();
        hotkey_service.stop_processing_decision()
    };

    match stop_decision {
        StopProcessingDecision::Process => {
            VoicePipeline::default()
                .handle_hotkey_stopped(delegate)
                .await;
        }
        StopProcessingDecision::AcknowledgeOnly => {
            let hotkey_service = app.state::<HotkeyService>();
            hotkey_service.acknowledge_transition(RecordingTransition::Stopped, false);
        }
        StopProcessingDecision::DeferUntilStarted | StopProcessingDecision::Ignore => {}
    }
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
fn get_onboarding_status(state: tauri::State<'_, AppState>) -> bool {
    state.services.settings_store.current().onboarding_completed
}

#[tauri::command]
fn complete_onboarding(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    state.services.settings_store.update(
        &app,
        VoiceSettingsUpdate {
            onboarding_completed: Some(true),
            ..VoiceSettingsUpdate::default()
        },
    )?;
    Ok(true)
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
fn apply_settings(
    app: AppHandle,
    update: VoiceSettingsUpdate,
    state: tauri::State<'_, AppState>,
    hotkey_service: tauri::State<'_, HotkeyService>,
) -> Result<VoiceSettings, String> {
    let previous_hotkey = hotkey_service.current_config();
    let requested_hotkey = resolve_hotkey_config_for_settings(&update, &previous_hotkey)?;
    let previous_launch_at_login = get_launch_at_login_state(&app)?;
    let requested_launch_at_login = update.launch_at_login.unwrap_or(previous_launch_at_login);

    apply_settings_transaction_with_hooks(
        update,
        previous_hotkey,
        requested_hotkey,
        previous_launch_at_login,
        requested_launch_at_login,
        |config| hotkey_service.apply_config(&app, config),
        |enabled| set_launch_at_login_state(&app, enabled),
        |persist_update| state.services.settings_store.update(&app, persist_update),
        |enabled| set_launch_at_login_state(&app, enabled),
        |config| hotkey_service.apply_config(&app, config),
    )
}

#[tauri::command]
fn get_launch_at_login(app: AppHandle) -> Result<bool, String> {
    get_launch_at_login_state(&app)
}

#[tauri::command]
fn set_launch_at_login(
    app: AppHandle,
    enabled: bool,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let previous = get_launch_at_login_state(&app)?;
    set_launch_at_login_state(&app, enabled)?;

    if let Err(error) = state.services.settings_store.update(
        &app,
        VoiceSettingsUpdate {
            launch_at_login: Some(enabled),
            ..VoiceSettingsUpdate::default()
        },
    ) {
        if let Err(rollback_error) = set_launch_at_login_state(&app, previous) {
            return Err(format!(
                "Failed to persist launch-at-login setting: {error}. Failed to roll back launch-at-login state: {rollback_error}"
            ));
        }

        return Err(format!(
            "Failed to persist launch-at-login setting: {error}"
        ));
    }

    Ok(enabled)
}

#[tauri::command]
fn has_api_key(provider: String, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    debug!(provider = %provider, "api key presence lookup requested");
    let result = state.services.api_key_store.has_api_key(provider.as_str());
    match &result {
        Ok(true) => debug!(provider = %provider, "api key is present"),
        Ok(false) => debug!(provider = %provider, "api key is absent"),
        Err(error) => error!(provider = %provider, %error, "api key lookup failed"),
    }
    result
}

#[tauri::command]
fn get_auth_method(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let method = state.services.current_auth_method()?;
    Ok(method.as_str().to_string())
}

#[tauri::command]
fn set_auth_method(method: String, state: tauri::State<'_, AppState>) -> Result<String, String> {
    let parsed = AuthMethod::parse(&method)?;
    state.services.auth_store.set_auth_method(parsed)?;
    Ok(parsed.as_str().to_string())
}

#[tauri::command]
fn get_chatgpt_auth_status(
    state: tauri::State<'_, AppState>,
) -> Result<Option<ChatGptAuthStatus>, String> {
    Ok(state
        .services
        .auth_store
        .chatgpt_credentials()?
        .map(|credentials| ChatGptAuthStatus {
            account_id: credentials.account_id,
            expires_at: credentials.expires_at,
        }))
}

#[tauri::command]
fn get_auth_status(state: tauri::State<'_, AppState>) -> Result<Option<ChatGptAuthStatus>, String> {
    get_chatgpt_auth_status(state)
}

#[tauri::command]
async fn start_chatgpt_login(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<ChatGptAuthStatus, String> {
    info!("ChatGPT OAuth login requested");
    let login = oauth::start_chatgpt_login(&app).await?;
    state.services.auth_store.save_chatgpt_login(
        &login.access_token,
        &login.refresh_token,
        login.expires_at,
        &login.account_id,
    )?;

    Ok(ChatGptAuthStatus {
        account_id: login.account_id,
        expires_at: login.expires_at,
    })
}

#[tauri::command]
async fn start_oauth_login(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<ChatGptAuthStatus, String> {
    start_chatgpt_login(app, state).await
}

#[tauri::command]
fn logout_chatgpt(state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!("ChatGPT OAuth logout requested");
    state.services.auth_store.logout_chatgpt()?;
    Ok(())
}

#[tauri::command]
fn save_api_key(
    provider: String,
    key: String,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    set_api_key(provider, key, state)
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
        return result;
    }

    if provider.trim().eq_ignore_ascii_case("openai") {
        if let Err(error) = state.services.auth_store.set_api_key(key.as_str()) {
            error!(provider = %provider, %error, "failed to update auth store after setting API key");
            return Err(error);
        }
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
        return result;
    }

    if provider.trim().eq_ignore_ascii_case("openai") {
        if let Err(error) = state.services.auth_store.clear_api_key() {
            error!(provider = %provider, %error, "failed to update auth store after deleting API key");
            return Err(error);
        }
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
fn check_permissions(state: tauri::State<'_, AppState>) -> PermissionSnapshot {
    state.services.permission_service.check_permissions()
}

#[tauri::command]
fn request_permission(
    r#type: PermissionType,
    state: tauri::State<'_, AppState>,
) -> Result<PermissionSnapshot, String> {
    state.services.permission_service.request_permission(r#type)
}

#[tauri::command]
fn request_mic_permission(state: tauri::State<'_, AppState>) -> Result<PermissionSnapshot, String> {
    state
        .services
        .permission_service
        .request_microphone_permission()
}

#[tauri::command]
fn open_accessibility_settings(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state
        .services
        .permission_service
        .open_accessibility_settings()
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
    ensure_microphone_permission_for_recording(&state)?;

    let result = state.services.audio_capture_service.start_recording(
        app.clone(),
        microphone_id.as_deref(),
        None,
    );

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
fn cancel_recording(app: AppHandle, state: tauri::State<'_, AppState>) -> Result<(), String> {
    info!("recording/transcription cancel requested");
    let cancel_result = cancel_recording_with_hooks(
        || {
            let runtime_state = app.state::<PipelineRuntimeState>();
            runtime_state.begin_session();
        },
        || {
            let hotkey_service = app.state::<HotkeyService>();
            hotkey_service.force_stop_recording(&app);
        },
        || {
            state
                .services
                .audio_capture_service
                .abort_recording(app.clone())
        },
        |status| set_status_for_state(&app, &state, status),
    );

    match cancel_result {
        Ok(recording_aborted) => {
            info!(
                recording_aborted,
                "recording/transcription cancel completed"
            );
            Ok(())
        }
        Err(error) => {
            error!(%error, "recording/transcription cancel failed");
            Err(error)
        }
    }
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
    ensure_accessibility_permission_for_insertion(&state)?;
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
    let app_for_delta = app.clone();
    let mut request_options = options.unwrap_or_default();
    request_options.on_delta = Some(Arc::new(move |delta| {
        emit_transcription_delta_event(&app_for_delta, &delta);
    }));
    let auth_method = state.services.current_auth_method()?;
    let orchestrator = state.services.transcription_orchestrator.clone();
    let chatgpt_provider = state.services.chatgpt_transcription_provider.clone();

    let result = match auth_method {
        AuthMethod::ApiKey => orchestrator.transcribe(audio_bytes, request_options).await,
        AuthMethod::ChatgptOauth => {
            chatgpt_provider
                .transcribe_via_webview(&app, audio_bytes, request_options)
                .await
        }
        AuthMethod::None => Err(transcription::TranscriptionError::Provider(
            "No authentication configured. Add an OpenAI API key or login with ChatGPT."
                .to_string(),
        )),
    };

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
fn get_usage_stats(stats_store: tauri::State<'_, StatsStore>) -> Result<UsageStatsReport, String> {
    debug!("usage stats requested");
    stats_store.get_usage_stats()
}

#[tauri::command]
fn reset_usage_stats(stats_store: tauri::State<'_, StatsStore>) -> Result<(), String> {
    info!("usage stats reset requested");
    stats_store.reset_usage_stats()
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
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None::<Vec<&str>>,
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(HotkeyService::new())
        .manage(PipelineRuntimeState::default())
        .setup(|app| {
            let logging_state = logging::initialize(app.handle()).map_err(std::io::Error::other)?;
            app.manage(logging_state);

            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            info!("setup started");

            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(std::io::Error::other)?;
            migrate_legacy_app_data_dir(&app_data_dir);
            app.manage(AppState::new(app_data_dir.clone()));
            info!(path = %app_data_dir.display(), "app state initialized");

            let history_store = HistoryStore::new(app.handle()).map_err(std::io::Error::other)?;
            app.manage(history_store);
            info!("history store initialized");

            let stats_store = StatsStore::new(app.handle()).map_err(std::io::Error::other)?;
            app.manage(stats_store);
            info!("usage stats store initialized");

            app.handle()
                .plugin(tauri_plugin_global_shortcut::Builder::new().build())?;
            info!("global shortcut plugin initialized");

            let hotkey_service = app.state::<HotkeyService>();
            let app_state = app.state::<AppState>();

            let permission_state = app_state
                .services
                .permission_service
                .microphone_permission();
            info!(?permission_state, "microphone permission check completed");

            let settings = load_startup_settings_with_fallback(|| {
                app_state.services.settings_store.load(app.handle())
            });
            let launch_at_login = settings.launch_at_login;

            apply_hotkey_from_settings_with_fallback(
                &settings,
                |config| {
                    hotkey_service
                        .apply_config(app.handle(), config)
                        .map(|_| ())
                },
                || hotkey_service.register_default_shortcut(app.handle()),
            )
            .map_err(std::io::Error::other)?;
            info!("hotkey configuration applied");

            if let Err(error) = set_launch_at_login_state(app.handle(), launch_at_login) {
                warn!(%error, "failed to apply launch-at-login preference");
            }

            setup_recording_overlay_window(app.handle());
            register_overlay_audio_forwarder(app.handle());
            register_pipeline_handlers(app.handle());
            set_status_for_app(app.handle(), AppStatus::Idle);
            info!("overlay, pipeline handlers, and initial status configured");

            let show_item =
                MenuItem::with_id(app, "show_window", "Open Voice", true, None::<&str>)?;
            let hide_item =
                MenuItem::with_id(app, "hide_window", "Hide Voice", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit Voice", true, None::<&str>)?;
            let tray_menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

            let tray_icon_image = tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))
                .expect("failed to decode tray icon PNG");

            tauri::tray::TrayIconBuilder::with_id("voice-tray")
                .icon(tray_icon_image)
                .icon_as_template(true)
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
                info!(window = %window.label(), "window close requested; hiding instead");
                if let Err(error) = window.hide() {
                    warn!(%error, window = %window.label(), "failed to hide window on close request");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            set_status,
            get_settings,
            get_onboarding_status,
            complete_onboarding,
            update_settings,
            apply_settings,
            get_launch_at_login,
            set_launch_at_login,
            has_api_key,
            get_auth_method,
            set_auth_method,
            get_chatgpt_auth_status,
            get_auth_status,
            start_chatgpt_login,
            start_oauth_login,
            logout_chatgpt,
            save_api_key,
            set_api_key,
            delete_api_key,
            list_microphones,
            check_permissions,
            request_permission,
            request_mic_permission,
            open_accessibility_settings,
            start_recording,
            stop_recording,
            cancel_recording,
            get_audio_level,
            insert_text,
            copy_to_clipboard,
            transcribe_audio,
            list_history,
            get_history_entry,
            delete_history_entry,
            clear_history,
            get_usage_stats,
            reset_usage_stats,
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
        path::{Path, PathBuf},
        sync::{atomic::Ordering, Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use tauri::PhysicalPosition;
    use tokio::sync::{oneshot, Notify};
    use uuid::Uuid;

    use crate::{
        hotkey_service::{HotkeyConfig, RecordingMode},
        settings_store::{
            VoiceSettings, VoiceSettingsUpdate, RECORDING_MODE_DOUBLE_TAP_TOGGLE,
            RECORDING_MODE_TOGGLE,
        },
        status_notifier::AppStatus,
        voice_pipeline::{
            PipelineError, PipelineErrorStage, PipelineTranscript, VoicePipeline,
            VoicePipelineDelegate,
        },
    };

    use super::{
        active_pipeline_session_id, apply_hotkey_from_settings_with_fallback,
        apply_settings_transaction_with_hooks, cancel_recording_with_hooks,
        copy_directory_contents, handle_audio_input_stream_error_with_hooks, has_api_key,
        load_startup_settings_with_fallback, migrate_legacy_app_data_dir,
        overlay_position_from_work_area, permission_preflight_error_message,
        should_show_overlay_for_status, spawn_pipeline_stage_error_reset, AppState,
        PipelineRuntimeState, OVERLAY_WINDOW_TOP_MARGIN, OVERLAY_WINDOW_WIDTH,
    };
    use crate::permission_service::{PermissionState, PermissionType};

    #[derive(Debug)]
    struct TempDirGuard {
        path: PathBuf,
    }

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("temporary directory should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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

    #[derive(Debug, Default)]
    struct InsertionFailureDelegate {
        statuses: Mutex<Vec<AppStatus>>,
        errors: Mutex<Vec<PipelineError>>,
        transcripts: Mutex<Vec<String>>,
        saved_history: Mutex<Vec<PipelineTranscript>>,
    }

    impl InsertionFailureDelegate {
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

        fn saved_history(&self) -> Vec<PipelineTranscript> {
            self.saved_history
                .lock()
                .expect("saved-history lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl VoicePipelineDelegate for InsertionFailureDelegate {
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
            Ok(vec![7, 8, 9])
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
            Ok(PipelineTranscript {
                text: "hello world".to_string(),
                duration_secs: Some(2.4),
                language: Some("en".to_string()),
                provider: "test".to_string(),
            })
        }

        fn insert_text(&self, _transcript: &str) -> Result<(), String> {
            Err("accessibility denied".to_string())
        }

        fn save_history_entry(&self, transcript: &PipelineTranscript) -> Result<(), String> {
            self.saved_history
                .lock()
                .expect("saved-history lock should not be poisoned")
                .push(transcript.clone());
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

    #[test]
    fn active_pipeline_session_id_returns_current_session_without_mutating_counter() {
        let runtime = PipelineRuntimeState::default();
        let session_id = runtime.begin_session();
        let next_before = runtime.next_session_id.load(Ordering::Relaxed);

        assert_eq!(active_pipeline_session_id(&runtime), Some(session_id));
        assert_eq!(runtime.next_session_id.load(Ordering::Relaxed), next_before);
    }

    #[test]
    fn active_pipeline_session_id_is_none_before_any_session_starts() {
        let runtime = PipelineRuntimeState::default();
        assert_eq!(active_pipeline_session_id(&runtime), None);
    }

    #[test]
    fn short_recordings_are_discarded_before_transcription() {
        assert!(crate::should_discard_recording(
            crate::MIN_RECORDING_DURATION_MS - 1
        ));
        assert!(!crate::should_discard_recording(
            crate::MIN_RECORDING_DURATION_MS
        ));
        assert!(!crate::should_discard_recording(
            crate::MIN_RECORDING_DURATION_MS + 1
        ));
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

    #[tokio::test]
    async fn insertion_failure_still_persists_history_entry() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = InsertionFailureDelegate::default();

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::TextInsertion,
                message: "accessibility denied".to_string(),
            }]
        );
        assert_eq!(delegate.transcripts(), vec!["hello world".to_string()]);
        assert_eq!(
            delegate.saved_history(),
            vec![PipelineTranscript {
                text: "hello world".to_string(),
                duration_secs: Some(2.4),
                language: Some("en".to_string()),
                provider: "test".to_string(),
            }]
        );
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

    #[test]
    fn cancel_recording_resets_to_idle_after_abort_attempt() {
        let call_order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses: Arc<Mutex<Vec<AppStatus>>> = Arc::new(Mutex::new(Vec::new()));

        let begin_call_order = Arc::clone(&call_order);
        let stop_call_order = Arc::clone(&call_order);
        let abort_call_order = Arc::clone(&call_order);
        let status_call_order = Arc::clone(&call_order);
        let status_log = Arc::clone(&statuses);

        let result = cancel_recording_with_hooks(
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
                Ok(true)
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
        );

        assert_eq!(result, Ok(true));
        assert_eq!(
            call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .clone(),
            vec![
                "begin_session",
                "force_stop_recording",
                "abort_recording",
                "set_status"
            ]
        );
        assert_eq!(
            statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone(),
            vec![AppStatus::Idle]
        );
    }

    #[test]
    fn cancel_recording_still_resets_to_idle_when_abort_fails() {
        let call_order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses: Arc<Mutex<Vec<AppStatus>>> = Arc::new(Mutex::new(Vec::new()));

        let begin_call_order = Arc::clone(&call_order);
        let stop_call_order = Arc::clone(&call_order);
        let abort_call_order = Arc::clone(&call_order);
        let status_call_order = Arc::clone(&call_order);
        let status_log = Arc::clone(&statuses);

        let result = cancel_recording_with_hooks(
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
                Err("abort failure".to_string())
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
        );

        assert_eq!(result, Err("abort failure".to_string()));
        assert_eq!(
            call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .clone(),
            vec![
                "begin_session",
                "force_stop_recording",
                "abort_recording",
                "set_status"
            ]
        );
        assert_eq!(
            statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone(),
            vec![AppStatus::Idle]
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

    #[test]
    fn microphone_not_determined_preflight_error_mentions_system_settings_path() {
        let message = permission_preflight_error_message(
            PermissionType::Microphone,
            PermissionState::NotDetermined,
            "start recording",
        );

        assert!(message.contains("cannot start recording"));
        assert!(message.contains("Privacy & Security > Microphone"));
    }

    #[test]
    fn accessibility_preflight_error_mentions_accessibility_settings_path() {
        let message = permission_preflight_error_message(
            PermissionType::Accessibility,
            PermissionState::Denied,
            "insert text",
        );

        assert!(message.contains("Accessibility access is required"));
        assert!(message.contains("Privacy & Security > Accessibility"));
    }

    #[test]
    fn startup_restore_applies_persisted_hotkey_configuration() {
        let settings = VoiceSettings {
            hotkey_shortcut: "Cmd+Shift+Space".to_string(),
            recording_mode: RECORDING_MODE_TOGGLE.to_string(),
            ..VoiceSettings::default()
        };
        let mut applied = Vec::new();
        let mut default_fallback_calls = 0usize;

        apply_hotkey_from_settings_with_fallback(
            &settings,
            |config| {
                applied.push(config);
                Ok(())
            },
            || {
                default_fallback_calls += 1;
                Ok(())
            },
        )
        .expect("startup hotkey restoration should succeed");

        assert_eq!(
            applied,
            vec![HotkeyConfig {
                shortcut: "Cmd+Shift+Space".to_string(),
                mode: RecordingMode::Toggle,
            }]
        );
        assert_eq!(default_fallback_calls, 0);
    }

    #[test]
    fn startup_restore_applies_double_tap_toggle_mode() {
        let settings = VoiceSettings {
            hotkey_shortcut: "F6".to_string(),
            recording_mode: RECORDING_MODE_DOUBLE_TAP_TOGGLE.to_string(),
            ..VoiceSettings::default()
        };
        let mut applied = Vec::new();

        apply_hotkey_from_settings_with_fallback(
            &settings,
            |config| {
                applied.push(config);
                Ok(())
            },
            || panic!("double tap toggle mode should not require fallback"),
        )
        .expect("startup restoration should accept double tap toggle mode");

        assert_eq!(
            applied,
            vec![HotkeyConfig {
                shortcut: "F6".to_string(),
                mode: RecordingMode::DoubleTapToggle,
            }]
        );
    }

    #[test]
    fn startup_restore_falls_back_to_default_when_persisted_mode_is_invalid() {
        let settings = VoiceSettings {
            hotkey_shortcut: "Cmd+Shift+Space".to_string(),
            recording_mode: "invalid_mode".to_string(),
            ..VoiceSettings::default()
        };
        let mut apply_attempts = 0usize;
        let mut default_fallback_calls = 0usize;

        apply_hotkey_from_settings_with_fallback(
            &settings,
            |_config| {
                apply_attempts += 1;
                Ok(())
            },
            || {
                default_fallback_calls += 1;
                Ok(())
            },
        )
        .expect("startup fallback should recover from invalid mode");

        assert_eq!(apply_attempts, 0);
        assert_eq!(default_fallback_calls, 1);
    }

    #[test]
    fn startup_restore_falls_back_to_default_when_persisted_shortcut_apply_fails() {
        let settings = VoiceSettings {
            hotkey_shortcut: "Cmd+Shift+Space".to_string(),
            recording_mode: RECORDING_MODE_TOGGLE.to_string(),
            ..VoiceSettings::default()
        };
        let mut apply_attempts = 0usize;
        let mut default_fallback_calls = 0usize;

        apply_hotkey_from_settings_with_fallback(
            &settings,
            |_config| {
                apply_attempts += 1;
                Err("shortcut already registered".to_string())
            },
            || {
                default_fallback_calls += 1;
                Ok(())
            },
        )
        .expect("startup fallback should recover from persisted hotkey apply failure");

        assert_eq!(apply_attempts, 1);
        assert_eq!(default_fallback_calls, 1);
    }

    #[test]
    fn startup_settings_load_falls_back_to_defaults_when_load_errors() {
        let mut load_attempts = 0usize;
        let settings = load_startup_settings_with_fallback(|| {
            load_attempts += 1;
            Err("Failed to parse settings file `/tmp/settings.json`: malformed".to_string())
        });

        assert_eq!(load_attempts, 1);
        assert_eq!(settings, VoiceSettings::default());
    }

    #[test]
    fn apply_settings_rolls_back_hotkey_when_launch_at_login_apply_fails() {
        let previous_hotkey = HotkeyConfig {
            shortcut: "Alt+Space".to_string(),
            mode: RecordingMode::HoldToTalk,
        };
        let requested_hotkey = HotkeyConfig {
            shortcut: "Ctrl+Space".to_string(),
            mode: RecordingMode::Toggle,
        };
        let update = VoiceSettingsUpdate {
            microphone_id: Some(Some("mic-42".to_string())),
            launch_at_login: Some(true),
            ..VoiceSettingsUpdate::default()
        };
        let mut applied_hotkeys = Vec::new();
        let mut rollback_hotkeys = Vec::new();
        let mut launch_apply_attempts = Vec::new();

        let error = apply_settings_transaction_with_hooks(
            update,
            previous_hotkey.clone(),
            requested_hotkey.clone(),
            false,
            true,
            |config| {
                applied_hotkeys.push(config.clone());
                Ok(config)
            },
            |enabled| {
                launch_apply_attempts.push(enabled);
                Err("launchctl denied".to_string())
            },
            |_update| panic!("settings should not be persisted when launch-at-login apply fails"),
            |_enabled| panic!("launch-at-login rollback should not run before a successful apply"),
            |config| {
                rollback_hotkeys.push(config.clone());
                Ok(config)
            },
        )
        .expect_err("launch-at-login apply failure should return an error");

        assert_eq!(applied_hotkeys, vec![requested_hotkey]);
        assert_eq!(launch_apply_attempts, vec![true]);
        assert_eq!(rollback_hotkeys, vec![previous_hotkey]);
        assert!(error.contains("Failed to apply launch-at-login setting: launchctl denied"));
    }

    #[test]
    fn apply_settings_persists_effective_hotkey_configuration_from_apply_result() {
        let previous_hotkey = HotkeyConfig {
            shortcut: "Alt+Space".to_string(),
            mode: RecordingMode::HoldToTalk,
        };
        let requested_hotkey = HotkeyConfig {
            shortcut: " Ctrl+Space ".to_string(),
            mode: RecordingMode::HoldToTalk,
        };
        let update = VoiceSettingsUpdate {
            microphone_id: Some(Some("mic-42".to_string())),
            auto_insert: Some(false),
            ..VoiceSettingsUpdate::default()
        };
        let applied_hotkey = HotkeyConfig {
            shortcut: "Ctrl+Space".to_string(),
            mode: RecordingMode::Toggle,
        };
        let mut persisted_updates = Vec::new();
        let mut rollback_launch_attempts = 0usize;
        let mut rollback_hotkey_attempts = 0usize;

        let persisted = apply_settings_transaction_with_hooks(
            update,
            previous_hotkey,
            requested_hotkey,
            false,
            false,
            |_config| Ok(applied_hotkey.clone()),
            |_enabled| Ok(()),
            |persist_update| {
                persisted_updates.push(persist_update.clone());
                Ok(VoiceSettings {
                    hotkey_shortcut: persist_update
                        .hotkey_shortcut
                        .expect("effective shortcut should be persisted"),
                    recording_mode: persist_update
                        .recording_mode
                        .expect("effective recording mode should be persisted"),
                    microphone_id: persist_update.microphone_id.unwrap_or(None),
                    auto_insert: persist_update.auto_insert.unwrap_or(true),
                    launch_at_login: persist_update.launch_at_login.unwrap_or(false),
                    ..VoiceSettings::default()
                })
            },
            |_enabled| {
                rollback_launch_attempts += 1;
                Err("launch rollback should not run for successful transaction".to_string())
            },
            |_config| {
                rollback_hotkey_attempts += 1;
                Err("hotkey rollback should not run for successful transaction".to_string())
            },
        )
        .expect("settings transaction should persist applied hotkey config");

        assert_eq!(persisted_updates.len(), 1);
        let persisted_update = persisted_updates
            .pop()
            .expect("persist update should be captured");
        assert_eq!(
            persisted_update.hotkey_shortcut.as_deref(),
            Some("Ctrl+Space")
        );
        assert_eq!(
            persisted_update.recording_mode.as_deref(),
            Some(RECORDING_MODE_TOGGLE)
        );
        assert_eq!(
            persisted_update.microphone_id,
            Some(Some("mic-42".to_string()))
        );
        assert_eq!(persisted_update.auto_insert, Some(false));
        assert_eq!(persisted_update.launch_at_login, None);
        assert_eq!(persisted.hotkey_shortcut, "Ctrl+Space");
        assert_eq!(persisted.recording_mode, RECORDING_MODE_TOGGLE);
        assert_eq!(persisted.microphone_id.as_deref(), Some("mic-42"));
        assert!(!persisted.auto_insert);
        assert_eq!(rollback_launch_attempts, 0);
        assert_eq!(rollback_hotkey_attempts, 0);
    }

    #[test]
    fn apply_settings_rolls_back_launch_at_login_and_hotkey_when_settings_persist_fails() {
        let previous_hotkey = HotkeyConfig {
            shortcut: "Alt+Space".to_string(),
            mode: RecordingMode::HoldToTalk,
        };
        let requested_hotkey = HotkeyConfig {
            shortcut: "Ctrl+Space".to_string(),
            mode: RecordingMode::Toggle,
        };
        let update = VoiceSettingsUpdate {
            launch_at_login: Some(true),
            ..VoiceSettingsUpdate::default()
        };
        let mut rollback_launch_states = Vec::new();
        let mut rollback_hotkeys = Vec::new();

        let error = apply_settings_transaction_with_hooks(
            update,
            previous_hotkey.clone(),
            requested_hotkey.clone(),
            false,
            true,
            |config| Ok(config),
            |_enabled| Ok(()),
            |_update| Err("disk full".to_string()),
            |enabled| {
                rollback_launch_states.push(enabled);
                Ok(())
            },
            |config| {
                rollback_hotkeys.push(config.clone());
                Ok(config)
            },
        )
        .expect_err("persist failure should trigger both launch-at-login and hotkey rollbacks");

        assert!(error.contains("Failed to persist settings: disk full"));
        assert_eq!(rollback_launch_states, vec![false]);
        assert_eq!(rollback_hotkeys, vec![previous_hotkey]);
    }

    #[test]
    fn apply_settings_reports_rollback_failures_after_persist_error() {
        let previous_hotkey = HotkeyConfig {
            shortcut: "Alt+Space".to_string(),
            mode: RecordingMode::HoldToTalk,
        };
        let requested_hotkey = HotkeyConfig {
            shortcut: "Ctrl+Space".to_string(),
            mode: RecordingMode::Toggle,
        };
        let update = VoiceSettingsUpdate {
            launch_at_login: Some(true),
            ..VoiceSettingsUpdate::default()
        };

        let error = apply_settings_transaction_with_hooks(
            update,
            previous_hotkey,
            requested_hotkey,
            false,
            true,
            Ok,
            |_enabled| Ok(()),
            |_update| Err("disk full".to_string()),
            |_enabled| Err("launch rollback failed".to_string()),
            |_config| Err("rollback shortcut registration failed".to_string()),
        )
        .expect_err("persist failure with rollback failures should return combined error");

        assert!(error.contains("Failed to persist settings: disk full"));
        assert!(error.contains("Failed to roll back launch-at-login state"));
        assert!(error.contains("Failed to roll back hotkey config"));
    }

    #[test]
    fn copy_directory_contents_copies_nested_files() {
        let temp_dir = TempDirGuard::new("voice-copy-directory-contents");
        let source_dir = temp_dir.path().join("source");
        let destination_dir = temp_dir.path().join("destination");
        let nested_source_dir = source_dir.join("nested");
        std::fs::create_dir_all(&nested_source_dir).expect("nested source directory should exist");
        std::fs::write(source_dir.join("settings.json"), b"{\"autoInsert\":true}")
            .expect("settings should be written");
        std::fs::write(nested_source_dir.join("history.json"), b"[]")
            .expect("history should be written");

        copy_directory_contents(&source_dir, &destination_dir)
            .expect("copy helper should copy source directory contents");

        assert_eq!(
            std::fs::read(destination_dir.join("settings.json"))
                .expect("destination settings should exist"),
            b"{\"autoInsert\":true}"
        );
        assert_eq!(
            std::fs::read(destination_dir.join("nested").join("history.json"))
                .expect("destination history should exist"),
            b"[]"
        );
    }

    #[test]
    fn migrate_legacy_app_data_dir_copies_when_legacy_exists_and_new_is_missing() {
        let temp_dir = TempDirGuard::new("voice-legacy-app-data-migration");
        let app_support_dir = temp_dir.path().join("Application Support");
        let legacy_app_data_dir = app_support_dir.join("com.sawyerhood.voice");
        let new_app_data_dir = app_support_dir.join("com.sawyerhood.buzz");
        std::fs::create_dir_all(legacy_app_data_dir.join("nested"))
            .expect("legacy app data directory should exist");
        std::fs::write(legacy_app_data_dir.join("api-key.json"), b"secret")
            .expect("legacy api key should be written");
        std::fs::write(
            legacy_app_data_dir.join("nested").join("stats.json"),
            b"{\"words\":42}",
        )
        .expect("legacy stats should be written");

        migrate_legacy_app_data_dir(&new_app_data_dir);

        assert_eq!(
            std::fs::read(new_app_data_dir.join("api-key.json"))
                .expect("new app data should include copied api key"),
            b"secret"
        );
        assert_eq!(
            std::fs::read(new_app_data_dir.join("nested").join("stats.json"))
                .expect("new app data should include copied nested stats"),
            b"{\"words\":42}"
        );
        assert!(legacy_app_data_dir.exists());
    }

    #[test]
    fn migrate_legacy_app_data_dir_skips_when_new_directory_already_exists() {
        let temp_dir = TempDirGuard::new("voice-legacy-app-data-skip");
        let app_support_dir = temp_dir.path().join("Application Support");
        let legacy_app_data_dir = app_support_dir.join("com.sawyerhood.voice");
        let new_app_data_dir = app_support_dir.join("com.sawyerhood.buzz");
        std::fs::create_dir_all(&legacy_app_data_dir)
            .expect("legacy app data directory should exist");
        std::fs::write(legacy_app_data_dir.join("history.json"), b"[\"legacy\"]")
            .expect("legacy history should be written");
        std::fs::create_dir_all(&new_app_data_dir).expect("new app data directory should exist");
        std::fs::write(new_app_data_dir.join("history.json"), b"[\"new\"]")
            .expect("new history should be written");

        migrate_legacy_app_data_dir(&new_app_data_dir);

        assert_eq!(
            std::fs::read(new_app_data_dir.join("history.json"))
                .expect("new history should remain unchanged"),
            b"[\"new\"]"
        );
    }

    #[test]
    fn overlay_is_visible_while_listening_or_transcribing() {
        assert!(should_show_overlay_for_status(AppStatus::Listening));
        assert!(should_show_overlay_for_status(AppStatus::Transcribing));
        assert!(!should_show_overlay_for_status(AppStatus::Idle));
        assert!(!should_show_overlay_for_status(AppStatus::Error));
    }

    #[test]
    fn overlay_position_is_top_centered_in_work_area() {
        let position = overlay_position_from_work_area(PhysicalPosition::new(100, 32), 1600, 2.0);

        let expected_x = (100.0 / 2.0) + ((1600.0 / 2.0 - OVERLAY_WINDOW_WIDTH) / 2.0);
        let expected_y = (32.0 / 2.0) + OVERLAY_WINDOW_TOP_MARGIN;

        assert!((position.x - expected_x).abs() < f64::EPSILON);
        assert!((position.y - expected_y).abs() < f64::EPSILON);
    }

    #[test]
    fn has_api_key_command_contract_returns_boolean_presence_only() {
        let _: for<'a> fn(String, tauri::State<'a, AppState>) -> Result<bool, String> = has_api_key;
    }
}

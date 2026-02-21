use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
pub mod macos_event_tap;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tracing::{debug, error, info, warn};

pub const DEFAULT_SHORTCUT: &str = "Alt+Space";
pub const EVENT_HOTKEY_CONFIG_CHANGED: &str = "voice://hotkey-config-changed";
pub const EVENT_RECORDING_STATE_CHANGED: &str = "voice://recording-state-changed";
pub const EVENT_RECORDING_STARTED: &str = "voice://recording-started";
pub const EVENT_RECORDING_STOPPED: &str = "voice://recording-stopped";
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    HoldToTalk,
    Toggle,
    DoubleTapToggle,
}

impl Default for RecordingMode {
    fn default() -> Self {
        Self::HoldToTalk
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyConfig {
    pub shortcut: String,
    pub mode: RecordingMode,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            shortcut: DEFAULT_SHORTCUT.to_string(),
            mode: RecordingMode::HoldToTalk,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingTransition {
    Started,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopProcessingDecision {
    Ignore,
    DeferUntilStarted,
    AcknowledgeOnly,
    Process,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyTrigger {
    Pressed,
    Released,
}

impl From<ShortcutState> for HotkeyTrigger {
    fn from(value: ShortcutState) -> Self {
        match value {
            ShortcutState::Pressed => Self::Pressed,
            ShortcutState::Released => Self::Released,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingStateChangedEvent {
    pub is_recording: bool,
    pub mode: RecordingMode,
    pub shortcut: String,
    pub transition: RecordingTransition,
    pub trigger: HotkeyTrigger,
}

#[derive(Debug)]
struct HotkeyRuntimeState {
    config: HotkeyConfig,
    registered_shortcut: Option<String>,
    is_recording: bool,
    desired_recording: bool,
    pending_transitions: VecDeque<RecordingTransition>,
    last_press_timestamp: Option<Instant>,
}

impl Default for HotkeyRuntimeState {
    fn default() -> Self {
        Self {
            config: HotkeyConfig::default(),
            registered_shortcut: None,
            is_recording: false,
            desired_recording: false,
            pending_transitions: VecDeque::new(),
            last_press_timestamp: None,
        }
    }
}

impl HotkeyRuntimeState {
    fn apply_shortcut_event(
        &mut self,
        shortcut_state: ShortcutState,
    ) -> Option<RecordingTransition> {
        let (next_recording_state, transition) = resolve_transition_with_runtime_state(
            self.config.mode,
            self.desired_recording,
            shortcut_state,
            &mut self.last_press_timestamp,
            Instant::now(),
        )?;

        self.desired_recording = next_recording_state;
        self.pending_transitions.push_back(transition);
        Some(transition)
    }

    fn acknowledge_transition(&mut self, transition: RecordingTransition, success: bool) {
        if self.pending_transitions.front().copied() == Some(transition) {
            self.pending_transitions.pop_front();
        } else if let Some(index) = self
            .pending_transitions
            .iter()
            .position(|pending| *pending == transition)
        {
            self.pending_transitions.remove(index);
        }

        self.is_recording = match transition {
            RecordingTransition::Started => success,
            RecordingTransition::Stopped => false,
        };

        if matches!(transition, RecordingTransition::Stopped) || !self.is_recording {
            self.last_press_timestamp = None;
        }

        self.recompute_desired_recording();
    }

    fn recompute_desired_recording(&mut self) {
        let mut desired_recording = self.is_recording;
        for pending_transition in &self.pending_transitions {
            desired_recording = matches!(pending_transition, RecordingTransition::Started);
        }

        self.desired_recording = desired_recording;
    }

    fn clear_registered_shortcut(&mut self) {
        self.registered_shortcut = None;
        self.is_recording = false;
        self.desired_recording = false;
        self.pending_transitions.clear();
        self.last_press_timestamp = None;
    }

    fn stop_processing_decision(&self) -> StopProcessingDecision {
        let Some(stop_index) = self
            .pending_transitions
            .iter()
            .position(|transition| matches!(transition, RecordingTransition::Stopped))
        else {
            return StopProcessingDecision::Ignore;
        };

        let has_start_before_stop = self
            .pending_transitions
            .iter()
            .take(stop_index)
            .any(|transition| matches!(transition, RecordingTransition::Started));

        if has_start_before_stop {
            return StopProcessingDecision::DeferUntilStarted;
        }

        if self.is_recording {
            StopProcessingDecision::Process
        } else {
            StopProcessingDecision::AcknowledgeOnly
        }
    }
}

#[derive(Debug, Clone)]
pub struct HotkeyService {
    state: Arc<Mutex<HotkeyRuntimeState>>,
}

impl Default for HotkeyService {
    fn default() -> Self {
        Self::new()
    }
}

impl HotkeyService {
    pub fn new() -> Self {
        debug!("hotkey service initialized");
        Self {
            state: Arc::new(Mutex::new(HotkeyRuntimeState::default())),
        }
    }

    pub fn register_default_shortcut<R: Runtime>(&self, app: &AppHandle<R>) -> Result<(), String> {
        info!(
            shortcut = DEFAULT_SHORTCUT,
            "registering default hotkey shortcut"
        );
        self.apply_config(app, HotkeyConfig::default()).map(|_| ())
    }

    pub fn current_config(&self) -> HotkeyConfig {
        self.state
            .lock()
            .map(|state| state.config.clone())
            .unwrap_or_else(|_| HotkeyConfig::default())
    }

    pub fn is_recording(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.is_recording)
            .unwrap_or(false)
    }

    pub fn acknowledge_transition(&self, transition: RecordingTransition, success: bool) {
        if let Ok(mut state) = self.state.lock() {
            debug!(?transition, success, "acknowledging hotkey transition");
            state.acknowledge_transition(transition, success);
        } else {
            error!("hotkey state lock poisoned while acknowledging transition");
        }
    }

    pub fn stop_processing_decision(&self) -> StopProcessingDecision {
        self.state
            .lock()
            .map(|state| state.stop_processing_decision())
            .unwrap_or(StopProcessingDecision::Ignore)
    }

    pub fn force_stop_recording<R: Runtime>(&self, app: &AppHandle<R>) -> bool {
        let payload = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    error!("hotkey state lock poisoned while forcing stop");
                    return false;
                }
            };

            let was_active = state.is_recording
                || state.desired_recording
                || !state.pending_transitions.is_empty();

            if !was_active {
                debug!("force stop requested while not recording");
                return false;
            }

            state.is_recording = false;
            state.desired_recording = false;
            state.pending_transitions.clear();

            RecordingStateChangedEvent {
                is_recording: false,
                mode: state.config.mode,
                shortcut: state.config.shortcut.clone(),
                transition: RecordingTransition::Stopped,
                trigger: HotkeyTrigger::Released,
            }
        };

        info!(
            mode = ?payload.mode,
            shortcut = %payload.shortcut,
            "forced hotkey recording stop"
        );
        if let Err(error) = app.emit(EVENT_RECORDING_STATE_CHANGED, &payload) {
            warn!(%error, "failed to emit recording state change after force stop");
        }
        true
    }

    pub fn apply_config<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        config: HotkeyConfig,
    ) -> Result<HotkeyConfig, String> {
        info!(
            shortcut = %config.shortcut,
            mode = ?config.mode,
            "applying hotkey configuration"
        );
        let service = self.clone();
        apply_config_with_registrar(
            &self.state,
            config,
            |shortcut| {
                app.global_shortcut()
                    .unregister(shortcut)
                    .map_err(|error| error.to_string())
            },
            |shortcut| {
                let callback_service = service.clone();
                app.global_shortcut()
                    .on_shortcut(shortcut, move |app, _shortcut, event| {
                        callback_service.handle_shortcut_event(app, event.state);
                    })
                    .map_err(|error| error.to_string())
            },
            |config| emit_hotkey_config_changed(app, config),
        )
    }

    fn handle_shortcut_event<R: Runtime>(&self, app: &AppHandle<R>, shortcut_state: ShortcutState) {
        let event_payload = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => {
                    error!("hotkey state lock poisoned while handling shortcut event");
                    return;
                }
            };

            let transition = match state.apply_shortcut_event(shortcut_state) {
                Some(transition) => transition,
                None => {
                    debug!(
                        ?shortcut_state,
                        "ignoring shortcut event with no state transition"
                    );
                    return;
                }
            };

            RecordingStateChangedEvent {
                is_recording: state.is_recording,
                mode: state.config.mode,
                shortcut: state.config.shortcut.clone(),
                transition,
                trigger: shortcut_state.into(),
            }
        };

        info!(
            transition = ?event_payload.transition,
            trigger = ?event_payload.trigger,
            mode = ?event_payload.mode,
            is_recording = event_payload.is_recording,
            shortcut = %event_payload.shortcut,
            "hotkey transition emitted"
        );
        if let Err(error) = app.emit(EVENT_RECORDING_STATE_CHANGED, &event_payload) {
            warn!(%error, "failed to emit recording state change event");
        }

        match event_payload.transition {
            RecordingTransition::Started => {
                if let Err(error) = app.emit(EVENT_RECORDING_STARTED, &event_payload) {
                    warn!(%error, "failed to emit recording started event");
                }
            }
            RecordingTransition::Stopped => {
                if let Err(error) = app.emit(EVENT_RECORDING_STOPPED, &event_payload) {
                    warn!(%error, "failed to emit recording stopped event");
                }
            }
        }
    }
}

fn apply_config_with_registrar<FU, FR, FE>(
    state: &Arc<Mutex<HotkeyRuntimeState>>,
    config: HotkeyConfig,
    mut unregister_shortcut: FU,
    mut register_shortcut: FR,
    mut emit_config_changed: FE,
) -> Result<HotkeyConfig, String>
where
    FU: FnMut(&str) -> Result<(), String>,
    FR: FnMut(&str) -> Result<(), String>,
    FE: FnMut(&HotkeyConfig),
{
    let next_config = normalize_config(config);
    debug!(
        shortcut = %next_config.shortcut,
        mode = ?next_config.mode,
        "normalized hotkey configuration"
    );
    validate_shortcut(&next_config.shortcut)?;

    let current_shortcut = {
        let state = state.lock().map_err(|_| lock_error())?;
        state.registered_shortcut.clone()
    };

    if current_shortcut
        .as_deref()
        .is_some_and(|registered| shortcuts_match(registered, next_config.shortcut.as_str()))
    {
        debug!(
            shortcut = %next_config.shortcut,
            "hotkey already registered; updating mode only"
        );
        let mut state = state.lock().map_err(|_| lock_error())?;
        state.config = next_config.clone();
        state.last_press_timestamp = None;
        drop(state);
        emit_config_changed(&next_config);
        return Ok(next_config);
    }

    let previous_shortcut = current_shortcut.clone();

    if let Some(registered_shortcut) = current_shortcut {
        debug!(shortcut = %registered_shortcut, "unregistering previous hotkey shortcut");
        unregister_shortcut(registered_shortcut.as_str()).map_err(|error| {
            format!("Failed to unregister hotkey `{registered_shortcut}`: {error}")
        })?;
    }

    if let Err(error) = register_shortcut(next_config.shortcut.as_str()) {
        warn!(
            shortcut = %next_config.shortcut,
            %error,
            "failed to register new hotkey shortcut; attempting rollback"
        );
        let register_error = error;
        let mut restore_error: Option<String> = None;

        if let Some(previous_shortcut) = previous_shortcut.as_deref() {
            if let Err(error) = register_shortcut(previous_shortcut) {
                restore_error = Some(error);
            }
        }

        if should_clear_registered_shortcut_after_failed_registration(
            previous_shortcut.as_deref(),
            restore_error.as_deref(),
        ) {
            if let Ok(mut state) = state.lock() {
                state.clear_registered_shortcut();
            }
        }

        return Err(format_registration_failure(
            next_config.shortcut.as_str(),
            register_error.as_str(),
            previous_shortcut.as_deref(),
            restore_error.as_deref(),
        ));
    }

    {
        let mut state = state.lock().map_err(|_| lock_error())?;
        state.config = next_config.clone();
        state.registered_shortcut = Some(next_config.shortcut.clone());
        state.is_recording = false;
        state.desired_recording = false;
        state.pending_transitions.clear();
        state.last_press_timestamp = None;
    }

    info!(
        shortcut = %next_config.shortcut,
        mode = ?next_config.mode,
        "hotkey configuration applied"
    );
    emit_config_changed(&next_config);
    Ok(next_config)
}

#[tauri::command]
pub fn get_hotkey_config(service: State<'_, HotkeyService>) -> HotkeyConfig {
    service.current_config()
}

#[tauri::command]
pub fn get_hotkey_recording_state(service: State<'_, HotkeyService>) -> bool {
    service.is_recording()
}

#[tauri::command]
pub fn set_hotkey_config(
    app: AppHandle,
    service: State<'_, HotkeyService>,
    config: HotkeyConfig,
) -> Result<HotkeyConfig, String> {
    service.apply_config(&app, config)
}

fn normalize_config(mut config: HotkeyConfig) -> HotkeyConfig {
    let trimmed_shortcut = config.shortcut.trim();
    config.shortcut = if trimmed_shortcut.is_empty() {
        DEFAULT_SHORTCUT.to_string()
    } else {
        trimmed_shortcut.to_string()
    };

    config
}

fn validate_shortcut(shortcut: &str) -> Result<(), String> {
    shortcut
        .parse::<Shortcut>()
        .map(|_| ())
        .map_err(|error| format!("Invalid hotkey `{shortcut}`: {error}"))
}

fn shortcuts_match(left: &str, right: &str) -> bool {
    match (left.parse::<Shortcut>(), right.parse::<Shortcut>()) {
        (Ok(left_shortcut), Ok(right_shortcut)) => left_shortcut.id() == right_shortcut.id(),
        _ => left.eq_ignore_ascii_case(right),
    }
}

fn resolve_transition(
    mode: RecordingMode,
    is_recording: bool,
    shortcut_state: ShortcutState,
) -> Option<(bool, RecordingTransition)> {
    match mode {
        RecordingMode::HoldToTalk => match shortcut_state {
            ShortcutState::Pressed if !is_recording => Some((true, RecordingTransition::Started)),
            ShortcutState::Released if is_recording => Some((false, RecordingTransition::Stopped)),
            _ => None,
        },
        RecordingMode::Toggle => match shortcut_state {
            ShortcutState::Pressed if is_recording => Some((false, RecordingTransition::Stopped)),
            ShortcutState::Pressed => Some((true, RecordingTransition::Started)),
            ShortcutState::Released => None,
        },
        RecordingMode::DoubleTapToggle => None,
    }
}

fn resolve_transition_with_runtime_state(
    mode: RecordingMode,
    is_recording: bool,
    shortcut_state: ShortcutState,
    last_press_timestamp: &mut Option<Instant>,
    now: Instant,
) -> Option<(bool, RecordingTransition)> {
    match mode {
        RecordingMode::DoubleTapToggle => {
            resolve_double_tap_transition(is_recording, shortcut_state, last_press_timestamp, now)
        }
        _ => resolve_transition(mode, is_recording, shortcut_state),
    }
}

fn resolve_double_tap_transition(
    is_recording: bool,
    shortcut_state: ShortcutState,
    last_press_timestamp: &mut Option<Instant>,
    now: Instant,
) -> Option<(bool, RecordingTransition)> {
    if !matches!(shortcut_state, ShortcutState::Pressed) {
        return None;
    }

    if is_recording {
        *last_press_timestamp = None;
        return Some((false, RecordingTransition::Stopped));
    }

    let is_double_tap = last_press_timestamp
        .map(|timestamp| now.saturating_duration_since(timestamp) <= DOUBLE_TAP_WINDOW)
        .unwrap_or(false);

    *last_press_timestamp = Some(now);

    if is_double_tap {
        *last_press_timestamp = None;
        Some((true, RecordingTransition::Started))
    } else {
        None
    }
}

fn should_clear_registered_shortcut_after_failed_registration(
    previous_shortcut: Option<&str>,
    restore_error: Option<&str>,
) -> bool {
    previous_shortcut.is_none() || restore_error.is_some()
}

fn format_registration_failure(
    attempted_shortcut: &str,
    register_error: &str,
    previous_shortcut: Option<&str>,
    restore_error: Option<&str>,
) -> String {
    match (previous_shortcut, restore_error) {
        (Some(previous_shortcut), Some(restore_error)) => format!(
            "Failed to register global hotkey `{attempted_shortcut}`: {register_error}. Failed to restore previous hotkey `{previous_shortcut}`: {restore_error}. No global hotkey is currently registered."
        ),
        (Some(previous_shortcut), None) => format!(
            "Failed to register global hotkey `{attempted_shortcut}`: {register_error}. Previous hotkey `{previous_shortcut}` remains registered."
        ),
        (None, _) => format!(
            "Failed to register global hotkey `{attempted_shortcut}`: {register_error}"
        ),
    }
}

fn emit_hotkey_config_changed<R: Runtime>(app: &AppHandle<R>, config: &HotkeyConfig) {
    if let Err(error) = app.emit(EVENT_HOTKEY_CONFIG_CHANGED, config) {
        warn!(%error, "failed to emit hotkey config changed event");
    }
}

fn lock_error() -> String {
    "Hotkey service state lock was poisoned".to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Mutex,
        time::{Duration, Instant},
    };

    use async_trait::async_trait;

    use crate::{
        status_notifier::AppStatus,
        voice_pipeline::{
            PipelineError, PipelineErrorStage, PipelineTranscript, VoicePipeline,
            VoicePipelineDelegate,
        },
    };

    use super::*;

    #[derive(Debug)]
    struct StartFailurePipelineDelegate {
        hotkey_state: Mutex<HotkeyRuntimeState>,
        statuses: Mutex<Vec<AppStatus>>,
        errors: Mutex<Vec<PipelineError>>,
    }

    impl StartFailurePipelineDelegate {
        fn new_with_pending_start() -> Self {
            let mut hotkey_state = HotkeyRuntimeState::default();
            hotkey_state.apply_shortcut_event(ShortcutState::Pressed);

            Self {
                hotkey_state: Mutex::new(hotkey_state),
                statuses: Mutex::new(Vec::new()),
                errors: Mutex::new(Vec::new()),
            }
        }

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
    impl VoicePipelineDelegate for StartFailurePipelineDelegate {
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

        fn on_recording_started(&self, success: bool) {
            let mut hotkey_state = self
                .hotkey_state
                .lock()
                .expect("hotkey state lock should not be poisoned");
            hotkey_state.acknowledge_transition(RecordingTransition::Started, success);
        }

        fn start_recording(&self) -> Result<(), String> {
            Err("microphone unavailable".to_string())
        }

        fn stop_recording(&self) -> Result<Vec<u8>, String> {
            panic!("stop should not be called for start failure scenario");
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
            panic!("transcribe should not be called for start failure scenario");
        }

        fn insert_text(&self, _transcript: &str) -> Result<(), String> {
            panic!("insert_text should not be called for start failure scenario");
        }
    }

    #[test]
    fn default_config_is_hold_to_talk_with_option_space() {
        let config = HotkeyConfig::default();
        assert_eq!(config.shortcut, DEFAULT_SHORTCUT);
        assert_eq!(config.mode, RecordingMode::HoldToTalk);
    }

    #[test]
    fn normalize_config_uses_default_shortcut_when_blank() {
        let config = HotkeyConfig {
            shortcut: "   ".to_string(),
            mode: RecordingMode::Toggle,
        };

        let normalized = normalize_config(config);

        assert_eq!(normalized.shortcut, DEFAULT_SHORTCUT);
        assert_eq!(normalized.mode, RecordingMode::Toggle);
    }

    #[test]
    fn validate_shortcut_accepts_expected_format_and_rejects_invalid_values() {
        assert!(validate_shortcut(DEFAULT_SHORTCUT).is_ok());
        assert!(validate_shortcut("not-a-shortcut").is_err());
    }

    #[test]
    fn hold_to_talk_transitions_on_press_and_release_only() {
        assert_eq!(
            resolve_transition(RecordingMode::HoldToTalk, false, ShortcutState::Pressed),
            Some((true, RecordingTransition::Started))
        );

        assert_eq!(
            resolve_transition(RecordingMode::HoldToTalk, true, ShortcutState::Released),
            Some((false, RecordingTransition::Stopped))
        );

        assert_eq!(
            resolve_transition(RecordingMode::HoldToTalk, false, ShortcutState::Released),
            None
        );
    }

    #[test]
    fn toggle_mode_transitions_on_pressed_only() {
        assert_eq!(
            resolve_transition(RecordingMode::Toggle, false, ShortcutState::Pressed),
            Some((true, RecordingTransition::Started))
        );

        assert_eq!(
            resolve_transition(RecordingMode::Toggle, true, ShortcutState::Pressed),
            Some((false, RecordingTransition::Stopped))
        );

        assert_eq!(
            resolve_transition(RecordingMode::Toggle, true, ShortcutState::Released),
            None
        );
    }

    #[test]
    fn double_tap_toggle_requires_two_quick_presses_to_start() {
        let mut last_press_timestamp = None;
        let first_press = Instant::now();

        assert_eq!(
            resolve_transition_with_runtime_state(
                RecordingMode::DoubleTapToggle,
                false,
                ShortcutState::Pressed,
                &mut last_press_timestamp,
                first_press
            ),
            None
        );
        assert_eq!(last_press_timestamp, Some(first_press));

        let second_press = first_press + (DOUBLE_TAP_WINDOW / 2);
        assert_eq!(
            resolve_transition_with_runtime_state(
                RecordingMode::DoubleTapToggle,
                false,
                ShortcutState::Pressed,
                &mut last_press_timestamp,
                second_press
            ),
            Some((true, RecordingTransition::Started))
        );
        assert_eq!(last_press_timestamp, None);
    }

    #[test]
    fn double_tap_toggle_ignores_slow_second_press() {
        let mut last_press_timestamp = Some(Instant::now());
        let next_press =
            last_press_timestamp.unwrap() + DOUBLE_TAP_WINDOW + Duration::from_millis(1);

        assert_eq!(
            resolve_transition_with_runtime_state(
                RecordingMode::DoubleTapToggle,
                false,
                ShortcutState::Pressed,
                &mut last_press_timestamp,
                next_press
            ),
            None
        );
        assert_eq!(last_press_timestamp, Some(next_press));
    }

    #[test]
    fn double_tap_toggle_stops_on_single_press_while_recording() {
        let mut last_press_timestamp = Some(Instant::now());

        assert_eq!(
            resolve_transition_with_runtime_state(
                RecordingMode::DoubleTapToggle,
                true,
                ShortcutState::Pressed,
                &mut last_press_timestamp,
                Instant::now()
            ),
            Some((false, RecordingTransition::Stopped))
        );
        assert_eq!(last_press_timestamp, None);

        assert_eq!(
            resolve_transition_with_runtime_state(
                RecordingMode::DoubleTapToggle,
                true,
                ShortcutState::Released,
                &mut last_press_timestamp,
                Instant::now()
            ),
            None
        );
    }

    #[test]
    fn shortcut_comparison_ignores_case_and_alias_formatting() {
        assert!(shortcuts_match("alt+space", "Alt+Space"));
    }

    #[test]
    fn acknowledge_started_failure_rolls_back_to_not_recording() {
        let mut state = HotkeyRuntimeState::default();

        assert_eq!(
            state.apply_shortcut_event(ShortcutState::Pressed),
            Some(RecordingTransition::Started)
        );
        assert!(!state.is_recording);
        assert!(state.desired_recording);

        state.acknowledge_transition(RecordingTransition::Started, false);

        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
    }

    #[test]
    fn acknowledge_started_success_marks_recording_as_confirmed() {
        let mut state = HotkeyRuntimeState::default();

        state.apply_shortcut_event(ShortcutState::Pressed);
        state.acknowledge_transition(RecordingTransition::Started, true);

        assert!(state.is_recording);
        assert!(state.desired_recording);
        assert!(state.pending_transitions.is_empty());
    }

    #[test]
    fn desired_state_recomputes_from_pending_transitions() {
        let mut state = HotkeyRuntimeState::default();

        state.apply_shortcut_event(ShortcutState::Pressed);
        state.apply_shortcut_event(ShortcutState::Released);
        state.acknowledge_transition(RecordingTransition::Started, false);

        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert_eq!(
            state.pending_transitions,
            VecDeque::from([RecordingTransition::Stopped])
        );
    }

    #[test]
    fn quick_release_defers_stop_until_start_ack_then_processes() {
        let mut state = HotkeyRuntimeState::default();

        state.apply_shortcut_event(ShortcutState::Pressed);
        state.apply_shortcut_event(ShortcutState::Released);

        assert_eq!(
            state.stop_processing_decision(),
            StopProcessingDecision::DeferUntilStarted
        );

        state.acknowledge_transition(RecordingTransition::Started, true);
        assert!(state.is_recording);
        assert_eq!(
            state.stop_processing_decision(),
            StopProcessingDecision::Process
        );

        state.acknowledge_transition(RecordingTransition::Stopped, true);
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
    }

    #[test]
    fn quick_release_after_start_failure_acknowledges_stop_without_processing() {
        let mut state = HotkeyRuntimeState::default();

        state.apply_shortcut_event(ShortcutState::Pressed);
        state.apply_shortcut_event(ShortcutState::Released);
        state.acknowledge_transition(RecordingTransition::Started, false);

        assert_eq!(
            state.stop_processing_decision(),
            StopProcessingDecision::AcknowledgeOnly
        );

        state.acknowledge_transition(RecordingTransition::Stopped, false);
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
    }

    #[tokio::test]
    async fn pipeline_start_failure_rolls_back_hotkey_state_and_reports_ui_error() {
        let delegate = StartFailurePipelineDelegate::new_with_pending_start();
        VoicePipeline::new(Duration::ZERO)
            .handle_hotkey_started(&delegate)
            .await;

        let state = delegate
            .hotkey_state
            .lock()
            .expect("hotkey state lock should not be poisoned");
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());

        assert_eq!(delegate.statuses(), vec![AppStatus::Error, AppStatus::Idle]);
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::RecordingStart,
                message: "microphone unavailable".to_string(),
            }]
        );
    }

    #[test]
    fn re_register_failure_with_restore_failure_clears_shortcut_state() {
        let state = Arc::new(Mutex::new(HotkeyRuntimeState {
            config: HotkeyConfig::default(),
            registered_shortcut: Some(DEFAULT_SHORTCUT.to_string()),
            is_recording: true,
            desired_recording: true,
            pending_transitions: VecDeque::from([RecordingTransition::Started]),
            last_press_timestamp: None,
        }));
        let mut unregister_attempts = Vec::new();
        let mut register_attempts = Vec::new();
        let mut emitted_configs = Vec::new();

        let result = apply_config_with_registrar(
            &state,
            HotkeyConfig {
                shortcut: "Ctrl+Space".to_string(),
                mode: RecordingMode::Toggle,
            },
            |shortcut| {
                unregister_attempts.push(shortcut.to_string());
                Ok(())
            },
            |shortcut| {
                register_attempts.push(shortcut.to_string());
                Err("registration failed".to_string())
            },
            |config| emitted_configs.push(config.clone()),
        );

        let error = result.expect_err("re-register should fail");
        assert!(error.contains("Failed to register global hotkey `Ctrl+Space`"));
        assert_eq!(unregister_attempts, vec![DEFAULT_SHORTCUT.to_string()]);
        assert_eq!(
            register_attempts,
            vec!["Ctrl+Space".to_string(), DEFAULT_SHORTCUT.to_string()]
        );
        assert!(emitted_configs.is_empty());

        let state = state
            .lock()
            .expect("hotkey state lock should not be poisoned");
        assert_eq!(state.config, HotkeyConfig::default());
        assert_eq!(state.registered_shortcut, None);
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
    }

    #[test]
    fn re_register_failure_with_successful_restore_keeps_previous_shortcut_state() {
        let state = Arc::new(Mutex::new(HotkeyRuntimeState {
            config: HotkeyConfig::default(),
            registered_shortcut: Some(DEFAULT_SHORTCUT.to_string()),
            is_recording: true,
            desired_recording: true,
            pending_transitions: VecDeque::from([RecordingTransition::Started]),
            last_press_timestamp: None,
        }));
        let mut unregister_attempts = Vec::new();
        let mut register_attempts = Vec::new();
        let mut emitted_configs = Vec::new();

        let result = apply_config_with_registrar(
            &state,
            HotkeyConfig {
                shortcut: "Ctrl+Space".to_string(),
                mode: RecordingMode::Toggle,
            },
            |shortcut| {
                unregister_attempts.push(shortcut.to_string());
                Ok(())
            },
            |shortcut| {
                register_attempts.push(shortcut.to_string());
                if shortcut == "Ctrl+Space" {
                    Err("registration failed".to_string())
                } else {
                    Ok(())
                }
            },
            |config| emitted_configs.push(config.clone()),
        );

        let error = result.expect_err("re-register should fail");
        assert!(error.contains("Failed to register global hotkey `Ctrl+Space`"));
        assert!(error.contains("Previous hotkey `Alt+Space` remains registered"));
        assert_eq!(unregister_attempts, vec![DEFAULT_SHORTCUT.to_string()]);
        assert_eq!(
            register_attempts,
            vec!["Ctrl+Space".to_string(), DEFAULT_SHORTCUT.to_string()]
        );
        assert!(emitted_configs.is_empty());

        let state = state
            .lock()
            .expect("hotkey state lock should not be poisoned");
        assert_eq!(state.config, HotkeyConfig::default());
        assert_eq!(state.registered_shortcut.as_deref(), Some(DEFAULT_SHORTCUT));
        assert!(state.is_recording);
        assert!(state.desired_recording);
        assert_eq!(
            state.pending_transitions,
            VecDeque::from([RecordingTransition::Started])
        );
    }

    #[test]
    fn clear_registered_shortcut_resets_runtime_flags() {
        let mut state = HotkeyRuntimeState {
            config: HotkeyConfig::default(),
            registered_shortcut: Some("Alt+Space".to_string()),
            is_recording: true,
            desired_recording: true,
            pending_transitions: VecDeque::from([RecordingTransition::Started]),
            last_press_timestamp: Some(Instant::now()),
        };

        state.clear_registered_shortcut();

        assert_eq!(state.registered_shortcut, None);
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
        assert_eq!(state.last_press_timestamp, None);
    }

    #[test]
    fn rollback_decision_clears_state_when_restore_fails_or_previous_missing() {
        assert!(should_clear_registered_shortcut_after_failed_registration(
            None, None
        ));
        assert!(should_clear_registered_shortcut_after_failed_registration(
            Some("Alt+Space"),
            Some("already registered")
        ));
        assert!(!should_clear_registered_shortcut_after_failed_registration(
            Some("Alt+Space"),
            None
        ));
    }

    #[test]
    fn registration_failure_message_reports_restore_failure_explicitly() {
        let message = format_registration_failure(
            "Ctrl+Shift+Space",
            "new shortcut rejected",
            Some("Alt+Space"),
            Some("restore rejected"),
        );

        assert!(message.contains("Failed to register global hotkey `Ctrl+Shift+Space`"));
        assert!(message.contains("Failed to restore previous hotkey `Alt+Space`"));
        assert!(message.contains("No global hotkey is currently registered"));
    }
}

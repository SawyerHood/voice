use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Runtime, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

pub const DEFAULT_SHORTCUT: &str = "Alt+Space";
pub const EVENT_HOTKEY_CONFIG_CHANGED: &str = "voice://hotkey-config-changed";
pub const EVENT_RECORDING_STATE_CHANGED: &str = "voice://recording-state-changed";
pub const EVENT_RECORDING_STARTED: &str = "voice://recording-started";
pub const EVENT_RECORDING_STOPPED: &str = "voice://recording-stopped";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingMode {
    HoldToTalk,
    Toggle,
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
}

impl Default for HotkeyRuntimeState {
    fn default() -> Self {
        Self {
            config: HotkeyConfig::default(),
            registered_shortcut: None,
            is_recording: false,
            desired_recording: false,
            pending_transitions: VecDeque::new(),
        }
    }
}

impl HotkeyRuntimeState {
    fn apply_shortcut_event(
        &mut self,
        shortcut_state: ShortcutState,
    ) -> Option<RecordingTransition> {
        let (next_recording_state, transition) =
            resolve_transition(self.config.mode, self.desired_recording, shortcut_state)?;

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
        Self {
            state: Arc::new(Mutex::new(HotkeyRuntimeState::default())),
        }
    }

    pub fn register_default_shortcut<R: Runtime>(&self, app: &AppHandle<R>) -> Result<(), String> {
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
            state.acknowledge_transition(transition, success);
        }
    }

    pub fn force_stop_recording<R: Runtime>(&self, app: &AppHandle<R>) -> bool {
        let payload = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return false,
            };

            let was_active = state.is_recording
                || state.desired_recording
                || !state.pending_transitions.is_empty();

            if !was_active {
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

        let _ = app.emit(EVENT_RECORDING_STATE_CHANGED, &payload);
        true
    }

    pub fn apply_config<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        config: HotkeyConfig,
    ) -> Result<HotkeyConfig, String> {
        let next_config = normalize_config(config);
        validate_shortcut(&next_config.shortcut)?;

        let current_shortcut = {
            let state = self.state.lock().map_err(|_| lock_error())?;
            state.registered_shortcut.clone()
        };

        if current_shortcut
            .as_deref()
            .is_some_and(|registered| shortcuts_match(registered, next_config.shortcut.as_str()))
        {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.config = next_config.clone();
            drop(state);
            emit_hotkey_config_changed(app, &next_config);
            return Ok(next_config);
        }

        let previous_shortcut = current_shortcut.clone();

        if let Some(registered_shortcut) = current_shortcut {
            app.global_shortcut()
                .unregister(registered_shortcut.as_str())
                .map_err(|error| {
                    format!("Failed to unregister hotkey `{registered_shortcut}`: {error}")
                })?;
        }

        let service = self.clone();
        let shortcut_to_register = next_config.shortcut.clone();

        let register_result = app.global_shortcut().on_shortcut(
            shortcut_to_register.as_str(),
            move |app, _shortcut, event| {
                service.handle_shortcut_event(app, event.state);
            },
        );

        if let Err(error) = register_result {
            let register_error = error.to_string();
            let mut restore_error: Option<String> = None;

            if let Some(previous_shortcut) = previous_shortcut.as_deref() {
                let restore_service = self.clone();
                if let Err(error) = app.global_shortcut().on_shortcut(
                    previous_shortcut,
                    move |app, _shortcut, event| {
                        restore_service.handle_shortcut_event(app, event.state);
                    },
                ) {
                    restore_error = Some(error.to_string());
                }
            }

            if should_clear_registered_shortcut_after_failed_registration(
                previous_shortcut.as_deref(),
                restore_error.as_deref(),
            ) {
                let mut state = self.state.lock().map_err(|_| lock_error())?;
                state.clear_registered_shortcut();
            }

            return Err(format_registration_failure(
                shortcut_to_register.as_str(),
                register_error.as_str(),
                previous_shortcut.as_deref(),
                restore_error.as_deref(),
            ));
        }

        {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.config = next_config.clone();
            state.registered_shortcut = Some(next_config.shortcut.clone());
            state.is_recording = false;
            state.desired_recording = false;
            state.pending_transitions.clear();
        }

        emit_hotkey_config_changed(app, &next_config);
        Ok(next_config)
    }

    fn handle_shortcut_event<R: Runtime>(&self, app: &AppHandle<R>, shortcut_state: ShortcutState) {
        let event_payload = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return,
            };

            let transition = match state.apply_shortcut_event(shortcut_state) {
                Some(transition) => transition,
                None => return,
            };

            RecordingStateChangedEvent {
                is_recording: state.is_recording,
                mode: state.config.mode,
                shortcut: state.config.shortcut.clone(),
                transition,
                trigger: shortcut_state.into(),
            }
        };

        let _ = app.emit(EVENT_RECORDING_STATE_CHANGED, &event_payload);

        match event_payload.transition {
            RecordingTransition::Started => {
                let _ = app.emit(EVENT_RECORDING_STARTED, &event_payload);
            }
            RecordingTransition::Stopped => {
                let _ = app.emit(EVENT_RECORDING_STOPPED, &event_payload);
            }
        }
    }
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
    let _ = app.emit(EVENT_HOTKEY_CONFIG_CHANGED, config);
}

fn lock_error() -> String {
    "Hotkey service state lock was poisoned".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn clear_registered_shortcut_resets_runtime_flags() {
        let mut state = HotkeyRuntimeState {
            config: HotkeyConfig::default(),
            registered_shortcut: Some("Alt+Space".to_string()),
            is_recording: true,
            desired_recording: true,
            pending_transitions: VecDeque::from([RecordingTransition::Started]),
        };

        state.clear_registered_shortcut();

        assert_eq!(state.registered_shortcut, None);
        assert!(!state.is_recording);
        assert!(!state.desired_recording);
        assert!(state.pending_transitions.is_empty());
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

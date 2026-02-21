use std::sync::{Arc, Mutex};

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
}

impl Default for HotkeyRuntimeState {
    fn default() -> Self {
        Self {
            config: HotkeyConfig::default(),
            registered_shortcut: None,
            is_recording: false,
        }
    }
}

impl HotkeyRuntimeState {
    fn apply_shortcut_event(
        &mut self,
        shortcut_state: ShortcutState,
    ) -> Option<RecordingTransition> {
        let (next_recording_state, transition) =
            resolve_transition(self.config.mode, self.is_recording, shortcut_state)?;

        self.is_recording = next_recording_state;
        Some(transition)
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

    pub fn force_stop_recording<R: Runtime>(&self, app: &AppHandle<R>) -> bool {
        let payload = {
            let mut state = match self.state.lock() {
                Ok(state) => state,
                Err(_) => return false,
            };

            if !state.is_recording {
                return false;
            }

            state.is_recording = false;
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
            if let Some(previous_shortcut) = previous_shortcut {
                let restore_service = self.clone();
                let _ = app.global_shortcut().on_shortcut(
                    previous_shortcut.as_str(),
                    move |app, _shortcut, event| {
                        restore_service.handle_shortcut_event(app, event.state);
                    },
                );
            }

            return Err(format!(
                "Failed to register global hotkey `{shortcut_to_register}`: {error}"
            ));
        }

        {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.config = next_config.clone();
            state.registered_shortcut = Some(next_config.shortcut.clone());
            state.is_recording = false;
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
}

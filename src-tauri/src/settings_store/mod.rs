use std::{
    fs,
    path::{Path, PathBuf},
    sync::RwLock,
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};

pub const DEFAULT_HOTKEY_SHORTCUT: &str = "Alt+Space";
pub const RECORDING_MODE_HOLD_TO_TALK: &str = "hold_to_talk";
pub const RECORDING_MODE_TOGGLE: &str = "toggle";
pub const DEFAULT_TRANSCRIPTION_PROVIDER: &str = "openai";

const SETTINGS_FILE_NAME: &str = "settings.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct VoiceSettings {
    pub hotkey_shortcut: String,
    pub recording_mode: String,
    pub microphone_id: Option<String>,
    pub language: Option<String>,
    pub transcription_provider: String,
    pub auto_insert: bool,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self {
            hotkey_shortcut: DEFAULT_HOTKEY_SHORTCUT.to_string(),
            recording_mode: RECORDING_MODE_HOLD_TO_TALK.to_string(),
            microphone_id: None,
            language: None,
            transcription_provider: DEFAULT_TRANSCRIPTION_PROVIDER.to_string(),
            auto_insert: true,
        }
    }
}

impl VoiceSettings {
    fn normalized(mut self) -> Result<Self, String> {
        self.hotkey_shortcut = normalize_required_string(self.hotkey_shortcut, "hotkey_shortcut")?;
        self.recording_mode = normalize_recording_mode(self.recording_mode)?;
        self.microphone_id = normalize_optional_string(self.microphone_id);
        self.language = normalize_optional_string(self.language);
        self.transcription_provider =
            normalize_required_string(self.transcription_provider, "transcription_provider")?
                .to_lowercase();

        Ok(self)
    }

    fn with_update(mut self, update: VoiceSettingsUpdate) -> Result<Self, String> {
        if let Some(hotkey_shortcut) = update.hotkey_shortcut {
            self.hotkey_shortcut = hotkey_shortcut;
        }

        if let Some(recording_mode) = update.recording_mode {
            self.recording_mode = recording_mode;
        }

        if let Some(microphone_id) = update.microphone_id {
            self.microphone_id = microphone_id;
        }

        if let Some(language) = update.language {
            self.language = language;
        }

        if let Some(transcription_provider) = update.transcription_provider {
            self.transcription_provider = transcription_provider;
        }

        if let Some(auto_insert) = update.auto_insert {
            self.auto_insert = auto_insert;
        }

        self.normalized()
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct VoiceSettingsUpdate {
    pub hotkey_shortcut: Option<String>,
    pub recording_mode: Option<String>,
    pub microphone_id: Option<Option<String>>,
    pub language: Option<Option<String>>,
    pub transcription_provider: Option<String>,
    pub auto_insert: Option<bool>,
}

#[derive(Debug, Default)]
pub struct SettingsStore {
    settings: RwLock<VoiceSettings>,
}

impl SettingsStore {
    pub fn new() -> Self {
        Self {
            settings: RwLock::new(VoiceSettings::default()),
        }
    }

    pub fn current(&self) -> VoiceSettings {
        self.settings
            .read()
            .map(|settings| settings.clone())
            .unwrap_or_else(|_| VoiceSettings::default())
    }

    pub fn load<R: Runtime>(&self, app: &AppHandle<R>) -> Result<VoiceSettings, String> {
        let settings_path = self.settings_path(app)?;
        self.load_from_path(&settings_path)
    }

    pub fn update<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        update: VoiceSettingsUpdate,
    ) -> Result<VoiceSettings, String> {
        let settings_path = self.settings_path(app)?;
        self.update_at_path(&settings_path, update)
    }

    fn settings_path<R: Runtime>(&self, app: &AppHandle<R>) -> Result<PathBuf, String> {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;

        Ok(app_data_dir.join(SETTINGS_FILE_NAME))
    }

    fn load_from_path(&self, settings_path: &Path) -> Result<VoiceSettings, String> {
        let settings = read_settings_file(settings_path)?;
        let mut guard = self.settings.write().map_err(|_| lock_error())?;
        *guard = settings.clone();
        Ok(settings)
    }

    fn update_at_path(
        &self,
        settings_path: &Path,
        update: VoiceSettingsUpdate,
    ) -> Result<VoiceSettings, String> {
        let current_settings = read_settings_file(settings_path)?;
        let updated_settings = current_settings.with_update(update)?;
        write_settings_file(settings_path, &updated_settings)?;

        let mut guard = self.settings.write().map_err(|_| lock_error())?;
        *guard = updated_settings.clone();
        Ok(updated_settings)
    }
}

fn read_settings_file(settings_path: &Path) -> Result<VoiceSettings, String> {
    if !settings_path.exists() {
        return Ok(VoiceSettings::default());
    }

    let file_contents = fs::read_to_string(settings_path).map_err(|error| {
        format!(
            "Failed to read settings file `{}`: {error}",
            settings_path.display()
        )
    })?;

    serde_json::from_str::<VoiceSettings>(&file_contents)
        .map_err(|error| {
            format!(
                "Failed to parse settings file `{}`: {error}",
                settings_path.display()
            )
        })?
        .normalized()
}

fn write_settings_file(settings_path: &Path, settings: &VoiceSettings) -> Result<(), String> {
    if let Some(parent_dir) = settings_path.parent() {
        fs::create_dir_all(parent_dir).map_err(|error| {
            format!(
                "Failed to create settings directory `{}`: {error}",
                parent_dir.display()
            )
        })?;
    }

    let serialized = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Failed to serialize settings: {error}"))?;
    fs::write(settings_path, serialized).map_err(|error| {
        format!(
            "Failed to write settings file `{}`: {error}",
            settings_path.display()
        )
    })?;

    Ok(())
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|candidate| {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_required_string(value: String, field_name: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("`{field_name}` cannot be empty"));
    }

    Ok(trimmed.to_string())
}

fn normalize_recording_mode(value: String) -> Result<String, String> {
    let normalized = normalize_required_string(value, "recording_mode")?.to_lowercase();
    match normalized.as_str() {
        RECORDING_MODE_HOLD_TO_TALK | RECORDING_MODE_TOGGLE => Ok(normalized),
        _ => Err(format!(
            "Unsupported recording mode `{normalized}`. Expected `{RECORDING_MODE_HOLD_TO_TALK}` or `{RECORDING_MODE_TOGGLE}`"
        )),
    }
}

fn lock_error() -> String {
    "Settings store lock was poisoned".to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn unique_settings_path(prefix: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("voice-settings-store-{prefix}-{timestamp}"))
            .join("settings.json")
    }

    fn cleanup_settings_path(path: &Path) {
        if let Some(parent_dir) = path.parent() {
            let _ = fs::remove_dir_all(parent_dir);
        }
    }

    #[test]
    fn defaults_match_expected_schema() {
        let defaults = VoiceSettings::default();

        assert_eq!(defaults.hotkey_shortcut, DEFAULT_HOTKEY_SHORTCUT);
        assert_eq!(defaults.recording_mode, RECORDING_MODE_HOLD_TO_TALK);
        assert_eq!(defaults.microphone_id, None);
        assert_eq!(defaults.language, None);
        assert_eq!(
            defaults.transcription_provider,
            DEFAULT_TRANSCRIPTION_PROVIDER
        );
        assert!(defaults.auto_insert);
    }

    #[test]
    fn load_uses_defaults_when_settings_file_is_missing() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("missing");

        let loaded = store
            .load_from_path(&settings_path)
            .expect("loading missing settings should succeed");

        assert_eq!(loaded, VoiceSettings::default());
        cleanup_settings_path(&settings_path);
    }

    #[test]
    fn update_persists_settings_to_disk() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("persist");

        let updated = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    hotkey_shortcut: Some("Cmd+Shift+Space".to_string()),
                    recording_mode: Some("toggle".to_string()),
                    microphone_id: Some(Some("mic-42".to_string())),
                    language: Some(Some("en".to_string())),
                    transcription_provider: Some("OpenAI".to_string()),
                    auto_insert: Some(false),
                },
            )
            .expect("update should succeed");

        let reloaded = read_settings_file(&settings_path).expect("reloading persisted settings");

        assert_eq!(updated.hotkey_shortcut, "Cmd+Shift+Space");
        assert_eq!(updated.recording_mode, RECORDING_MODE_TOGGLE);
        assert_eq!(updated.microphone_id.as_deref(), Some("mic-42"));
        assert_eq!(updated.language.as_deref(), Some("en"));
        assert_eq!(updated.transcription_provider, "openai");
        assert!(!updated.auto_insert);
        assert_eq!(reloaded, updated);

        cleanup_settings_path(&settings_path);
    }

    #[test]
    fn update_accepts_null_to_clear_optional_values() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("clear-options");

        store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    microphone_id: Some(Some("mic-a".to_string())),
                    language: Some(Some("fr".to_string())),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect("initial update should succeed");

        let cleared = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    microphone_id: Some(None),
                    language: Some(None),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect("clearing update should succeed");

        assert_eq!(cleared.microphone_id, None);
        assert_eq!(cleared.language, None);

        cleanup_settings_path(&settings_path);
    }

    #[test]
    fn update_rejects_invalid_recording_mode() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("invalid-mode");

        let error = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    recording_mode: Some("invalid".to_string()),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect_err("invalid mode should fail");

        assert!(error.contains("Unsupported recording mode"));
        cleanup_settings_path(&settings_path);
    }
}

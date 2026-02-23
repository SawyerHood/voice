use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, Runtime};
use tracing::{debug, info, warn};

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
    pub launch_at_login: bool,
    pub onboarding_completed: bool,
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
            launch_at_login: false,
            onboarding_completed: false,
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
            normalize_transcription_provider(self.transcription_provider)?;

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

        if let Some(launch_at_login) = update.launch_at_login {
            self.launch_at_login = launch_at_login;
        }

        if let Some(onboarding_completed) = update.onboarding_completed {
            self.onboarding_completed = onboarding_completed;
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
    pub launch_at_login: Option<bool>,
    pub onboarding_completed: Option<bool>,
}

#[derive(Debug)]
pub struct SettingsStore {
    settings: RwLock<VoiceSettings>,
    io_lock: Mutex<()>,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsStore {
    pub fn new() -> Self {
        debug!("settings store initialized");
        Self {
            settings: RwLock::new(VoiceSettings::default()),
            io_lock: Mutex::new(()),
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
        debug!(path = %settings_path.display(), "loading settings from disk");
        self.load_from_path(&settings_path)
    }

    pub fn update<R: Runtime>(
        &self,
        app: &AppHandle<R>,
        update: VoiceSettingsUpdate,
    ) -> Result<VoiceSettings, String> {
        let settings_path = self.settings_path(app)?;
        debug!(path = %settings_path.display(), "updating settings on disk");
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
        let _io_guard = self.io_lock.lock().map_err(|_| io_lock_error())?;
        let settings = read_settings_file_with_recovery(settings_path)?;
        let mut guard = self.settings.write().map_err(|_| lock_error())?;
        *guard = settings.clone();
        Ok(settings)
    }

    fn update_at_path(
        &self,
        settings_path: &Path,
        update: VoiceSettingsUpdate,
    ) -> Result<VoiceSettings, String> {
        let _io_guard = self.io_lock.lock().map_err(|_| io_lock_error())?;
        let current_settings = read_settings_file_with_recovery(settings_path)?;
        let updated_settings = current_settings.with_update(update)?;
        write_settings_file(settings_path, &updated_settings)?;

        let mut guard = self.settings.write().map_err(|_| lock_error())?;
        *guard = updated_settings.clone();
        Ok(updated_settings)
    }
}

#[derive(Debug)]
struct SettingsReadError {
    message: String,
    recoverable: bool,
}

impl SettingsReadError {
    fn read(message: String) -> Self {
        Self {
            message,
            recoverable: false,
        }
    }

    fn malformed(message: String) -> Self {
        Self {
            message,
            recoverable: true,
        }
    }
}

fn read_settings_file_with_recovery(settings_path: &Path) -> Result<VoiceSettings, String> {
    match read_settings_file(settings_path) {
        Ok(settings) => Ok(settings),
        Err(error) if error.recoverable => {
            let backup_path = backup_corrupt_settings_file(settings_path)?;
            let defaults = VoiceSettings::default();
            write_settings_file(settings_path, &defaults)?;
            warn!(
                path = %settings_path.display(),
                backup = %backup_path.display(),
                reason = %error.message,
                "recovered malformed settings file"
            );
            Ok(defaults)
        }
        Err(error) => Err(error.message),
    }
}

fn read_settings_file(settings_path: &Path) -> Result<VoiceSettings, SettingsReadError> {
    if !settings_path.exists() {
        info!(path = %settings_path.display(), "settings file missing; using defaults");
        return Ok(VoiceSettings::default());
    }

    let file_contents = fs::read_to_string(settings_path)
        .map_err(|error| {
            format!(
                "Failed to read settings file `{}`: {error}",
                settings_path.display()
            )
        })
        .map_err(SettingsReadError::read)?;

    let parsed = serde_json::from_str::<VoiceSettings>(&file_contents).map_err(|error| {
        SettingsReadError::malformed(format!(
            "Failed to parse settings file `{}`: {error}",
            settings_path.display()
        ))
    })?;

    parsed.normalized().map_err(|error| {
        SettingsReadError::malformed(format!(
            "Failed to validate settings file `{}`: {error}",
            settings_path.display()
        ))
    })
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

    let serialized = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("Failed to serialize settings: {error}"))?;
    write_atomic_file(settings_path, &serialized)?;

    info!(
        path = %settings_path.display(),
        recording_mode = %settings.recording_mode,
        auto_insert = settings.auto_insert,
        "settings file written"
    );
    Ok(())
}

fn write_atomic_file(file_path: &Path, contents: &[u8]) -> Result<(), String> {
    let temp_path = temp_file_path_for(file_path);
    let mut temp_file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|error| {
            format!(
                "Failed to create temp settings file `{}`: {error}",
                temp_path.display()
            )
        })?;

    if let Err(error) = temp_file.write_all(contents) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to write temp settings file `{}`: {error}",
            temp_path.display()
        ));
    }

    if let Err(error) = temp_file.sync_all() {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to flush temp settings file `{}`: {error}",
            temp_path.display()
        ));
    }

    drop(temp_file);

    fs::rename(&temp_path, file_path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "Failed to finalize settings file `{}`: {error}",
            file_path.display()
        )
    })?;

    Ok(())
}

fn temp_file_path_for(file_path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = file_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let pid = std::process::id();

    file_path.with_file_name(format!(".{file_name}.{pid}.{timestamp}.tmp"))
}

fn backup_corrupt_settings_file(settings_path: &Path) -> Result<PathBuf, String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = settings_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("settings.json");
    let backup_path = settings_path.with_file_name(format!(
        "{file_name}.corrupt-{}-{timestamp}.bak",
        std::process::id()
    ));

    fs::rename(settings_path, &backup_path).map_err(|error| {
        format!(
            "Failed to backup malformed settings file `{}` to `{}`: {error}",
            settings_path.display(),
            backup_path.display()
        )
    })?;

    Ok(backup_path)
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

fn normalize_transcription_provider(value: String) -> Result<String, String> {
    let normalized = normalize_required_string(value, "transcription_provider")?.to_lowercase();
    match normalized.as_str() {
        DEFAULT_TRANSCRIPTION_PROVIDER => Ok(normalized),
        _ => Err(format!(
            "Unsupported transcription provider `{normalized}`. Expected `{DEFAULT_TRANSCRIPTION_PROVIDER}`"
        )),
    }
}

fn lock_error() -> String {
    "Settings store lock was poisoned".to_string()
}

fn io_lock_error() -> String {
    "Settings store IO lock was poisoned".to_string()
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

    fn corrupt_backup_paths(settings_path: &Path) -> Vec<PathBuf> {
        let Some(parent_dir) = settings_path.parent() else {
            return Vec::new();
        };
        let Some(file_name) = settings_path.file_name().and_then(|name| name.to_str()) else {
            return Vec::new();
        };

        let mut backups = Vec::new();
        if let Ok(entries) = fs::read_dir(parent_dir) {
            for entry in entries.flatten() {
                if let Some(candidate) = entry.file_name().to_str() {
                    if candidate.starts_with(&format!("{file_name}.corrupt-"))
                        && candidate.ends_with(".bak")
                    {
                        backups.push(entry.path());
                    }
                }
            }
        }

        backups
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
        assert!(!defaults.launch_at_login);
        assert!(!defaults.onboarding_completed);
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
    fn load_backfills_launch_at_login_for_legacy_settings_files() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("legacy");

        if let Some(parent_dir) = settings_path.parent() {
            fs::create_dir_all(parent_dir).expect("legacy test directory should be created");
        }

        let legacy_payload = serde_json::json!({
            "hotkey_shortcut": "Alt+Space",
            "recording_mode": "hold_to_talk",
            "microphone_id": null,
            "language": null,
            "transcription_provider": "openai",
            "auto_insert": true
        });
        fs::write(
            &settings_path,
            serde_json::to_string_pretty(&legacy_payload)
                .expect("legacy settings payload should serialize"),
        )
        .expect("legacy settings file should be written");

        let loaded = store
            .load_from_path(&settings_path)
            .expect("legacy settings should load");

        assert!(!loaded.launch_at_login);
        assert!(!loaded.onboarding_completed);
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
                    launch_at_login: Some(true),
                    onboarding_completed: Some(true),
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
        assert!(updated.launch_at_login);
        assert!(updated.onboarding_completed);
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
    fn update_accepts_hold_to_talk_recording_mode() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("hold-to-talk-mode");

        let updated = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    recording_mode: Some(RECORDING_MODE_HOLD_TO_TALK.to_string()),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect("hold-to-talk mode should be supported");

        assert_eq!(updated.recording_mode, RECORDING_MODE_HOLD_TO_TALK);
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

    #[test]
    fn update_rejects_unknown_transcription_provider() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("invalid-provider");

        let error = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    transcription_provider: Some("anthropic".to_string()),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect_err("unsupported provider should fail");

        assert!(error.contains("Unsupported transcription provider"));
        cleanup_settings_path(&settings_path);
    }

    #[test]
    fn load_recovers_from_malformed_json_by_backing_up_and_resetting_defaults() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("malformed");

        if let Some(parent_dir) = settings_path.parent() {
            fs::create_dir_all(parent_dir).expect("malformed test directory should be created");
        }
        fs::write(&settings_path, "{ definitely not json")
            .expect("malformed settings should be written");

        let recovered = store
            .load_from_path(&settings_path)
            .expect("malformed settings should be recovered");

        assert_eq!(recovered, VoiceSettings::default());
        assert_eq!(
            read_settings_file(&settings_path)
                .expect("recovered settings file should be readable")
                .normalized()
                .expect("recovered settings should validate"),
            VoiceSettings::default()
        );
        assert_eq!(corrupt_backup_paths(&settings_path).len(), 1);

        cleanup_settings_path(&settings_path);
    }

    #[test]
    fn update_recovers_from_malformed_json_before_applying_changes() {
        let store = SettingsStore::new();
        let settings_path = unique_settings_path("malformed-update");

        if let Some(parent_dir) = settings_path.parent() {
            fs::create_dir_all(parent_dir).expect("malformed update directory should be created");
        }
        fs::write(&settings_path, "{ broken ")
            .expect("malformed settings should be written for update test");

        let updated = store
            .update_at_path(
                &settings_path,
                VoiceSettingsUpdate {
                    auto_insert: Some(false),
                    ..VoiceSettingsUpdate::default()
                },
            )
            .expect("update should recover malformed settings");

        assert!(!updated.auto_insert);
        assert_eq!(
            updated.transcription_provider,
            DEFAULT_TRANSCRIPTION_PROVIDER
        );
        assert_eq!(corrupt_backup_paths(&settings_path).len(), 1);
        cleanup_settings_path(&settings_path);
    }
}

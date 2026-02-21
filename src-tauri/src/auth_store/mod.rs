use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::debug;

use crate::api_key_store::ApiKeyStore;

const AUTH_CREDENTIALS_FILE_NAME: &str = "auth_credentials.json";
const OPENAI_PROVIDER: &str = "openai";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    #[default]
    None,
    ApiKey,
    ChatgptOauth,
}

impl AuthMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ApiKey => "api_key",
            Self::ChatgptOauth => "chatgpt_oauth",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_lowercase().as_str() {
            "none" => Ok(Self::None),
            "api_key" => Ok(Self::ApiKey),
            "chatgpt_oauth" => Ok(Self::ChatgptOauth),
            other => Err(format!(
                "Unsupported auth method `{other}`. Expected `none`, `api_key`, or `chatgpt_oauth`"
            )),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct AuthCredentials {
    pub auth_method: AuthMethod,
    pub api_key: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptStoredCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    pub account_id: String,
}

#[derive(Debug, Clone)]
pub struct AuthStore {
    file_path: PathBuf,
    io_lock: Arc<Mutex<()>>,
}

impl AuthStore {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let file_path = app_data_dir.join(AUTH_CREDENTIALS_FILE_NAME);
        debug!(path = %file_path.display(), "auth store initialized");
        Self {
            file_path,
            io_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn current(&self) -> Result<AuthCredentials, String> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "auth store lock poisoned".to_string())?;
        self.read_credentials()
    }

    pub fn current_auth_method(&self) -> Result<AuthMethod, String> {
        Ok(self.current()?.auth_method)
    }

    pub fn effective_auth_method(&self, api_key_store: &ApiKeyStore) -> Result<AuthMethod, String> {
        let mut credentials = self.current()?;
        if credentials.auth_method == AuthMethod::None
            && api_key_store.has_api_key(OPENAI_PROVIDER)?
        {
            credentials.auth_method = AuthMethod::ApiKey;
            self.write_credentials(&credentials)?;
        }
        Ok(credentials.auth_method)
    }

    pub fn set_auth_method(&self, method: AuthMethod) -> Result<AuthCredentials, String> {
        self.with_update(|credentials| {
            credentials.auth_method = method;
            Ok(())
        })
    }

    pub fn set_api_key(&self, key: &str) -> Result<AuthCredentials, String> {
        let normalized_key = normalize_required_string(Some(key.to_string()), "api_key")?;
        self.with_update(|credentials| {
            credentials.api_key = Some(normalized_key.clone());
            credentials.auth_method = AuthMethod::ApiKey;
            Ok(())
        })
    }

    pub fn clear_api_key(&self) -> Result<AuthCredentials, String> {
        self.with_update(|credentials| {
            credentials.api_key = None;
            if credentials.auth_method == AuthMethod::ApiKey {
                credentials.auth_method = AuthMethod::None;
            }
            Ok(())
        })
    }

    pub fn save_chatgpt_login(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_id: &str,
    ) -> Result<AuthCredentials, String> {
        let normalized_access =
            normalize_required_string(Some(access_token.to_string()), "access_token")?;
        let normalized_refresh =
            normalize_required_string(Some(refresh_token.to_string()), "refresh_token")?;
        let normalized_account =
            normalize_required_string(Some(account_id.to_string()), "account_id")?;

        self.with_update(|credentials| {
            credentials.auth_method = AuthMethod::ChatgptOauth;
            credentials.access_token = Some(normalized_access.clone());
            credentials.refresh_token = Some(normalized_refresh.clone());
            credentials.expires_at = Some(expires_at);
            credentials.account_id = Some(normalized_account.clone());
            Ok(())
        })
    }

    pub fn update_chatgpt_tokens(
        &self,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_id: &str,
    ) -> Result<AuthCredentials, String> {
        let normalized_access =
            normalize_required_string(Some(access_token.to_string()), "access_token")?;
        let normalized_refresh =
            normalize_required_string(Some(refresh_token.to_string()), "refresh_token")?;
        let normalized_account =
            normalize_required_string(Some(account_id.to_string()), "account_id")?;

        self.with_update(|credentials| {
            credentials.access_token = Some(normalized_access.clone());
            credentials.refresh_token = Some(normalized_refresh.clone());
            credentials.expires_at = Some(expires_at);
            credentials.account_id = Some(normalized_account.clone());
            if credentials.auth_method != AuthMethod::ChatgptOauth {
                credentials.auth_method = AuthMethod::ChatgptOauth;
            }
            Ok(())
        })
    }

    pub fn logout_chatgpt(&self) -> Result<AuthCredentials, String> {
        self.with_update(|credentials| {
            credentials.access_token = None;
            credentials.refresh_token = None;
            credentials.expires_at = None;
            credentials.account_id = None;
            credentials.auth_method = AuthMethod::None;
            Ok(())
        })
    }

    pub fn chatgpt_credentials(&self) -> Result<Option<ChatGptStoredCredentials>, String> {
        let credentials = self.current()?;
        Ok(resolve_chatgpt_credentials(&credentials))
    }

    fn with_update<F>(&self, mut update: F) -> Result<AuthCredentials, String>
    where
        F: FnMut(&mut AuthCredentials) -> Result<(), String>,
    {
        let mut credentials = self.current()?;
        update(&mut credentials)?;
        self.write_credentials(&credentials)?;
        Ok(credentials)
    }

    fn ensure_file_exists(&self) -> Result<(), String> {
        if let Some(parent_dir) = self.file_path.parent() {
            fs::create_dir_all(parent_dir).map_err(|error| {
                format!(
                    "Failed to create auth credentials directory `{}`: {error}",
                    parent_dir.display()
                )
            })?;
        }

        if self.file_path.exists() {
            return Ok(());
        }

        let serialized = serde_json::to_vec_pretty(&AuthCredentials::default())
            .map_err(|error| format!("Failed to serialize default auth credentials: {error}"))?;
        write_atomic_file(&self.file_path, &serialized)
    }

    fn read_credentials(&self) -> Result<AuthCredentials, String> {
        self.ensure_file_exists()?;
        let raw_contents = fs::read_to_string(&self.file_path).map_err(|error| {
            format!(
                "Failed to read auth credentials file `{}`: {error}",
                self.file_path.display()
            )
        })?;

        if raw_contents.trim().is_empty() {
            return Ok(AuthCredentials::default());
        }

        serde_json::from_str::<AuthCredentials>(&raw_contents).map_err(|error| {
            format!(
                "Failed to parse auth credentials file `{}`: {error}",
                self.file_path.display()
            )
        })
    }

    fn write_credentials(&self, credentials: &AuthCredentials) -> Result<(), String> {
        let serialized = serde_json::to_vec_pretty(credentials)
            .map_err(|error| format!("Failed to serialize auth credentials: {error}"))?;
        write_atomic_file(&self.file_path, &serialized)
    }
}

pub fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn resolve_chatgpt_credentials(credentials: &AuthCredentials) -> Option<ChatGptStoredCredentials> {
    Some(ChatGptStoredCredentials {
        access_token: normalize_required_string(credentials.access_token.clone(), "access_token")
            .ok()?,
        refresh_token: normalize_required_string(
            credentials.refresh_token.clone(),
            "refresh_token",
        )
        .ok()?,
        expires_at: credentials.expires_at?,
        account_id: normalize_required_string(credentials.account_id.clone(), "account_id").ok()?,
    })
}

fn normalize_required_string(value: Option<String>, field_name: &str) -> Result<String, String> {
    let Some(value) = value else {
        return Err(format!("Missing required `{field_name}` value"));
    };

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("Missing required `{field_name}` value"));
    }

    Ok(trimmed.to_string())
}

fn write_atomic_file(file_path: &Path, contents: &[u8]) -> Result<(), String> {
    let temp_path = temp_file_path_for(file_path);
    let mut temp_file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|error| {
            format!(
                "Failed to create temp auth credentials file `{}`: {error}",
                temp_path.display()
            )
        })?;

    if let Err(error) = temp_file.write_all(contents) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to write temp auth credentials file `{}`: {error}",
            temp_path.display()
        ));
    }

    if let Err(error) = temp_file.sync_all() {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to flush temp auth credentials file `{}`: {error}",
            temp_path.display()
        ));
    }

    drop(temp_file);

    fs::rename(&temp_path, file_path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "Failed to finalize auth credentials file `{}`: {error}",
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
        .unwrap_or(AUTH_CREDENTIALS_FILE_NAME);
    let pid = std::process::id();

    file_path.with_file_name(format!(".{file_name}.{pid}.{timestamp}.tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_key_store::ApiKeyStore;

    fn temp_app_data_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "voice-auth-store-tests-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("temp auth store test directory should be created");
        path
    }

    #[test]
    fn set_api_key_sets_api_key_auth_method() {
        let app_data_dir = temp_app_data_dir("api-key");
        let store = AuthStore::new(app_data_dir.clone());

        let updated = store
            .set_api_key(" sk-test ")
            .expect("api key should persist");

        assert_eq!(updated.auth_method, AuthMethod::ApiKey);
        assert_eq!(updated.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn save_chatgpt_login_persists_and_logout_clears() {
        let app_data_dir = temp_app_data_dir("chatgpt");
        let store = AuthStore::new(app_data_dir.clone());

        let persisted = store
            .save_chatgpt_login("access", "refresh", 1234, "acct_1")
            .expect("oauth login should persist");
        assert_eq!(persisted.auth_method, AuthMethod::ChatgptOauth);

        let logged_out = store.logout_chatgpt().expect("logout should succeed");
        assert_eq!(logged_out.auth_method, AuthMethod::None);
        assert!(logged_out.access_token.is_none());
        assert!(logged_out.refresh_token.is_none());
        assert!(logged_out.expires_at.is_none());
        assert!(logged_out.account_id.is_none());
    }

    #[test]
    fn effective_auth_method_migrates_existing_openai_key() {
        let app_data_dir = temp_app_data_dir("migrate");
        let api_key_store = ApiKeyStore::new(app_data_dir.clone());
        api_key_store
            .set_api_key("openai", "sk-test")
            .expect("openai key should persist");

        let store = AuthStore::new(app_data_dir);
        let method = store
            .effective_auth_method(&api_key_store)
            .expect("effective method should resolve");

        assert_eq!(method, AuthMethod::ApiKey);
        assert_eq!(
            store
                .current_auth_method()
                .expect("auth method should persist migration"),
            AuthMethod::ApiKey
        );
    }
}

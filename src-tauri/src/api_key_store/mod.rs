use std::sync::Arc;
use tracing::{debug, info, warn};

const KEYCHAIN_SERVICE: &str = "voice.transcription.api-keys";
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

#[derive(Debug, Clone)]
pub struct ApiKeyStore {
    backend: Arc<dyn ApiKeyBackend>,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyStore {
    pub fn new() -> Self {
        debug!("api key store initialized");
        Self {
            backend: Arc::new(SystemKeychainBackend),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn ApiKeyBackend>) -> Self {
        Self { backend }
    }

    pub fn get_api_key(&self, provider: &str) -> Result<Option<String>, String> {
        let account = normalize_provider(provider)?;
        debug!(provider = %account, "reading api key from store");
        self.backend.get(KEYCHAIN_SERVICE, account.as_str())
    }

    pub fn has_api_key(&self, provider: &str) -> Result<bool, String> {
        Ok(self.get_api_key(provider)?.is_some())
    }

    pub fn set_api_key(&self, provider: &str, key: &str) -> Result<(), String> {
        let account = normalize_provider(provider)?;
        let normalized_key = normalize_api_key(key)?;
        info!(provider = %account, "writing api key to store");
        self.backend
            .set(KEYCHAIN_SERVICE, account.as_str(), normalized_key.as_str())
    }

    pub fn delete_api_key(&self, provider: &str) -> Result<(), String> {
        let account = normalize_provider(provider)?;
        info!(provider = %account, "deleting api key from store");
        self.backend.delete(KEYCHAIN_SERVICE, account.as_str())
    }
}

trait ApiKeyBackend: Send + Sync + std::fmt::Debug {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn set(&self, service: &str, account: &str, key: &str) -> Result<(), String>;
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

#[derive(Debug)]
struct SystemKeychainBackend;

#[cfg(target_os = "macos")]
impl ApiKeyBackend for SystemKeychainBackend {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        use security_framework::passwords::get_generic_password;

        match get_generic_password(service, account) {
            Ok(raw_key) => {
                let key = String::from_utf8(raw_key)
                    .map_err(|error| format!("Stored key is not valid UTF-8: {error}"))?;
                debug!(provider = %account, "api key read from macOS keychain");
                Ok(normalize_optional_string(Some(key)))
            }
            Err(error) if is_item_not_found(&error) => Ok(None),
            Err(error) => Err(format!("Failed to read macOS keychain item: {error}")),
        }
    }

    fn set(&self, service: &str, account: &str, key: &str) -> Result<(), String> {
        use security_framework::passwords::set_generic_password;

        set_generic_password(service, account, key.as_bytes())
            .map_err(|error| format!("Failed to write macOS keychain item: {error}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        use security_framework::passwords::delete_generic_password;

        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if is_item_not_found(&error) => {
                warn!(provider = %account, "api key delete requested but keychain item was absent");
                Ok(())
            }
            Err(error) => Err(format!("Failed to delete macOS keychain item: {error}")),
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl ApiKeyBackend for SystemKeychainBackend {
    fn get(&self, _service: &str, _account: &str) -> Result<Option<String>, String> {
        Err("macOS keychain is only available on macOS targets".to_string())
    }

    fn set(&self, _service: &str, _account: &str, _key: &str) -> Result<(), String> {
        Err("macOS keychain is only available on macOS targets".to_string())
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<(), String> {
        Err("macOS keychain is only available on macOS targets".to_string())
    }
}

#[cfg(target_os = "macos")]
fn is_item_not_found(error: &security_framework::base::Error) -> bool {
    error.code() == ERR_SEC_ITEM_NOT_FOUND
}

fn normalize_provider(provider: &str) -> Result<String, String> {
    let trimmed = provider.trim().to_lowercase();
    if trimmed.is_empty() {
        return Err("`provider` cannot be empty".to_string());
    }

    Ok(trimmed)
}

fn normalize_api_key(key: &str) -> Result<String, String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err("`key` cannot be empty".to_string());
    }

    Ok(trimmed.to_string())
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

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[derive(Debug, Default)]
    struct InMemoryBackend {
        map: Mutex<HashMap<(String, String), String>>,
    }

    impl ApiKeyBackend for InMemoryBackend {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            let guard = self
                .map
                .lock()
                .map_err(|_| "backend lock poisoned".to_string())?;
            Ok(guard
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        }

        fn set(&self, service: &str, account: &str, key: &str) -> Result<(), String> {
            let mut guard = self
                .map
                .lock()
                .map_err(|_| "backend lock poisoned".to_string())?;
            guard.insert((service.to_string(), account.to_string()), key.to_string());
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<(), String> {
            let mut guard = self
                .map
                .lock()
                .map_err(|_| "backend lock poisoned".to_string())?;
            guard.remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[test]
    fn set_get_delete_round_trip_works() {
        let store = ApiKeyStore::with_backend(Arc::new(InMemoryBackend::default()));

        store
            .set_api_key("openai", "sk-test-1")
            .expect("set should succeed");
        assert!(
            store.has_api_key("openai").expect("has should succeed"),
            "expected provider to report an API key after saving"
        );
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("get should succeed")
                .as_deref(),
            Some("sk-test-1")
        );

        store
            .delete_api_key("openai")
            .expect("delete should succeed");
        assert!(
            !store.has_api_key("openai").expect("has should succeed"),
            "expected provider to report no API key after deletion"
        );
        assert_eq!(
            store.get_api_key("openai").expect("get should succeed"),
            None
        );
    }

    #[test]
    fn rejects_blank_provider_or_key_values() {
        let store = ApiKeyStore::with_backend(Arc::new(InMemoryBackend::default()));

        assert!(store.get_api_key("  ").is_err());
        assert!(store.has_api_key("  ").is_err());
        assert!(store.set_api_key("openai", "   ").is_err());
        assert!(store.set_api_key("   ", "sk-test-2").is_err());
    }

    #[test]
    fn provider_names_are_normalized_case_insensitively() {
        let store = ApiKeyStore::with_backend(Arc::new(InMemoryBackend::default()));

        store
            .set_api_key("OpenAI", "sk-case")
            .expect("set should succeed");
        assert!(
            store.has_api_key("openai").expect("has should succeed"),
            "expected normalized provider lookup to report stored key"
        );
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("get should succeed")
                .as_deref(),
            Some("sk-case")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_keychain_backend_round_trip_works() {
        let store = ApiKeyStore::new();
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let provider = format!("openai-test-{suffix}");
        let key = format!("sk-roundtrip-{suffix}");

        let _ = store.delete_api_key(provider.as_str());
        store
            .set_api_key(provider.as_str(), key.as_str())
            .expect("set should succeed");
        assert!(
            store
                .has_api_key(provider.as_str())
                .expect("has should succeed after set"),
            "expected macOS keychain provider to report key presence"
        );

        let fetched = store
            .get_api_key(provider.as_str())
            .expect("get should succeed");
        assert_eq!(fetched.as_deref(), Some(key.as_str()));

        store
            .delete_api_key(provider.as_str())
            .expect("delete should succeed");
        assert!(
            !store
                .has_api_key(provider.as_str())
                .expect("has should succeed after delete"),
            "expected macOS keychain provider to report no key after delete"
        );
        assert_eq!(
            store
                .get_api_key(provider.as_str())
                .expect("get after delete should succeed"),
            None
        );
    }
}

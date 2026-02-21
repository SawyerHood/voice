use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::{debug, info};

use crate::settings_store::DEFAULT_TRANSCRIPTION_PROVIDER;

const API_KEY_STORE_NAMESPACE: &str = "voice.transcription.api-keys";
const API_KEYS_FILE_NAME: &str = "api_keys.json";

#[derive(Debug, Clone)]
pub struct ApiKeyStore {
    backend: Arc<dyn ApiKeyBackend>,
    cache: Arc<Mutex<HashMap<String, Option<String>>>>,
}

impl ApiKeyStore {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let file_path = app_data_dir.join(API_KEYS_FILE_NAME);
        debug!(path = %file_path.display(), "api key store initialized");
        Self {
            backend: Arc::new(FileBackend::new(file_path)),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn ApiKeyBackend>) -> Self {
        Self {
            backend,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn get_api_key(&self, provider: &str) -> Result<Option<String>, String> {
        let account = normalize_provider(provider)?;
        if let Some(cached) = self.get_cached_api_key(account.as_str())? {
            debug!(provider = %account, "api key served from in-memory cache");
            return Ok(cached);
        }

        debug!(provider = %account, "reading api key from store");
        let key = self
            .backend
            .get(API_KEY_STORE_NAMESPACE, account.as_str())?;
        self.set_cached_api_key(account.as_str(), key.clone())?;
        Ok(key)
    }

    pub fn has_api_key(&self, provider: &str) -> Result<bool, String> {
        Ok(self.get_api_key(provider)?.is_some())
    }

    pub fn set_api_key(&self, provider: &str, key: &str) -> Result<(), String> {
        let account = normalize_provider(provider)?;
        let normalized_key = normalize_api_key(key)?;
        info!(provider = %account, "writing api key to store");
        self.backend.set(
            API_KEY_STORE_NAMESPACE,
            account.as_str(),
            normalized_key.as_str(),
        )?;
        self.set_cached_api_key(account.as_str(), Some(normalized_key))
    }

    pub fn delete_api_key(&self, provider: &str) -> Result<(), String> {
        let account = normalize_provider(provider)?;
        info!(provider = %account, "deleting api key from store");
        self.backend
            .delete(API_KEY_STORE_NAMESPACE, account.as_str())?;
        self.clear_cached_api_key(account.as_str())
    }

    fn get_cached_api_key(&self, provider: &str) -> Result<Option<Option<String>>, String> {
        let guard = self
            .cache
            .lock()
            .map_err(|_| "api key cache lock poisoned".to_string())?;
        Ok(guard.get(provider).cloned())
    }

    fn set_cached_api_key(&self, provider: &str, key: Option<String>) -> Result<(), String> {
        let mut guard = self
            .cache
            .lock()
            .map_err(|_| "api key cache lock poisoned".to_string())?;
        guard.insert(provider.to_string(), key);
        Ok(())
    }

    fn clear_cached_api_key(&self, provider: &str) -> Result<(), String> {
        let mut guard = self
            .cache
            .lock()
            .map_err(|_| "api key cache lock poisoned".to_string())?;
        guard.remove(provider);
        Ok(())
    }
}

trait ApiKeyBackend: Send + Sync + std::fmt::Debug {
    fn get(&self, service: &str, account: &str) -> Result<Option<String>, String>;
    fn set(&self, service: &str, account: &str, key: &str) -> Result<(), String>;
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

#[derive(Debug)]
struct FileBackend {
    file_path: PathBuf,
    io_lock: Mutex<()>,
}

impl FileBackend {
    fn new(file_path: PathBuf) -> Self {
        Self {
            file_path,
            io_lock: Mutex::new(()),
        }
    }

    fn ensure_file_exists(&self) -> Result<(), String> {
        if let Some(parent_dir) = self.file_path.parent() {
            fs::create_dir_all(parent_dir).map_err(|error| {
                format!(
                    "Failed to create API key directory `{}`: {error}",
                    parent_dir.display()
                )
            })?;
        }

        if self.file_path.exists() {
            return Ok(());
        }

        write_atomic_file(&self.file_path, br#"{}"#)
    }

    fn read_keys(&self) -> Result<HashMap<String, String>, String> {
        self.ensure_file_exists()?;
        let raw_contents = fs::read_to_string(&self.file_path).map_err(|error| {
            format!(
                "Failed to read API key file `{}`: {error}",
                self.file_path.display()
            )
        })?;

        if raw_contents.trim().is_empty() {
            return Ok(HashMap::new());
        }

        serde_json::from_str(&raw_contents).map_err(|error| {
            format!(
                "Failed to parse API key file `{}`: {error}",
                self.file_path.display()
            )
        })
    }

    fn write_keys(&self, keys: &HashMap<String, String>) -> Result<(), String> {
        let serialized = serde_json::to_vec_pretty(keys)
            .map_err(|error| format!("Failed to serialize API keys: {error}"))?;
        write_atomic_file(&self.file_path, &serialized)
    }
}

impl ApiKeyBackend for FileBackend {
    fn get(&self, _service: &str, account: &str) -> Result<Option<String>, String> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "api key backend lock poisoned".to_string())?;
        let keys = self.read_keys()?;
        Ok(normalize_optional_string(keys.get(account).cloned()))
    }

    fn set(&self, _service: &str, account: &str, key: &str) -> Result<(), String> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "api key backend lock poisoned".to_string())?;
        let mut keys = self.read_keys()?;
        keys.insert(account.to_string(), key.to_string());
        self.write_keys(&keys)
    }

    fn delete(&self, _service: &str, account: &str) -> Result<(), String> {
        let _guard = self
            .io_lock
            .lock()
            .map_err(|_| "api key backend lock poisoned".to_string())?;
        let mut keys = self.read_keys()?;
        if keys.remove(account).is_some() {
            return self.write_keys(&keys);
        }

        Ok(())
    }
}

fn write_atomic_file(file_path: &Path, contents: &[u8]) -> Result<(), String> {
    let temp_path = temp_file_path_for(file_path);
    let mut temp_file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(|error| {
            format!(
                "Failed to create temp API key file `{}`: {error}",
                temp_path.display()
            )
        })?;

    if let Err(error) = temp_file.write_all(contents) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to write temp API key file `{}`: {error}",
            temp_path.display()
        ));
    }

    if let Err(error) = temp_file.sync_all() {
        let _ = fs::remove_file(&temp_path);
        return Err(format!(
            "Failed to flush temp API key file `{}`: {error}",
            temp_path.display()
        ));
    }

    drop(temp_file);

    fs::rename(&temp_path, file_path).map_err(|error| {
        let _ = fs::remove_file(&temp_path);
        format!(
            "Failed to finalize API key file `{}`: {error}",
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
        .unwrap_or(API_KEYS_FILE_NAME);
    let pid = std::process::id();

    file_path.with_file_name(format!(".{file_name}.{pid}.{timestamp}.tmp"))
}

fn normalize_provider(provider: &str) -> Result<String, String> {
    let trimmed = provider.trim().to_lowercase();
    if trimmed.is_empty() {
        return Err("`provider` cannot be empty".to_string());
    }

    if !is_supported_provider(trimmed.as_str()) {
        return Err(format!(
            "Unsupported provider `{trimmed}`. Expected `{DEFAULT_TRANSCRIPTION_PROVIDER}`"
        ));
    }

    Ok(trimmed)
}

fn is_supported_provider(provider: &str) -> bool {
    if provider == DEFAULT_TRANSCRIPTION_PROVIDER {
        return true;
    }

    #[cfg(test)]
    {
        if provider.starts_with("openai-test-") {
            return true;
        }
    }

    false
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
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
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

    #[derive(Debug, Default)]
    struct CountingBackend {
        map: Mutex<HashMap<(String, String), String>>,
        get_calls: AtomicUsize,
    }

    impl CountingBackend {
        fn get_call_count(&self) -> usize {
            self.get_calls.load(Ordering::SeqCst)
        }
    }

    impl ApiKeyBackend for CountingBackend {
        fn get(&self, service: &str, account: &str) -> Result<Option<String>, String> {
            self.get_calls.fetch_add(1, Ordering::SeqCst);
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

    fn unique_api_key_file_path(prefix: &str) -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("voice-api-key-store-{prefix}-{timestamp}"))
            .join(API_KEYS_FILE_NAME)
    }

    fn cleanup_api_key_file(path: &Path) {
        if let Some(parent_dir) = path.parent() {
            let _ = fs::remove_dir_all(parent_dir);
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

    #[test]
    fn rejects_unsupported_provider() {
        let store = ApiKeyStore::with_backend(Arc::new(InMemoryBackend::default()));

        assert!(store.get_api_key("anthropic").is_err());
        assert!(store.has_api_key("gemini").is_err());
        assert!(store.set_api_key("azure-openai", "sk-test").is_err());
        assert!(store.delete_api_key("custom").is_err());
    }

    #[test]
    fn caches_backend_get_results_per_provider() {
        let backend = Arc::new(CountingBackend::default());
        backend
            .set(API_KEY_STORE_NAMESPACE, "openai", "sk-cached")
            .expect("seed should succeed");
        let store = ApiKeyStore::with_backend(backend.clone());

        assert_eq!(
            store
                .get_api_key("openai")
                .expect("first get should succeed")
                .as_deref(),
            Some("sk-cached")
        );
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("second get should succeed")
                .as_deref(),
            Some("sk-cached")
        );
        assert_eq!(
            backend.get_call_count(),
            1,
            "expected backend read to happen once due to in-memory cache"
        );
    }

    #[test]
    fn caches_missing_api_keys() {
        let backend = Arc::new(CountingBackend::default());
        let store = ApiKeyStore::with_backend(backend.clone());

        assert_eq!(
            store
                .get_api_key("openai")
                .expect("first get should succeed for missing value"),
            None
        );
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("second get should succeed for missing value"),
            None
        );
        assert_eq!(
            backend.get_call_count(),
            1,
            "expected missing key result to be cached"
        );
    }

    #[test]
    fn set_and_delete_keep_cache_in_sync() {
        let backend = Arc::new(CountingBackend::default());
        let store = ApiKeyStore::with_backend(backend.clone());

        store
            .set_api_key("openai", "sk-updated")
            .expect("set should succeed");
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("get after set should succeed")
                .as_deref(),
            Some("sk-updated")
        );
        assert_eq!(
            backend.get_call_count(),
            0,
            "expected get after set to come from cache"
        );

        store
            .delete_api_key("openai")
            .expect("delete should succeed");
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("get after delete should succeed"),
            None
        );
        assert_eq!(
            backend.get_call_count(),
            1,
            "expected backend read after delete because cache entry is removed"
        );
    }

    #[test]
    fn file_backend_round_trip_works() {
        let file_path = unique_api_key_file_path("roundtrip");
        let app_data_dir = file_path
            .parent()
            .expect("api key file should have parent directory")
            .to_path_buf();
        let store = ApiKeyStore::new(app_data_dir);

        assert_eq!(
            store
                .get_api_key("openai")
                .expect("initial lookup should succeed"),
            None
        );
        assert!(
            file_path.exists(),
            "expected API key file to be created when first read"
        );

        store
            .set_api_key("openai", "sk-file")
            .expect("set should succeed");
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("lookup after set should succeed")
                .as_deref(),
            Some("sk-file")
        );

        let persisted = fs::read_to_string(&file_path).expect("api key file should be readable");
        let persisted_map = serde_json::from_str::<HashMap<String, String>>(&persisted)
            .expect("api key file should parse");
        assert_eq!(
            persisted_map.get("openai").map(String::as_str),
            Some("sk-file")
        );

        store
            .delete_api_key("openai")
            .expect("delete should succeed");
        assert_eq!(
            store
                .get_api_key("openai")
                .expect("lookup after delete should succeed"),
            None
        );

        let after_delete =
            fs::read_to_string(&file_path).expect("api key file should remain readable");
        let after_delete_map = serde_json::from_str::<HashMap<String, String>>(&after_delete)
            .expect("api key file should parse after delete");
        assert!(
            !after_delete_map.contains_key("openai"),
            "expected API key to be removed from persisted file"
        );

        cleanup_api_key_file(&file_path);
    }
}

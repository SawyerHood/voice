use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tauri::{AppHandle, Manager, Runtime};
use tracing::info;
use tracing_subscriber::{fmt::MakeWriter, prelude::*, EnvFilter};

const LOG_FILE_NAME: &str = "voice.log";
const DEFAULT_LOG_FILTER: &str = "info,tauri_app_lib=debug";
const MAX_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LoggingState {
    log_file_path: Arc<PathBuf>,
}

impl LoggingState {
    pub fn new(log_file_path: PathBuf) -> Self {
        Self {
            log_file_path: Arc::new(log_file_path),
        }
    }

    pub fn log_file_path(&self) -> &Path {
        self.log_file_path.as_ref().as_path()
    }
}

pub fn initialize<R: Runtime>(app: &AppHandle<R>) -> Result<LoggingState, String> {
    let log_file_path = resolve_log_file_path(app)?;
    let log_file = open_log_file(&log_file_path)?;
    let writer = SharedLogWriterFactory::new(log_file);
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    let subscriber = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_level(true)
                .with_writer(writer),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_level(true)
                .with_writer(std::io::stderr),
        );

    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| format!("Failed to initialize diagnostics logger: {error}"))?;

    info!(log_file = %log_file_path.display(), "diagnostic logging initialized");
    Ok(LoggingState::new(log_file_path))
}

pub fn export_log_contents(state: &LoggingState) -> Result<String, String> {
    read_log_file(state.log_file_path())
}

fn resolve_log_file_path<R: Runtime>(app: &AppHandle<R>) -> Result<PathBuf, String> {
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory for logs: {error}"))?;

    Ok(app_data_dir.join(LOG_FILE_NAME))
}

fn open_log_file(log_file_path: &Path) -> Result<File, String> {
    if let Some(parent_dir) = log_file_path.parent() {
        fs::create_dir_all(parent_dir).map_err(|error| {
            format!(
                "Failed to create diagnostics log directory `{}`: {error}",
                parent_dir.display()
            )
        })?;
    }

    cap_log_file_size(log_file_path, MAX_LOG_FILE_BYTES)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path)
        .map_err(|error| {
            format!(
                "Failed to open diagnostics log file `{}`: {error}",
                log_file_path.display()
            )
        })
}

fn cap_log_file_size(log_file_path: &Path, max_bytes: u64) -> Result<(), String> {
    let metadata = match fs::metadata(log_file_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to inspect diagnostics log file `{}`: {error}",
                log_file_path.display()
            ))
        }
    };

    if metadata.len() <= max_bytes {
        return Ok(());
    }

    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(log_file_path)
        .map_err(|error| {
            format!(
                "Failed to truncate oversized diagnostics log file `{}`: {error}",
                log_file_path.display()
            )
        })?;

    Ok(())
}

fn read_log_file(log_file_path: &Path) -> Result<String, String> {
    let contents = match fs::read(log_file_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(format!(
                "Failed to read diagnostics log file `{}`: {error}",
                log_file_path.display()
            ))
        }
    };

    Ok(String::from_utf8_lossy(&contents).into_owned())
}

#[derive(Debug, Clone)]
struct SharedLogWriterFactory {
    file: Arc<Mutex<File>>,
}

impl SharedLogWriterFactory {
    fn new(file: File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

impl<'a> MakeWriter<'a> for SharedLogWriterFactory {
    type Writer = SharedLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        SharedLogWriter {
            file: Arc::clone(&self.file),
        }
    }
}

struct SharedLogWriter {
    file: Arc<Mutex<File>>,
}

impl io::Write for SharedLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log file lock poisoned"))?;
        file.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "log file lock poisoned"))?;
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use super::{cap_log_file_size, read_log_file};

    fn temp_log_path(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock should progress")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}.log"))
    }

    #[test]
    fn capping_oversized_log_file_truncates_contents() {
        let path = temp_log_path("voice-log-cap");
        fs::write(&path, "x".repeat(1024)).expect("should write test log file");

        cap_log_file_size(&path, 128).expect("capping should succeed");

        let truncated = fs::read_to_string(&path).expect("should read truncated log file");
        assert!(truncated.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn reading_missing_log_file_returns_empty_string() {
        let path = temp_log_path("voice-log-missing");

        let contents = read_log_file(&path).expect("reading missing log should succeed");
        assert!(contents.is_empty());
    }
}

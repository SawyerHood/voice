use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    Idle,
    Listening,
    Transcribing,
    Error,
}

impl Default for AppStatus {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Default)]
pub struct StatusNotifier {
    current: AppStatus,
}

impl StatusNotifier {
    pub fn new() -> Self {
        debug!("status notifier initialized");
        Self {
            current: AppStatus::Idle,
        }
    }

    pub fn current(&self) -> AppStatus {
        self.current
    }

    pub fn set(&mut self, status: AppStatus) {
        debug!(from = ?self.current, to = ?status, "status notifier updated");
        self.current = status;
    }
}

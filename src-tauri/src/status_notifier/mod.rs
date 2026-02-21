use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
        Self {
            current: AppStatus::Idle,
        }
    }

    pub fn current(&self) -> AppStatus {
        self.current
    }

    pub fn set(&mut self, status: AppStatus) {
        self.current = status;
    }
}

use tracing::debug;

#[derive(Debug, Clone, Copy)]
pub enum PermissionState {
    Unknown,
    Granted,
    Denied,
}

#[derive(Debug, Default)]
pub struct PermissionService;

impl PermissionService {
    pub fn new() -> Self {
        debug!("permission service initialized");
        Self
    }

    pub fn microphone_permission(&self) -> PermissionState {
        debug!("microphone permission check requested");
        // TODO: inspect microphone permission state.
        PermissionState::Unknown
    }
}

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
        Self
    }

    pub fn microphone_permission(&self) -> PermissionState {
        // TODO: inspect microphone permission state.
        PermissionState::Unknown
    }
}

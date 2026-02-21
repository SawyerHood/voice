#[derive(Debug, Default)]
pub struct HotkeyService;

impl HotkeyService {
    pub fn new() -> Self {
        Self
    }

    pub fn register_default_shortcut(&self) {
        // TODO: register a global shortcut for hold-to-talk / toggle modes.
    }
}

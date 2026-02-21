#[derive(Debug, Default)]
pub struct SettingsStore;

impl SettingsStore {
    pub fn new() -> Self {
        Self
    }

    pub fn load(&self) {
        // TODO: load persisted settings.
    }

    pub fn save(&self) {
        // TODO: persist settings updates.
    }
}

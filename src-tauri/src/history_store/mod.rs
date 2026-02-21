#[derive(Debug, Default)]
pub struct HistoryStore;

impl HistoryStore {
    pub fn new() -> Self {
        Self
    }

    pub fn append_entry(&self, _text: &str) {
        // TODO: store transcript history entry.
    }
}

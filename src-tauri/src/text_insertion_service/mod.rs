#[derive(Debug, Default)]
pub struct TextInsertionService;

impl TextInsertionService {
    pub fn new() -> Self {
        Self
    }

    pub fn insert_text(&self, _text: &str) {
        // TODO: insert into focused app with clipboard fallback.
    }
}

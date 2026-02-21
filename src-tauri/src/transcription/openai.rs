use super::{TranscriptionProvider, TranscriptionRequest, TranscriptionResult};

#[derive(Debug, Default)]
pub struct OpenAiTranscriptionProvider;

impl OpenAiTranscriptionProvider {
    pub fn new() -> Self {
        Self
    }
}

impl TranscriptionProvider for OpenAiTranscriptionProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn transcribe(&self, _request: TranscriptionRequest) -> Result<TranscriptionResult, String> {
        Err("OpenAI transcription provider stub is not implemented yet".to_string())
    }
}

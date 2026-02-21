pub mod openai;

#[derive(Debug, Clone)]
pub struct TranscriptionRequest {
    pub language: Option<String>,
    pub audio_bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct TranscriptionResult {
    pub text: String,
}

pub trait TranscriptionProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn transcribe(&self, request: TranscriptionRequest) -> Result<TranscriptionResult, String>;
}

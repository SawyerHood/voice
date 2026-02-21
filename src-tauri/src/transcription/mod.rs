pub mod openai;

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptionOptions {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub context_hint: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptionResult {
    pub text: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptionError {
    MissingApiKey,
    Authentication(String),
    RateLimited(String),
    Network(String),
    InvalidResponse(String),
    Provider(String),
}

impl fmt::Display for TranscriptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingApiKey => write!(f, "Missing transcription provider API key"),
            Self::Authentication(message) => write!(f, "Authentication failed: {message}"),
            Self::RateLimited(message) => write!(f, "Rate limited: {message}"),
            Self::Network(message) => write!(f, "Network error: {message}"),
            Self::InvalidResponse(message) => write!(f, "Invalid provider response: {message}"),
            Self::Provider(message) => write!(f, "Transcription provider error: {message}"),
        }
    }
}

impl std::error::Error for TranscriptionError {}

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        options: TranscriptionOptions,
    ) -> Result<TranscriptionResult, TranscriptionError>;
}

#[derive(Clone)]
pub struct TranscriptionOrchestrator {
    active_provider: Arc<dyn TranscriptionProvider>,
}

impl fmt::Debug for TranscriptionOrchestrator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptionOrchestrator")
            .field("active_provider", &self.active_provider.name())
            .finish()
    }
}

impl TranscriptionOrchestrator {
    pub fn new(active_provider: Arc<dyn TranscriptionProvider>) -> Self {
        Self { active_provider }
    }

    pub async fn transcribe(
        &self,
        audio_data: Vec<u8>,
        options: TranscriptionOptions,
    ) -> Result<TranscriptionResult, TranscriptionError> {
        if audio_data.is_empty() {
            return Err(TranscriptionError::Provider(
                "Audio payload is empty".to_string(),
            ));
        }

        let mut result = self.active_provider.transcribe(audio_data, options).await?;
        result.text = normalize_transcript_text(&result.text);
        Ok(result)
    }
}

pub(crate) fn normalize_transcript_text(raw_text: &str) -> String {
    raw_text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct StubProvider {
        captured_audio_len: Mutex<Option<usize>>,
        response_text: String,
    }

    #[async_trait]
    impl TranscriptionProvider for StubProvider {
        fn name(&self) -> &'static str {
            "stub"
        }

        async fn transcribe(
            &self,
            audio_data: Vec<u8>,
            _options: TranscriptionOptions,
        ) -> Result<TranscriptionResult, TranscriptionError> {
            let mut guard = self
                .captured_audio_len
                .lock()
                .expect("stub provider lock should not be poisoned");
            *guard = Some(audio_data.len());

            Ok(TranscriptionResult {
                text: self.response_text.clone(),
                language: Some("en".to_string()),
                duration_secs: Some(1.5),
                confidence: Some(0.8),
            })
        }
    }

    #[tokio::test]
    async fn orchestrator_normalizes_whitespace_and_forwards_audio() {
        let provider = Arc::new(StubProvider {
            captured_audio_len: Mutex::new(None),
            response_text: "  hello    world\n\nfrom   provider ".to_string(),
        });
        let orchestrator = TranscriptionOrchestrator::new(provider.clone());

        let result = orchestrator
            .transcribe(
                vec![1, 2, 3, 4],
                TranscriptionOptions {
                    language: Some("en".to_string()),
                    prompt: Some("dictation".to_string()),
                    context_hint: Some("short reply".to_string()),
                },
            )
            .await
            .expect("transcription should succeed");

        assert_eq!(result.text, "hello world from provider");
        assert_eq!(result.language.as_deref(), Some("en"));
        assert_eq!(result.duration_secs, Some(1.5));
        assert_eq!(
            *provider
                .captured_audio_len
                .lock()
                .expect("stub provider lock should not be poisoned"),
            Some(4)
        );
    }

    #[tokio::test]
    async fn orchestrator_rejects_empty_audio_payload() {
        let provider = Arc::new(StubProvider {
            captured_audio_len: Mutex::new(None),
            response_text: "unused".to_string(),
        });
        let orchestrator = TranscriptionOrchestrator::new(provider);

        let error = orchestrator
            .transcribe(Vec::new(), TranscriptionOptions::default())
            .await
            .expect_err("empty audio should fail");

        assert_eq!(
            error,
            TranscriptionError::Provider("Audio payload is empty".to_string())
        );
    }
}

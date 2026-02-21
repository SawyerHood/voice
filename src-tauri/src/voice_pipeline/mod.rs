use std::time::Duration;

use async_trait::async_trait;

use crate::status_notifier::AppStatus;

const DEFAULT_ERROR_RESET_DELAY_MS: u64 = 1_500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineErrorStage {
    RecordingStart,
    RecordingStop,
    RecordingRuntime,
    Transcription,
    TextInsertion,
}

impl PipelineErrorStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RecordingStart => "recording_start",
            Self::RecordingStop => "recording_stop",
            Self::RecordingRuntime => "recording_runtime",
            Self::Transcription => "transcription",
            Self::TextInsertion => "text_insertion",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineError {
    pub stage: PipelineErrorStage,
    pub message: String,
}

#[async_trait]
pub trait VoicePipelineDelegate: Send + Sync {
    fn set_status(&self, status: AppStatus);
    fn emit_transcript(&self, transcript: &str);
    fn emit_error(&self, error: &PipelineError);
    fn start_recording(&self) -> Result<(), String>;
    fn stop_recording(&self) -> Result<Vec<u8>, String>;
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> Result<String, String>;
    fn insert_text(&self, transcript: &str) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct VoicePipeline {
    error_reset_delay: Duration,
}

impl Default for VoicePipeline {
    fn default() -> Self {
        Self {
            error_reset_delay: Duration::from_millis(DEFAULT_ERROR_RESET_DELAY_MS),
        }
    }
}

impl VoicePipeline {
    pub fn new(error_reset_delay: Duration) -> Self {
        Self { error_reset_delay }
    }

    pub async fn handle_hotkey_started<D: VoicePipelineDelegate>(&self, delegate: &D) {
        match delegate.start_recording() {
            Ok(()) => delegate.set_status(AppStatus::Listening),
            Err(message) => {
                self.handle_error(delegate, PipelineErrorStage::RecordingStart, message)
                    .await;
            }
        }
    }

    pub async fn handle_hotkey_stopped<D: VoicePipelineDelegate>(&self, delegate: &D) {
        delegate.set_status(AppStatus::Transcribing);

        let wav_bytes = match delegate.stop_recording() {
            Ok(wav_bytes) => wav_bytes,
            Err(message) => {
                self.handle_error(delegate, PipelineErrorStage::RecordingStop, message)
                    .await;
                return;
            }
        };

        let transcript = match delegate.transcribe(wav_bytes).await {
            Ok(transcript) => transcript,
            Err(message) => {
                self.handle_error(delegate, PipelineErrorStage::Transcription, message)
                    .await;
                return;
            }
        };

        delegate.emit_transcript(&transcript);

        if let Err(message) = delegate.insert_text(&transcript) {
            self.handle_error(delegate, PipelineErrorStage::TextInsertion, message)
                .await;
            return;
        }

        delegate.set_status(AppStatus::Idle);
    }

    async fn handle_error<D: VoicePipelineDelegate>(
        &self,
        delegate: &D,
        stage: PipelineErrorStage,
        message: String,
    ) {
        let error = PipelineError { stage, message };
        delegate.emit_error(&error);
        delegate.set_status(AppStatus::Error);
        tokio::time::sleep(self.error_reset_delay).await;
        delegate.set_status(AppStatus::Idle);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Debug)]
    struct MockDelegate {
        start_result: Result<(), String>,
        stop_result: Result<Vec<u8>, String>,
        transcribe_result: Result<String, String>,
        insert_result: Result<(), String>,
        statuses: Mutex<Vec<AppStatus>>,
        transcripts: Mutex<Vec<String>>,
        errors: Mutex<Vec<PipelineError>>,
        call_order: Mutex<Vec<&'static str>>,
    }

    impl Default for MockDelegate {
        fn default() -> Self {
            Self {
                start_result: Ok(()),
                stop_result: Ok(vec![1, 2, 3]),
                transcribe_result: Ok("hello world".to_string()),
                insert_result: Ok(()),
                statuses: Mutex::new(Vec::new()),
                transcripts: Mutex::new(Vec::new()),
                errors: Mutex::new(Vec::new()),
                call_order: Mutex::new(Vec::new()),
            }
        }
    }

    impl MockDelegate {
        fn statuses(&self) -> Vec<AppStatus> {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .clone()
        }

        fn transcripts(&self) -> Vec<String> {
            self.transcripts
                .lock()
                .expect("transcript lock should not be poisoned")
                .clone()
        }

        fn errors(&self) -> Vec<PipelineError> {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .clone()
        }

        fn call_order(&self) -> Vec<&'static str> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .clone()
        }
    }

    #[async_trait]
    impl VoicePipelineDelegate for MockDelegate {
        fn set_status(&self, status: AppStatus) {
            self.statuses
                .lock()
                .expect("status lock should not be poisoned")
                .push(status);
        }

        fn emit_transcript(&self, transcript: &str) {
            self.transcripts
                .lock()
                .expect("transcript lock should not be poisoned")
                .push(transcript.to_string());
        }

        fn emit_error(&self, error: &PipelineError) {
            self.errors
                .lock()
                .expect("error lock should not be poisoned")
                .push(error.clone());
        }

        fn start_recording(&self) -> Result<(), String> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .push("start_recording");
            self.start_result.clone()
        }

        fn stop_recording(&self) -> Result<Vec<u8>, String> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .push("stop_recording");
            self.stop_result.clone()
        }

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<String, String> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .push("transcribe");
            self.transcribe_result.clone()
        }

        fn insert_text(&self, _transcript: &str) -> Result<(), String> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .push("insert_text");
            self.insert_result.clone()
        }
    }

    #[tokio::test]
    async fn hotkey_start_success_sets_listening_status() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate::default();

        pipeline.handle_hotkey_started(&delegate).await;

        assert_eq!(delegate.call_order(), vec!["start_recording"]);
        assert_eq!(delegate.statuses(), vec![AppStatus::Listening]);
        assert!(delegate.errors().is_empty());
    }

    #[tokio::test]
    async fn hotkey_start_failure_sets_error_then_idle() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate {
            start_result: Err("microphone unavailable".to_string()),
            ..MockDelegate::default()
        };

        pipeline.handle_hotkey_started(&delegate).await;

        assert_eq!(delegate.call_order(), vec!["start_recording"]);
        assert_eq!(delegate.statuses(), vec![AppStatus::Error, AppStatus::Idle]);
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::RecordingStart,
                message: "microphone unavailable".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn hotkey_stop_success_runs_pipeline_and_returns_to_idle() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate::default();

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(
            delegate.call_order(),
            vec!["stop_recording", "transcribe", "insert_text"]
        );
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Idle]
        );
        assert_eq!(delegate.transcripts(), vec!["hello world".to_string()]);
        assert!(delegate.errors().is_empty());
    }

    #[tokio::test]
    async fn hotkey_stop_recording_failure_sets_error_then_idle() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate {
            stop_result: Err("recording not active".to_string()),
            ..MockDelegate::default()
        };

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(delegate.call_order(), vec!["stop_recording"]);
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::RecordingStop,
                message: "recording not active".to_string(),
            }]
        );
        assert!(delegate.transcripts().is_empty());
    }

    #[tokio::test]
    async fn hotkey_stop_transcription_failure_sets_error_then_idle() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate {
            transcribe_result: Err("provider unavailable".to_string()),
            ..MockDelegate::default()
        };

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(delegate.call_order(), vec!["stop_recording", "transcribe"]);
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::Transcription,
                message: "provider unavailable".to_string(),
            }]
        );
        assert!(delegate.transcripts().is_empty());
    }
}

use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, error, info, warn};

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

#[derive(Debug, Clone, PartialEq)]
pub struct PipelineTranscript {
    pub text: String,
    pub duration_secs: Option<f64>,
    pub language: Option<String>,
    pub provider: String,
}

#[async_trait]
pub trait VoicePipelineDelegate: Send + Sync {
    fn set_status(&self, status: AppStatus);
    fn emit_transcript(&self, transcript: &str);
    fn emit_error(&self, error: &PipelineError);
    fn on_recording_started(&self, _success: bool) {}
    fn on_recording_stopped(&self, _success: bool) {}
    fn start_recording(&self) -> Result<(), String>;
    fn stop_recording(&self) -> Result<Vec<u8>, String>;
    async fn transcribe(&self, wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String>;
    fn insert_text(&self, transcript: &str) -> Result<(), String>;
    fn save_history_entry(&self, _transcript: &PipelineTranscript) -> Result<(), String> {
        Ok(())
    }
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
    #[cfg(test)]
    pub fn new(error_reset_delay: Duration) -> Self {
        debug!(?error_reset_delay, "voice pipeline initialized");
        Self { error_reset_delay }
    }

    pub async fn handle_hotkey_started<D: VoicePipelineDelegate>(&self, delegate: &D) {
        info!("pipeline handling hotkey start");
        match delegate.start_recording() {
            Ok(()) => {
                info!("recording started successfully from hotkey");
                delegate.on_recording_started(true);
                delegate.set_status(AppStatus::Listening);
            }
            Err(message) => {
                error!(message = %message, "recording start failed from hotkey");
                delegate.on_recording_started(false);
                self.handle_error(delegate, PipelineErrorStage::RecordingStart, message)
                    .await;
            }
        }
    }

    pub async fn handle_hotkey_stopped<D: VoicePipelineDelegate>(&self, delegate: &D) {
        info!("pipeline handling hotkey stop");
        delegate.set_status(AppStatus::Transcribing);

        let wav_bytes = match delegate.stop_recording() {
            Ok(wav_bytes) => {
                info!(
                    audio_bytes = wav_bytes.len(),
                    "recording stopped successfully"
                );
                delegate.on_recording_stopped(true);
                wav_bytes
            }
            Err(message) => {
                error!(message = %message, "recording stop failed");
                delegate.on_recording_stopped(false);
                self.handle_error(delegate, PipelineErrorStage::RecordingStop, message)
                    .await;
                return;
            }
        };

        let transcript = match delegate.transcribe(wav_bytes).await {
            Ok(transcript) => {
                info!(
                    transcript_chars = transcript.text.chars().count(),
                    provider = %transcript.provider,
                    "transcription completed in pipeline"
                );
                transcript
            }
            Err(message) => {
                error!(message = %message, "pipeline transcription failed");
                self.handle_error(delegate, PipelineErrorStage::Transcription, message)
                    .await;
                return;
            }
        };

        delegate.emit_transcript(&transcript.text);

        if let Err(message) = delegate.save_history_entry(&transcript) {
            warn!(message = %message, "failed to persist transcript history entry");
        }

        if let Err(message) = delegate.insert_text(&transcript.text) {
            error!(message = %message, "pipeline text insertion failed");
            self.handle_error(delegate, PipelineErrorStage::TextInsertion, message)
                .await;
            return;
        }
        info!("pipeline text insertion succeeded");

        debug!("pipeline returning to idle status");
        delegate.set_status(AppStatus::Idle);
    }

    pub async fn handle_stage_error<D: VoicePipelineDelegate>(
        &self,
        delegate: &D,
        stage: PipelineErrorStage,
        message: String,
    ) {
        debug!(stage = stage.as_str(), "handling pipeline stage error");
        self.handle_error(delegate, stage, message).await;
    }

    async fn handle_error<D: VoicePipelineDelegate>(
        &self,
        delegate: &D,
        stage: PipelineErrorStage,
        message: String,
    ) {
        let error = PipelineError { stage, message };
        error!(
            stage = error.stage.as_str(),
            message = %error.message,
            "pipeline entering error state"
        );
        delegate.emit_error(&error);
        delegate.set_status(AppStatus::Error);
        debug!(
            delay_ms = self.error_reset_delay.as_millis(),
            "waiting before idle reset"
        );
        tokio::time::sleep(self.error_reset_delay).await;
        info!("pipeline resetting status to idle after error");
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
        transcribe_result: Result<PipelineTranscript, String>,
        insert_result: Result<(), String>,
        save_history_result: Result<(), String>,
        start_acknowledgements: Mutex<Vec<bool>>,
        stop_acknowledgements: Mutex<Vec<bool>>,
        statuses: Mutex<Vec<AppStatus>>,
        transcripts: Mutex<Vec<String>>,
        saved_history: Mutex<Vec<PipelineTranscript>>,
        errors: Mutex<Vec<PipelineError>>,
        call_order: Mutex<Vec<&'static str>>,
    }

    impl Default for MockDelegate {
        fn default() -> Self {
            Self {
                start_result: Ok(()),
                stop_result: Ok(vec![1, 2, 3]),
                transcribe_result: Ok(PipelineTranscript {
                    text: "hello world".to_string(),
                    duration_secs: Some(2.4),
                    language: Some("en".to_string()),
                    provider: "openai".to_string(),
                }),
                insert_result: Ok(()),
                save_history_result: Ok(()),
                start_acknowledgements: Mutex::new(Vec::new()),
                stop_acknowledgements: Mutex::new(Vec::new()),
                statuses: Mutex::new(Vec::new()),
                transcripts: Mutex::new(Vec::new()),
                saved_history: Mutex::new(Vec::new()),
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

        fn saved_history(&self) -> Vec<PipelineTranscript> {
            self.saved_history
                .lock()
                .expect("saved-history lock should not be poisoned")
                .clone()
        }

        fn call_order(&self) -> Vec<&'static str> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .clone()
        }

        fn start_acknowledgements(&self) -> Vec<bool> {
            self.start_acknowledgements
                .lock()
                .expect("start-ack lock should not be poisoned")
                .clone()
        }

        fn stop_acknowledgements(&self) -> Vec<bool> {
            self.stop_acknowledgements
                .lock()
                .expect("stop-ack lock should not be poisoned")
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

        fn on_recording_started(&self, success: bool) {
            self.start_acknowledgements
                .lock()
                .expect("start-ack lock should not be poisoned")
                .push(success);
        }

        fn on_recording_stopped(&self, success: bool) {
            self.stop_acknowledgements
                .lock()
                .expect("stop-ack lock should not be poisoned")
                .push(success);
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

        async fn transcribe(&self, _wav_bytes: Vec<u8>) -> Result<PipelineTranscript, String> {
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

        fn save_history_entry(&self, transcript: &PipelineTranscript) -> Result<(), String> {
            self.call_order
                .lock()
                .expect("call-order lock should not be poisoned")
                .push("save_history_entry");
            self.saved_history
                .lock()
                .expect("saved-history lock should not be poisoned")
                .push(transcript.clone());
            self.save_history_result.clone()
        }
    }

    #[tokio::test]
    async fn hotkey_start_success_sets_listening_status() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate::default();

        pipeline.handle_hotkey_started(&delegate).await;

        assert_eq!(delegate.call_order(), vec!["start_recording"]);
        assert_eq!(delegate.start_acknowledgements(), vec![true]);
        assert!(delegate.stop_acknowledgements().is_empty());
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
        assert_eq!(delegate.start_acknowledgements(), vec![false]);
        assert!(delegate.stop_acknowledgements().is_empty());
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
            vec![
                "stop_recording",
                "transcribe",
                "save_history_entry",
                "insert_text"
            ]
        );
        assert!(delegate.start_acknowledgements().is_empty());
        assert_eq!(delegate.stop_acknowledgements(), vec![true]);
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Idle]
        );
        assert_eq!(delegate.transcripts(), vec!["hello world".to_string()]);
        assert_eq!(
            delegate.saved_history(),
            vec![PipelineTranscript {
                text: "hello world".to_string(),
                duration_secs: Some(2.4),
                language: Some("en".to_string()),
                provider: "openai".to_string(),
            }]
        );
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
        assert!(delegate.start_acknowledgements().is_empty());
        assert_eq!(delegate.stop_acknowledgements(), vec![false]);
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
        assert!(delegate.saved_history().is_empty());
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
        assert!(delegate.start_acknowledgements().is_empty());
        assert_eq!(delegate.stop_acknowledgements(), vec![true]);
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
        assert!(delegate.saved_history().is_empty());
    }

    #[tokio::test]
    async fn hotkey_stop_history_persist_failure_does_not_fail_pipeline() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate {
            save_history_result: Err("disk full".to_string()),
            ..MockDelegate::default()
        };

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(
            delegate.call_order(),
            vec![
                "stop_recording",
                "transcribe",
                "save_history_entry",
                "insert_text"
            ]
        );
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Idle]
        );
        assert!(delegate.errors().is_empty());
    }

    #[tokio::test]
    async fn hotkey_stop_insertion_failure_emits_transcript_and_sets_error() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate {
            insert_result: Err("accessibility denied".to_string()),
            ..MockDelegate::default()
        };

        pipeline.handle_hotkey_stopped(&delegate).await;

        assert_eq!(
            delegate.call_order(),
            vec![
                "stop_recording",
                "transcribe",
                "save_history_entry",
                "insert_text"
            ]
        );
        assert_eq!(delegate.transcripts(), vec!["hello world".to_string()]);
        assert_eq!(
            delegate.saved_history(),
            vec![PipelineTranscript {
                text: "hello world".to_string(),
                duration_secs: Some(2.4),
                language: Some("en".to_string()),
                provider: "openai".to_string(),
            }]
        );
        assert_eq!(
            delegate.statuses(),
            vec![AppStatus::Transcribing, AppStatus::Error, AppStatus::Idle]
        );
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::TextInsertion,
                message: "accessibility denied".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn handle_stage_error_uses_same_error_reset_policy() {
        let pipeline = VoicePipeline::new(Duration::ZERO);
        let delegate = MockDelegate::default();

        pipeline
            .handle_stage_error(
                &delegate,
                PipelineErrorStage::Transcription,
                "provider unavailable".to_string(),
            )
            .await;

        assert_eq!(delegate.statuses(), vec![AppStatus::Error, AppStatus::Idle]);
        assert_eq!(
            delegate.errors(),
            vec![PipelineError {
                stage: PipelineErrorStage::Transcription,
                message: "provider unavailable".to_string(),
            }]
        );
    }
}

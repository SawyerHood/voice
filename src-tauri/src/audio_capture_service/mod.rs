#[derive(Debug, Default)]
pub struct AudioCaptureService;

impl AudioCaptureService {
    pub fn new() -> Self {
        Self
    }

    pub fn start_recording(&self) {
        // TODO: start buffering microphone audio.
    }

    pub fn stop_recording(&self) {
        // TODO: finalize buffered audio payload.
    }
}

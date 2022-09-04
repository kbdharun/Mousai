mod aud_d;
mod error;
mod settings;

use async_trait::async_trait;

use std::{fmt, time::Duration};

use self::error::ProviderError;
pub use self::settings::{ProviderSettings, ProviderType, TestProviderMode};
use crate::{audio_recording::AudioRecording, model::Song};

#[async_trait(?Send)]
pub trait Provider: fmt::Debug {
    /// Recognize a song from a recording
    async fn recognize(&self, recording: &AudioRecording) -> Result<Song, ProviderError>;

    /// How long to record the audio
    fn listen_duration(&self) -> Duration;

    /// Whether this supports `TestProviderMode`
    fn is_test(&self) -> bool;
}

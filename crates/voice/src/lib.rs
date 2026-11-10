//! Voice calls for AtroxaBot.
//!
//! Microphone → voice-activity detection → speech-to-text → (the app's agent answers with streamed text) →
//! sentence segmentation → streaming text-to-speech → speakers, with barge-in. Fish Audio and ElevenLabs are
//! both supported for speech-to-text and text-to-speech.
//!
//! Everything is blocking I/O on std threads. [`Call`] runs a whole call; [`voices`], [`transcribe`] and
//! [`synthesize`] are the provider calls it is built from.
//!
//! There is no echo cancellation: with loudspeakers the agent's own voice can reach the microphone and count
//! as a barge-in. Use a headset, raise [`CallOptions::barge_in_rms`] or turn [`CallOptions::barge_in`] off.

mod audio;
mod call;
mod dsp;
mod provider;
mod textfilter;
mod turn;
mod wav;

use std::fmt;

pub use audio::{Device, input_devices, output_devices};
pub use call::{Call, CallEvent, CallOptions, CallState};
pub use provider::{PLAYBACK_RATE, Voice, synthesize, transcribe, voices};

/// A speech provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    /// Fish Audio (`api.fish.audio`).
    #[default]
    Fish,
    /// ElevenLabs (`api.elevenlabs.io`).
    ElevenLabs,
}

impl Provider {
    /// Every provider, in display order.
    pub const ALL: [Self; 2] = [Self::Fish, Self::ElevenLabs];

    /// The stable identifier for settings files: `"fish"` or `"elevenlabs"`.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Fish => "fish",
            Self::ElevenLabs => "elevenlabs",
        }
    }

    /// The provider with this [`key`](Self::key).
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.key() == key)
    }

    /// The name to show to people: `"Fish Audio"` or `"ElevenLabs"`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fish => "Fish Audio",
            Self::ElevenLabs => "ElevenLabs",
        }
    }

    /// The text-to-speech model used when [`VoiceConfig::tts_model`] is empty.
    #[must_use]
    pub const fn default_tts_model(self) -> &'static str {
        match self {
            Self::Fish => "s2.1-pro",
            Self::ElevenLabs => "eleven_flash_v2_5",
        }
    }

    /// The speech-to-text model used when [`VoiceConfig::stt_model`] is empty.
    #[must_use]
    pub const fn default_stt_model(self) -> &'static str {
        match self {
            Self::Fish => "transcribe-1-pro",
            Self::ElevenLabs => "scribe_v2",
        }
    }
}

/// Which provider, account and voice to use.
#[derive(Clone, Default)]
pub struct VoiceConfig {
    /// The speech provider.
    pub provider: Provider,
    /// The provider's API key. Only ever sent to that provider's host.
    pub api_key: String,
    /// The voice to speak with. Empty: the provider's default voice.
    pub voice_id: String,
    /// The text-to-speech model. Empty: [`Provider::default_tts_model`].
    pub tts_model: String,
    /// The speech-to-text model. Empty: [`Provider::default_stt_model`].
    pub stt_model: String,
    /// A language hint for speech-to-text, like `"nl"`. Empty: detect it.
    pub language: String,
    /// Speaking speed. `0.0` or `1.0`: normal. Clamped to what the provider accepts.
    pub speed: f32,
}

impl fmt::Debug for VoiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceConfig")
            .field("provider", &self.provider)
            .field("api_key", &if self.api_key.is_empty() { "<unset>" } else { "<redacted>" })
            .field("voice_id", &self.voice_id)
            .field("tts_model", &self.tts_model)
            .field("stt_model", &self.stt_model)
            .field("language", &self.language)
            .field("speed", &self.speed)
            .finish()
    }
}

/// What can go wrong in this crate. No variant ever contains an API key.
#[derive(Debug)]
pub enum Error {
    /// [`VoiceConfig::api_key`] is empty.
    MissingKey(Provider),
    /// The request never got an answer: DNS, TLS, a timeout or a dropped connection.
    Http {
        /// The provider that was called.
        provider: Provider,
        /// What the HTTP client reported.
        message: String,
    },
    /// The provider answered with an error status.
    Api {
        /// The provider that was called.
        provider: Provider,
        /// The HTTP status code.
        status: u16,
        /// The provider's explanation, or the start of the response body.
        message: String,
    },
    /// The provider answered with something that could not be understood.
    Response {
        /// The provider that was called.
        provider: Provider,
        /// What was wrong with the response.
        message: String,
    },
    /// An audio device is missing, unsupported or failed to start.
    Device(String),
    /// A worker thread could not be started.
    Thread(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey(provider) => write!(f, "no {} API key is set", provider.label()),
            Self::Http { provider, message } => write!(f, "could not reach {}: {message}", provider.label()),
            Self::Api { provider, status, message } => write!(f, "{} answered {status}: {message}", provider.label()),
            Self::Response { provider, message } => write!(f, "unexpected answer from {}: {message}", provider.label()),
            Self::Device(message) => write!(f, "audio device: {message}"),
            Self::Thread(source) => write!(f, "could not start a thread: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Thread(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_round_trip() {
        for provider in Provider::ALL {
            assert_eq!(Provider::from_key(provider.key()), Some(provider));
        }
        assert_eq!(Provider::from_key("elevenlabs"), Some(Provider::ElevenLabs));
        assert_eq!(Provider::from_key("openai"), None);
        assert_eq!(Provider::default(), Provider::Fish);
    }

    #[test]
    fn debug_redacts_the_api_key() {
        let config = VoiceConfig { api_key: "sk-secret-123".into(), ..VoiceConfig::default() };
        let debug = format!("{config:?}");
        assert!(!debug.contains("sk-secret-123"));
        assert!(debug.contains("<redacted>"));
    }
}

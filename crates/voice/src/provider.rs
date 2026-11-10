//! The Fish Audio and ElevenLabs HTTP APIs: voices, speech-to-text and streaming text-to-speech.
//!
//! Requests are built and responses parsed by pure functions, so both are tested without a network.

use std::fmt::Write;
use std::io::Read;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::textfilter::strip_cues;
use crate::wav::encode_wav;
use crate::{Error, Provider, VoiceConfig, turn};

/// The sample rate of the audio [`synthesize`] produces, in Hz.
pub const PLAYBACK_RATE: u32 = 24_000;

const FISH: &str = "https://api.fish.audio";
const ELEVENLABS: &str = "https://api.elevenlabs.io";
/// "George", the premade voice ElevenLabs uses in its own examples.
const ELEVENLABS_DEFAULT_VOICE: &str = "JBFqnCBsd6RMkjVDRZzb";
/// Speech-to-text rejects or mishears less audio than this (100 ms).
const MIN_TRANSCRIBE_SAMPLES: usize = turn::RATE as usize / 10;
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// A voice to speak with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    /// What goes into [`VoiceConfig::voice_id`].
    pub id: String,
    /// The voice's name.
    pub name: String,
    /// The provider's description of it; may be empty.
    pub description: String,
}

/// One HTTP request: a POST when it has a body, a GET otherwise.
pub(crate) struct Request {
    pub(crate) url: String,
    pub(crate) headers: Vec<(&'static str, String)>,
    pub(crate) body: Option<Vec<u8>>,
}

/// The voices to choose from: the account's own first, then (Fish Audio) the most popular public ones.
///
/// One bounded page per list, so at most 150 voices.
///
/// # Errors
/// When the key is missing, the provider cannot be reached or it answers with an error.
pub fn voices(config: &VoiceConfig) -> Result<Vec<Voice>, Error> {
    require_key(config)?;
    let mut all: Vec<Voice> = Vec::new();
    for request in voices_requests(config) {
        let body = read_text(config.provider, send(config, &request)?)?;
        for voice in parse_voices(config.provider, &body)? {
            if !all.iter().any(|known| known.id == voice.id) {
                all.push(voice);
            }
        }
    }
    Ok(all)
}

/// Transcribes 16 kHz mono audio; blocks until the provider answers. Silence gives an empty string.
///
/// A rate-limit or server error is retried once.
///
/// # Errors
/// When the key is missing, the provider cannot be reached or it answers with an error.
pub fn transcribe(config: &VoiceConfig, pcm_16k_mono: &[i16]) -> Result<String, Error> {
    require_key(config)?;
    if pcm_16k_mono.len() < MIN_TRANSCRIBE_SAMPLES {
        return Ok(String::new());
    }
    let request = stt_request(config, pcm_16k_mono);
    let attempt = || {
        let body = read_text(config.provider, send(config, &request)?)?;
        parse_transcript(config.provider, &body)
    };
    match attempt() {
        Err(Error::Api { status, .. }) if status == 429 || status >= 500 => {
            thread::sleep(RETRY_DELAY);
            attempt()
        }
        result => result,
    }
}

/// Streams speech for `text`; `on_pcm` gets s16le mono chunks at [`PLAYBACK_RATE`] as they arrive.
///
/// Returns early (`Ok`) once `cancel` is set; that is checked whenever a chunk arrives.
///
/// # Errors
/// When the key is missing, the provider cannot be reached, it answers with an error or the stream breaks.
pub fn synthesize(
    config: &VoiceConfig,
    text: &str,
    cancel: &AtomicBool,
    on_pcm: &mut dyn FnMut(&[i16]),
) -> Result<(), Error> {
    require_key(config)?;
    if text.trim().is_empty() {
        return Ok(());
    }
    let mut reader = send(config, &tts_request(config, text))?.into_body().into_reader();
    let mut bytes = [0u8; 8192];
    let mut samples = Vec::with_capacity(bytes.len() / 2 + 1);
    let mut carry = None;
    while !cancel.load(Ordering::Relaxed) {
        let read = match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::Http { provider: config.provider, message: error.to_string() }),
        };
        samples.clear();
        decode_pcm(&mut carry, bytes.get(..read).unwrap_or_default(), &mut samples);
        if !samples.is_empty() {
            on_pcm(&samples);
        }
    }
    Ok(())
}

/// Appends the s16le samples in `bytes` to `samples`; `carry` holds half a sample between calls.
fn decode_pcm(carry: &mut Option<u8>, mut bytes: &[u8], samples: &mut Vec<i16>) {
    if let Some(low) = carry.take() {
        let Some((&high, rest)) = bytes.split_first() else {
            *carry = Some(low);
            return;
        };
        samples.push(i16::from_le_bytes([low, high]));
        bytes = rest;
    }
    let (pairs, rest) = bytes.as_chunks::<2>();
    samples.extend(pairs.iter().map(|&pair| i16::from_le_bytes(pair)));
    *carry = rest.first().copied();
}

fn require_key(config: &VoiceConfig) -> Result<(), Error> {
    if config.api_key.is_empty() { Err(Error::MissingKey(config.provider)) } else { Ok(()) }
}

/// `value`, or `default` when it is empty.
fn or_default<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.is_empty() { default } else { value }
}

fn auth(config: &VoiceConfig) -> (&'static str, String) {
    match config.provider {
        Provider::Fish => ("Authorization", format!("Bearer {}", config.api_key)),
        Provider::ElevenLabs => ("xi-api-key", config.api_key.clone()),
    }
}

/// The speed to ask for, or `None` for the provider's normal speed.
fn speed(config: &VoiceConfig) -> Option<f64> {
    let speed = f64::from(config.speed);
    if !speed.is_finite() || speed <= 0.0 || (speed - 1.0).abs() < 0.005 {
        return None;
    }
    // Fish documents 0.5 to 2.0. ElevenLabs rejects values outside 0.7 to 1.2.
    let (slowest, fastest) = match config.provider {
        Provider::Fish => (0.5, 2.0),
        Provider::ElevenLabs => (0.7, 1.2),
    };
    Some((speed.clamp(slowest, fastest) * 100.0).round() / 100.0)
}

/// Keeps unreserved URL characters and percent-encodes the rest, so an id cannot change the path.
fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

pub(crate) fn voices_requests(config: &VoiceConfig) -> Vec<Request> {
    let get = |url: String| Request { url, headers: vec![auth(config)], body: None };
    match config.provider {
        Provider::Fish => {
            vec![get(format!("{FISH}/model?self=true&page_size=100")), get(format!("{FISH}/model?page_size=50"))]
        }
        Provider::ElevenLabs => vec![get(format!("{ELEVENLABS}/v2/voices?page_size=100"))],
    }
}

pub(crate) fn tts_request(config: &VoiceConfig, text: &str) -> Request {
    let model = or_default(&config.tts_model, config.provider.default_tts_model());
    let mut headers = vec![auth(config), ("Content-Type", "application/json".to_owned())];
    let mut body = Map::new();
    body.insert("text".to_owned(), text.into());
    let url = match config.provider {
        Provider::Fish => {
            // The model goes in a header; an unknown one silently becomes the default.
            headers.push(("model", model.to_owned()));
            body.insert("format".to_owned(), "pcm".into());
            body.insert("sample_rate".to_owned(), PLAYBACK_RATE.into());
            body.insert("latency".to_owned(), "balanced".into());
            if !config.voice_id.is_empty() {
                body.insert("reference_id".to_owned(), config.voice_id.as_str().into());
            }
            if let Some(speed) = speed(config) {
                body.insert("prosody".to_owned(), serde_json::json!({ "speed": speed }));
            }
            format!("{FISH}/v1/tts")
        }
        Provider::ElevenLabs => {
            body.insert("model_id".to_owned(), model.into());
            if let Some(speed) = speed(config) {
                body.insert("voice_settings".to_owned(), serde_json::json!({ "speed": speed }));
            }
            let voice = encode_path_segment(or_default(&config.voice_id, ELEVENLABS_DEFAULT_VOICE));
            format!("{ELEVENLABS}/v1/text-to-speech/{voice}/stream?output_format=pcm_{PLAYBACK_RATE}")
        }
    };
    Request { url, headers, body: Some(Value::Object(body).to_string().into_bytes()) }
}

pub(crate) fn stt_request(config: &VoiceConfig, pcm: &[i16]) -> Request {
    let model = or_default(&config.stt_model, config.provider.default_stt_model());
    let mut headers = vec![auth(config)];
    let (url, (content_type, body)) = match config.provider {
        Provider::Fish => {
            // The model goes in a header; a missing or misspelled one is billed as transcribe-1.
            headers.push(("model", model.to_owned()));
            let mut fields = vec![("ignore_timestamps", "true")];
            if !config.language.is_empty() {
                fields.push(("language", config.language.as_str()));
            }
            let wav = encode_wav(pcm, turn::RATE);
            (format!("{FISH}/v1/asr"), multipart(&fields, ("audio", "audio.wav", "audio/wav"), &wav))
        }
        Provider::ElevenLabs => {
            // Raw 16 kHz s16le mono is what we have, and ElevenLabs transcribes it faster than encoded audio.
            let mut fields = vec![("model_id", model), ("file_format", "pcm_s16le_16"), ("tag_audio_events", "false")];
            if !config.language.is_empty() {
                fields.push(("language_code", config.language.as_str()));
            }
            let raw: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
            let file = ("file", "audio.pcm", "application/octet-stream");
            (format!("{ELEVENLABS}/v1/speech-to-text"), multipart(&fields, file, &raw))
        }
    };
    headers.push(("Content-Type", content_type));
    Request { url, headers, body: Some(body) }
}

/// A `multipart/form-data` body with text fields and one file (`(field, file name, content type)`).
///
/// Returns the `Content-Type` header value and the body.
pub(crate) fn multipart(fields: &[(&str, &str)], file: (&str, &str, &str), data: &[u8]) -> (String, Vec<u8>) {
    const BOUNDARY: &str = "atroxabot-voice-boundary";
    let clean = |value: &str| value.replace(['\r', '\n', '"'], "");
    // The boundary must not occur in the audio. It practically never does; count up until it does not.
    let mut boundary = BOUNDARY.to_owned();
    let mut attempt = 0u32;
    while data.windows(boundary.len()).any(|window| window == boundary.as_bytes()) {
        attempt += 1;
        boundary = format!("{BOUNDARY}-{attempt}");
    }
    let mut body = Vec::with_capacity(data.len() + 512);
    for (name, value) in fields {
        let part = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n",
            clean(name),
            clean(value)
        );
        body.extend_from_slice(part.as_bytes());
    }
    let (name, file_name, content_type) = file;
    let head = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\n\
         Content-Type: {content_type}\r\n\r\n"
    );
    body.extend_from_slice(head.as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn parse_json(provider: Provider, body: &str) -> Result<Value, Error> {
    serde_json::from_str(body).map_err(|error| Error::Response { provider, message: error.to_string() })
}

pub(crate) fn parse_voices(provider: Provider, body: &str) -> Result<Vec<Voice>, Error> {
    let json = parse_json(provider, body)?;
    let (list, id, name) = match provider {
        Provider::Fish => ("items", "_id", "title"),
        Provider::ElevenLabs => ("voices", "voice_id", "name"),
    };
    let entries = json
        .get(list)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Response { provider, message: format!("no \"{list}\" list") })?;
    let text = |entry: &Value, key: &str| entry.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
    let voices = entries.iter().filter_map(|entry| {
        // Fish Audio also lists voice-conversion models and ones that are still training.
        let usable = entry.get("type").and_then(Value::as_str).is_none_or(|kind| kind == "tts")
            && entry.get("state").and_then(Value::as_str).is_none_or(|state| state == "trained");
        let voice = Voice { id: text(entry, id), name: text(entry, name), description: text(entry, "description") };
        (usable && !voice.id.is_empty()).then_some(voice)
    });
    Ok(voices.collect())
}

pub(crate) fn parse_transcript(provider: Provider, body: &str) -> Result<String, Error> {
    let json = parse_json(provider, body)?;
    let text = json
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Response { provider, message: "no \"text\" in the transcript".to_owned() })?;
    Ok(clean_transcript(text))
}

/// Drops speaker markers like `<|speaker:0|>` and sound cues like `[laughter]`.
pub(crate) fn clean_transcript(text: &str) -> String {
    let mut spoken = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once("<|speaker:") {
        spoken.push_str(before);
        spoken.push(' ');
        let after_digits = after.trim_start_matches(|c: char| c.is_ascii_digit());
        match after_digits.strip_prefix("|>") {
            Some(tail) if after_digits.len() < after.len() => rest = tail,
            _ => {
                spoken.push_str("<|speaker:");
                rest = after;
            }
        }
    }
    spoken.push_str(rest);
    strip_cues(&spoken.replace('[', " [")).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The human-readable part of an error response, or the start of the body.
pub(crate) fn error_message(body: &str) -> String {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let detail = json.get("detail");
    let message = json
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| detail.and_then(|detail| detail.get("message")).and_then(Value::as_str))
        .or_else(|| detail.and_then(Value::as_str))
        .or_else(|| detail.and_then(|detail| detail.get(0)).and_then(|first| first.get("msg")).and_then(Value::as_str))
        .unwrap_or(body);
    message.trim().chars().take(300).collect()
}

/// One connection pool for both providers, so sentences reuse a TLS connection.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .https_only(true)
        // A redirect could carry the key header to another host.
        .max_redirects(0)
        // Error bodies explain what went wrong; `send` reads them.
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(20)))
        .timeout_per_call(Some(Duration::from_secs(180)))
        .build()
        .new_agent()
});

fn send(config: &VoiceConfig, request: &Request) -> Result<ureq::http::Response<ureq::Body>, Error> {
    let provider = config.provider;
    let result = if let Some(body) = &request.body {
        let mut builder = AGENT.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }
        builder.send(body.as_slice())
    } else {
        let mut builder = AGENT.get(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(*name, value);
        }
        builder.call()
    };
    let response = result.map_err(|error| Error::Http { provider, message: error.to_string() })?;
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    let body = response.into_body().into_with_config().limit(64 * 1024).lossy_utf8(true).read_to_string();
    // No provider echoes the key, but an error must never be the place where it leaks.
    let mut message = error_message(&body.unwrap_or_default());
    if !config.api_key.is_empty() {
        message = message.replace(&config.api_key, "<key>");
    }
    Err(Error::Api { provider, status, message })
}

fn read_text(provider: Provider, response: ureq::http::Response<ureq::Body>) -> Result<String, Error> {
    response
        .into_body()
        .into_with_config()
        .limit(8 * 1024 * 1024)
        .read_to_string()
        .map_err(|error| Error::Http { provider, message: error.to_string() })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::dsp::Resampler;

    const KEY: &str = "secret-key";

    fn config(provider: Provider) -> VoiceConfig {
        VoiceConfig { provider, api_key: KEY.into(), ..VoiceConfig::default() }
    }

    fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request.headers.iter().find(|(header, _)| *header == name).map(|(_, value)| value.as_str())
    }

    fn json_body(request: &Request) -> Value {
        serde_json::from_slice(request.body.as_deref().unwrap()).unwrap()
    }

    fn text_body(request: &Request) -> String {
        String::from_utf8_lossy(request.body.as_deref().unwrap()).into_owned()
    }

    #[test]
    fn fish_tts_request_defaults() {
        let request = tts_request(&config(Provider::Fish), "Hello there. ");
        assert_eq!(request.url, "https://api.fish.audio/v1/tts");
        assert_eq!(header(&request, "Authorization"), Some("Bearer secret-key"));
        assert_eq!(header(&request, "Content-Type"), Some("application/json"));
        assert_eq!(header(&request, "model"), Some("s2.1-pro"));
        assert_eq!(header(&request, "xi-api-key"), None);
        assert_eq!(
            json_body(&request),
            json!({ "text": "Hello there. ", "format": "pcm", "sample_rate": 24000, "latency": "balanced" })
        );
    }

    #[test]
    fn fish_tts_request_with_voice_model_and_speed() {
        let config = VoiceConfig {
            voice_id: "voice-123".into(),
            tts_model: "s2-pro".into(),
            speed: 1.3,
            ..config(Provider::Fish)
        };
        let request = tts_request(&config, "Hi");
        assert_eq!(header(&request, "model"), Some("s2-pro"));
        let body = json_body(&request);
        assert_eq!(body["reference_id"], "voice-123");
        assert_eq!(body["prosody"], json!({ "speed": 1.3 }));
    }

    #[test]
    fn elevenlabs_tts_request_defaults() {
        let request = tts_request(&config(Provider::ElevenLabs), "Hello there. ");
        assert_eq!(
            request.url,
            "https://api.elevenlabs.io/v1/text-to-speech/JBFqnCBsd6RMkjVDRZzb/stream?output_format=pcm_24000"
        );
        assert_eq!(header(&request, "xi-api-key"), Some(KEY));
        assert_eq!(header(&request, "Content-Type"), Some("application/json"));
        assert_eq!(header(&request, "Authorization"), None);
        assert_eq!(json_body(&request), json!({ "text": "Hello there. ", "model_id": "eleven_flash_v2_5" }));
    }

    #[test]
    fn elevenlabs_tts_request_with_voice_model_and_speed() {
        let config = VoiceConfig {
            voice_id: "abc/../x?y".into(),
            tts_model: "eleven_v3".into(),
            speed: 3.0,
            ..config(Provider::ElevenLabs)
        };
        let request = tts_request(&config, "Hi");
        assert!(request.url.starts_with("https://api.elevenlabs.io/v1/text-to-speech/abc%2F..%2Fx%3Fy/stream?"));
        let body = json_body(&request);
        assert_eq!(body["model_id"], "eleven_v3");
        assert_eq!(body["voice_settings"], json!({ "speed": 1.2 }));
    }

    #[test]
    fn normal_speed_is_not_sent() {
        for value in [0.0, 1.0, -1.0, f32::NAN] {
            for provider in Provider::ALL {
                let body = json_body(&tts_request(&VoiceConfig { speed: value, ..config(provider) }, "Hi"));
                assert!(body.get("prosody").is_none() && body.get("voice_settings").is_none());
            }
        }
        assert_eq!(speed(&VoiceConfig { speed: 0.1, ..config(Provider::Fish) }), Some(0.5));
        assert_eq!(speed(&VoiceConfig { speed: 0.1, ..config(Provider::ElevenLabs) }), Some(0.7));
    }

    #[test]
    fn fish_stt_request_carries_model_header_and_wav_file() {
        let request = stt_request(&config(Provider::Fish), &[1, 2, 3]);
        assert_eq!(request.url, "https://api.fish.audio/v1/asr");
        assert_eq!(header(&request, "Authorization"), Some("Bearer secret-key"));
        assert_eq!(header(&request, "model"), Some("transcribe-1-pro"));
        assert_eq!(header(&request, "Content-Type"), Some("multipart/form-data; boundary=atroxabot-voice-boundary"));
        let body = text_body(&request);
        assert!(body.contains("name=\"ignore_timestamps\"\r\n\r\ntrue\r\n"));
        assert!(body.contains("name=\"audio\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF"));
        assert!(!body.contains("name=\"language\""));
        let wav = encode_wav(&[1, 2, 3], 16_000);
        assert!(request.body.as_deref().unwrap().windows(wav.len()).any(|window| window == wav));
    }

    #[test]
    fn fish_stt_request_sends_language_and_model_when_set() {
        let config = VoiceConfig { language: "nl".into(), stt_model: "transcribe-1".into(), ..config(Provider::Fish) };
        let request = stt_request(&config, &[0; 16]);
        assert_eq!(header(&request, "model"), Some("transcribe-1"));
        assert!(text_body(&request).contains("name=\"language\"\r\n\r\nnl\r\n"));
    }

    #[test]
    fn elevenlabs_stt_request_sends_raw_pcm() {
        let config = VoiceConfig { language: "nl".into(), ..config(Provider::ElevenLabs) };
        let request = stt_request(&config, &[0x0102, -2]);
        assert_eq!(request.url, "https://api.elevenlabs.io/v1/speech-to-text");
        assert_eq!(header(&request, "xi-api-key"), Some(KEY));
        assert_eq!(header(&request, "model"), None);
        let body = request.body.as_deref().unwrap();
        let text = text_body(&request);
        assert!(text.contains("name=\"model_id\"\r\n\r\nscribe_v2\r\n"));
        assert!(text.contains("name=\"file_format\"\r\n\r\npcm_s16le_16\r\n"));
        assert!(text.contains("name=\"tag_audio_events\"\r\n\r\nfalse\r\n"));
        assert!(text.contains("name=\"language_code\"\r\n\r\nnl\r\n"));
        assert!(text.contains("name=\"file\"; filename=\"audio.pcm\"\r\nContent-Type: application/octet-stream\r\n"));
        let tail = b"\r\n\r\n\x02\x01\xFE\xFF\r\n--atroxabot-voice-boundary--\r\n";
        assert!(body.ends_with(tail));
    }

    #[test]
    fn voices_requests_stay_on_the_provider_host() {
        let fish = voices_requests(&config(Provider::Fish));
        let urls: Vec<&str> = fish.iter().map(|request| request.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://api.fish.audio/model?self=true&page_size=100", "https://api.fish.audio/model?page_size=50"]
        );
        assert!(fish.iter().all(|request| header(request, "Authorization").is_some() && request.body.is_none()));
        let elevenlabs = voices_requests(&config(Provider::ElevenLabs));
        assert_eq!(elevenlabs.len(), 1);
        assert_eq!(elevenlabs[0].url, "https://api.elevenlabs.io/v2/voices?page_size=100");
        assert_eq!(header(&elevenlabs[0], "xi-api-key"), Some(KEY));
    }

    #[test]
    fn multipart_body_is_exact() {
        let (content_type, body) = multipart(&[("a", "1"), ("b", "x\r\ny")], ("file", "f.bin", "x/y"), b"\x00\xFF");
        assert_eq!(content_type, "multipart/form-data; boundary=atroxabot-voice-boundary");
        let expected = b"--atroxabot-voice-boundary\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n\
            --atroxabot-voice-boundary\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\nxy\r\n\
            --atroxabot-voice-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.bin\"\r\n\
            Content-Type: x/y\r\n\r\n\x00\xFF\r\n--atroxabot-voice-boundary--\r\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn multipart_boundary_avoids_the_data() {
        let (content_type, body) = multipart(&[], ("file", "f", "x/y"), b"..atroxabot-voice-boundary..");
        assert_eq!(content_type, "multipart/form-data; boundary=atroxabot-voice-boundary-1");
        assert!(body.ends_with(b"\r\n--atroxabot-voice-boundary-1--\r\n"));
    }

    #[test]
    fn fish_voices_are_parsed_and_filtered() {
        let body = r#"{"total": 4, "items": [
            {"_id": "a1", "type": "tts", "title": "Ann", "description": "Warm", "state": "trained"},
            {"_id": "b2", "type": "svc", "title": "Singer", "state": "trained"},
            {"_id": "c3", "type": "tts", "title": "Busy", "state": "training"},
            {"_id": "d4", "title": "Bare"}
        ]}"#;
        let voices = parse_voices(Provider::Fish, body).unwrap();
        assert_eq!(
            voices,
            [
                Voice { id: "a1".into(), name: "Ann".into(), description: "Warm".into() },
                Voice { id: "d4".into(), name: "Bare".into(), description: String::new() },
            ]
        );
    }

    #[test]
    fn elevenlabs_voices_are_parsed() {
        let body = r#"{"voices": [
            {"voice_id": "v1", "name": "George", "description": "Warm storyteller", "category": "premade"},
            {"voice_id": "v2", "name": "Clone", "description": null}
        ], "has_more": false, "total_count": 2, "next_page_token": null}"#;
        let voices = parse_voices(Provider::ElevenLabs, body).unwrap();
        assert_eq!(
            voices,
            [
                Voice { id: "v1".into(), name: "George".into(), description: "Warm storyteller".into() },
                Voice { id: "v2".into(), name: "Clone".into(), description: String::new() },
            ]
        );
    }

    #[test]
    fn unexpected_answers_are_errors() {
        assert!(matches!(parse_voices(Provider::Fish, "<html>"), Err(Error::Response { .. })));
        assert!(matches!(parse_voices(Provider::ElevenLabs, r#"{"items": []}"#), Err(Error::Response { .. })));
        assert!(matches!(parse_transcript(Provider::Fish, r#"{"duration": 1}"#), Err(Error::Response { .. })));
    }

    #[test]
    fn transcripts_are_parsed_and_cleaned() {
        let fish = r#"{"text": "<|speaker:0|>Hello [laughter] there. <|speaker:1|> [music]  Hi!", "duration": 1.5, "segments": []}"#;
        assert_eq!(parse_transcript(Provider::Fish, fish).unwrap(), "Hello there. Hi!");
        let elevenlabs =
            r#"{"language_code": "nld", "language_probability": 0.98, "text": " Hallo daar. ", "words": []}"#;
        assert_eq!(parse_transcript(Provider::ElevenLabs, elevenlabs).unwrap(), "Hallo daar.");
        assert_eq!(clean_transcript("[noise]"), "");
        assert_eq!(clean_transcript("a<|speaker:x|>b <|speaker:12|>c"), "a <|speaker:x|>b c");
    }

    #[test]
    fn error_messages_come_from_the_provider() {
        assert_eq!(error_message(r#"{"status": 402, "message": "Insufficient balance"}"#), "Insufficient balance");
        let elevenlabs = r#"{"detail": {"status": "invalid_api_key", "message": "Invalid API key"}}"#;
        assert_eq!(error_message(elevenlabs), "Invalid API key");
        assert_eq!(error_message(r#"{"detail": "Not Found"}"#), "Not Found");
        let validation = r#"{"detail": [{"loc": ["body", "text"], "msg": "field required", "type": "missing"}]}"#;
        assert_eq!(error_message(validation), "field required");
        assert_eq!(error_message(" Bad Gateway "), "Bad Gateway");
        assert_eq!(error_message(&"x".repeat(1000)).len(), 300);
    }

    #[test]
    fn pcm_bytes_split_anywhere_keep_samples_aligned() {
        let pcm: Vec<i16> = (-50..50).collect();
        let bytes: Vec<u8> = pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect();
        let (mut carry, mut samples) = (None, Vec::new());
        for chunk in [&bytes[..7], &bytes[7..8], &bytes[8..8], &bytes[8..51], &bytes[51..]] {
            decode_pcm(&mut carry, chunk, &mut samples);
        }
        assert_eq!(samples, pcm);
        assert_eq!(carry, None);
    }

    #[test]
    fn calls_without_a_key_fail_before_any_request() {
        let config = VoiceConfig::default();
        assert!(matches!(voices(&config), Err(Error::MissingKey(Provider::Fish))));
        assert!(matches!(transcribe(&config, &vec![0; 16_000]), Err(Error::MissingKey(_))));
        let cancel = AtomicBool::new(false);
        assert!(matches!(synthesize(&config, "Hi", &cancel, &mut |_| {}), Err(Error::MissingKey(_))));
    }

    /// Lists voices, speaks a sentence and transcribes it again. Skips (passes) without the key.
    fn live_round_trip(provider: Provider, key_variable: &str) {
        let Ok(api_key) = std::env::var(key_variable) else {
            eprintln!("{key_variable} is not set: skipped");
            return;
        };
        let config = VoiceConfig { provider, api_key, language: "en".into(), ..VoiceConfig::default() };
        let listed = voices(&config).unwrap();
        assert!(!listed.is_empty());

        let mut speech = Vec::new();
        let cancel = AtomicBool::new(false);
        synthesize(&config, "The quick brown fox jumps over the lazy dog.", &cancel, &mut |pcm| {
            speech.extend_from_slice(pcm);
        })
        .unwrap();
        assert!(speech.len() > PLAYBACK_RATE as usize, "less than a second of audio");

        let mut pcm_16k = Vec::new();
        Resampler::new(PLAYBACK_RATE, turn::RATE).process(&speech, &mut pcm_16k);
        let transcript = transcribe(&config, &pcm_16k).unwrap().to_lowercase();
        assert!(transcript.contains("fox"), "transcript: {transcript}");
    }

    /// Free: checks TLS, the request plumbing and error parsing against the real hosts.
    #[test]
    #[ignore = "live: needs a network; sends a made-up key to both providers"]
    fn live_wrong_key_is_an_api_error() {
        for provider in Provider::ALL {
            let config =
                VoiceConfig { provider, api_key: "atroxabot-voice-test-not-a-key".into(), ..Default::default() };
            let cancel = AtomicBool::new(false);
            let results = [
                voices(&config).map(drop),
                transcribe(&config, &vec![0; 16_000]).map(drop),
                synthesize(&config, "Hello.", &cancel, &mut |_| {}),
            ];
            for result in results {
                let Err(Error::Api { status, message, .. }) = result else { panic!("{provider:?}: {result:?}") };
                assert!(status == 401 || status == 402 || status == 403, "{provider:?}: {status} {message}");
                assert!(!message.is_empty() && !message.contains(&config.api_key));
            }
        }
    }

    #[test]
    #[ignore = "live: makes paid Fish Audio calls when FISH_API_KEY is set"]
    fn live_fish_round_trip() {
        live_round_trip(Provider::Fish, "FISH_API_KEY");
    }

    #[test]
    #[ignore = "live: makes paid ElevenLabs calls when ELEVENLABS_API_KEY is set"]
    fn live_elevenlabs_round_trip() {
        live_round_trip(Provider::ElevenLabs, "ELEVENLABS_API_KEY");
    }
}

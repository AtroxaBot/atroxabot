//! Voice-activity detection and utterance segmentation: pure logic, fed frames of microphone audio.

use std::collections::VecDeque;

use crate::CallOptions;
use crate::dsp::rms;

/// The sample rate of the frames, in Hz.
pub(crate) const RATE: u32 = 16_000;
/// The frame length the detector is tuned for: 30 ms.
pub(crate) const FRAME_SAMPLES: usize = 480;
/// Recent frames that are looked at to start an utterance; they also become its pre-roll (360 ms).
const START_WINDOW: usize = 12;
/// Loud frames in the window that start an utterance.
const START_LOUD: usize = 7;
/// The same while the agent is busy, so a click or a thud does not interrupt it.
const START_LOUD_BUSY: usize = 10;
/// Inside an utterance a frame this much quieter than `speech_rms` still counts as speech (hysteresis).
const CONTINUE_RATIO: f32 = 0.6;
/// Silence after which transcription may start early, in case the user is done.
const EARLY_MS: u64 = 400;
/// An utterance with less speech than this is noise ("yes" is about 240 ms).
const MIN_SPEECH_MS: u64 = 240;
/// An utterance is cut off at 30 seconds.
const MAX_SAMPLES: usize = RATE as usize * 30;

/// What a frame did to the turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TurnEvent {
    /// The user started talking.
    Started,
    /// The user paused; this is the audio so far, for an early transcription.
    Paused(Vec<i16>),
    /// The user finished. `early`: nothing was said since the last [`TurnEvent::Paused`], so its audio holds.
    Ended { audio: Vec<i16>, early: bool },
}

struct Utterance {
    audio: Vec<i16>,
    speech_ms: u64,
    last_speech_ms: u64,
    paused: bool,
}

/// Energy-based turn detector with hysteresis.
pub(crate) struct TurnDetector {
    speech_rms: f32,
    barge_in_rms: f32,
    end_silence_ms: u64,
    barge_in: bool,
    recent: VecDeque<(bool, Vec<i16>)>,
    utterance: Option<Utterance>,
}

impl TurnDetector {
    pub(crate) fn new(options: &CallOptions) -> Self {
        Self {
            speech_rms: options.speech_rms,
            barge_in_rms: options.barge_in_rms,
            end_silence_ms: u64::from(options.end_silence_ms),
            barge_in: options.barge_in,
            recent: VecDeque::with_capacity(START_WINDOW),
            utterance: None,
        }
    }

    /// Is the user talking right now?
    pub(crate) const fn hearing(&self) -> bool {
        self.utterance.is_some()
    }

    /// Feeds one frame of 16 kHz mono audio that ends at `now_ms`.
    ///
    /// `busy`: the agent is thinking or speaking, so starting an utterance is a barge-in and takes louder
    /// and longer input (or is not possible at all when barge-in is off).
    pub(crate) fn push(&mut self, frame: &[i16], now_ms: u64, busy: bool) -> Option<TurnEvent> {
        let level = rms(frame);
        let frame_ms = frame.len() as u64 * 1000 / u64::from(RATE);
        let Some(utterance) = &mut self.utterance else {
            if busy && !self.barge_in {
                self.recent.clear();
                return None;
            }
            let (threshold, need) =
                if busy { (self.barge_in_rms, START_LOUD_BUSY) } else { (self.speech_rms, START_LOUD) };
            if self.recent.len() >= START_WINDOW {
                self.recent.pop_front();
            }
            self.recent.push_back((level >= threshold, frame.to_vec()));
            let loud = self.recent.iter().filter(|(loud, _)| *loud).count();
            if loud < need {
                return None;
            }
            let audio = self.recent.drain(..).flat_map(|(_, frame)| frame).collect();
            self.utterance =
                Some(Utterance { audio, speech_ms: loud as u64 * frame_ms, last_speech_ms: now_ms, paused: false });
            return Some(TurnEvent::Started);
        };
        utterance.audio.extend_from_slice(frame);
        if level >= self.speech_rms * CONTINUE_RATIO {
            utterance.speech_ms += frame_ms;
            utterance.last_speech_ms = now_ms;
            utterance.paused = false;
        }
        let silence_ms = now_ms.saturating_sub(utterance.last_speech_ms);
        let is_speech = utterance.speech_ms >= MIN_SPEECH_MS;
        if silence_ms >= self.end_silence_ms || utterance.audio.len() >= MAX_SAMPLES {
            let ended = self.utterance.take()?;
            return is_speech.then_some(TurnEvent::Ended { audio: ended.audio, early: ended.paused });
        }
        if is_speech && !utterance.paused && silence_ms >= EARLY_MS {
            utterance.paused = true;
            return Some(TurnEvent::Paused(utterance.audio.clone()));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: [i16; FRAME_SAMPLES] = [0; FRAME_SAMPLES];
    const LOUD: [i16; FRAME_SAMPLES] = [2000; FRAME_SAMPLES];
    const FRAME_MS: u64 = 30;

    struct Feed {
        detector: TurnDetector,
        now_ms: u64,
    }

    impl Feed {
        fn new(options: &CallOptions) -> Self {
            Self { detector: TurnDetector::new(options), now_ms: 0 }
        }

        fn frames(&mut self, frame: &[i16], count: usize, busy: bool) -> Vec<TurnEvent> {
            let mut events = Vec::new();
            for _ in 0..count {
                self.now_ms += FRAME_MS;
                events.extend(self.detector.push(frame, self.now_ms, busy));
            }
            events
        }
    }

    /// Feeds runs of loud (`true`) and quiet frames while the agent is idle.
    fn feed(pattern: &[(bool, usize)]) -> Vec<TurnEvent> {
        let mut feed = Feed::new(&CallOptions::default());
        let mut events = Vec::new();
        for &(loud, count) in pattern {
            events.extend(feed.frames(if loud { &LOUD } else { &QUIET }, count, false));
        }
        events
    }

    fn kinds(events: &[TurnEvent]) -> Vec<&'static str> {
        let kind = |event: &TurnEvent| match event {
            TurnEvent::Started => "speech",
            TurnEvent::Paused(_) => "pause",
            TurnEvent::Ended { .. } => "utterance",
        };
        events.iter().map(kind).collect()
    }

    #[test]
    fn normal_utterance_gives_speech_pause_then_utterance_with_preroll() {
        let events = feed(&[(false, 3), (true, 20), (false, 30)]);
        assert_eq!(kinds(&events), ["speech", "pause", "utterance"]);
        let (TurnEvent::Paused(pause), TurnEvent::Ended { audio, early }) = (&events[1], &events[2]) else {
            panic!("unexpected events");
        };
        assert_eq!(audio[..4 * FRAME_SAMPLES], [QUIET, QUIET, QUIET, LOUD].concat());
        // 3 pre-roll frames + 20 loud + 700 ms of silence (24 frames of 30 ms)
        assert_eq!(audio.len(), (3 + 20 + 24) * FRAME_SAMPLES);
        // The pause comes after 400 ms (14 frames) of silence; nothing was said after it.
        assert_eq!(pause.len(), (3 + 20 + 14) * FRAME_SAMPLES);
        assert!(*early && audio.starts_with(pause));
    }

    #[test]
    fn talking_again_after_a_pause_gives_a_new_pause() {
        let events = feed(&[(true, 20), (false, 15), (true, 5), (false, 30)]);
        assert_eq!(kinds(&events), ["speech", "pause", "pause", "utterance"]);
        let (TurnEvent::Paused(first), TurnEvent::Paused(second)) = (&events[1], &events[2]) else {
            panic!("unexpected events");
        };
        assert!(second.len() > first.len());
        assert!(matches!(events[3], TurnEvent::Ended { early: true, .. }));
    }

    #[test]
    fn talking_on_after_a_pause_makes_the_early_audio_stale() {
        let mut feed = Feed::new(&CallOptions { end_silence_ms: 3000, ..CallOptions::default() });
        let mut events = feed.frames(&LOUD, 20, false);
        events.extend(feed.frames(&QUIET, 15, false));
        events.extend(feed.frames(&LOUD, 965, false)); // up to the 30 second limit
        assert_eq!(kinds(&events), ["speech", "pause", "utterance"]);
        assert!(matches!(events[2], TurnEvent::Ended { early: false, .. }));
    }

    #[test]
    fn one_short_word_is_an_utterance() {
        // "yes": about 240 ms of voice
        let events = feed(&[(false, 5), (true, 8), (false, 30)]);
        assert_eq!(kinds(&events), ["speech", "pause", "utterance"]);
        let TurnEvent::Ended { audio, .. } = &events[2] else { panic!("unexpected event") };
        assert_eq!(audio.iter().filter(|&&sample| sample == 2000).count(), 8 * FRAME_SAMPLES);
    }

    #[test]
    fn a_thud_starts_but_is_dropped_as_too_short() {
        let mut feed = Feed::new(&CallOptions::default());
        let mut events = feed.frames(&LOUD, 7, false);
        assert_eq!(events, [TurnEvent::Started]);
        assert!(feed.detector.hearing());
        events.extend(feed.frames(&QUIET, 40, false));
        assert_eq!(events, [TurnEvent::Started]);
        assert!(!feed.detector.hearing());
    }

    #[test]
    fn a_click_never_starts() {
        assert!(feed(&[(true, 3), (false, 40)]).is_empty());
    }

    #[test]
    fn scattered_noise_never_starts() {
        let pattern: Vec<(bool, usize)> = (0..400).map(|i| (i % 2 == 0, 1)).collect();
        assert!(feed(&pattern).is_empty());
    }

    #[test]
    fn over_speech_a_short_burst_needs_more_voice_to_start() {
        // 240 ms: a word, but only a thud while the agent is talking
        let mut busy = Feed::new(&CallOptions::default());
        assert!(busy.frames(&LOUD, 8, true).is_empty());
        assert!(busy.frames(&QUIET, 40, true).is_empty());
        assert_eq!(busy.frames(&LOUD, 10, true), [TurnEvent::Started]);
    }

    #[test]
    fn over_speech_input_must_be_louder_than_the_barge_in_level() {
        let normal = [1000; FRAME_SAMPLES]; // above speech_rms 300, below barge_in_rms 1500
        let mut feed = Feed::new(&CallOptions::default());
        assert!(feed.frames(&normal, 40, true).is_empty());
        assert_eq!(feed.frames(&normal, 7, false), [TurnEvent::Started]);
    }

    #[test]
    fn without_barge_in_nothing_starts_while_the_agent_is_busy() {
        let mut feed = Feed::new(&CallOptions { barge_in: false, ..CallOptions::default() });
        assert!(feed.frames(&LOUD, 40, true).is_empty());
        assert!(!feed.detector.hearing());
        assert_eq!(feed.frames(&LOUD, 7, false), [TurnEvent::Started]);
    }

    #[test]
    fn hysteresis_keeps_a_quiet_tail_but_does_not_start_on_it() {
        let soft = [200; FRAME_SAMPLES]; // below speech_rms 300, above 0.6 of it
        let mut feed = Feed::new(&CallOptions::default());
        assert!(feed.frames(&soft, 40, false).is_empty());
        assert_eq!(feed.frames(&LOUD, 10, false), [TurnEvent::Started]);
        assert!(feed.frames(&soft, 40, false).is_empty());
        assert_eq!(kinds(&feed.frames(&QUIET, 24, false)), ["pause", "utterance"]);
    }

    #[test]
    fn max_length_ends_at_30_seconds() {
        let events = feed(&[(true, 1003)]);
        assert_eq!(kinds(&events), ["speech", "utterance"]);
        let TurnEvent::Ended { audio, early } = &events[1] else { panic!("unexpected event") };
        assert_eq!(audio.len(), 1000 * FRAME_SAMPLES);
        assert!(!early);
    }

    #[test]
    fn end_silence_shorter_than_the_early_pause_skips_the_pause() {
        let mut feed = Feed::new(&CallOptions { end_silence_ms: 300, ..CallOptions::default() });
        let mut events = feed.frames(&LOUD, 20, false);
        events.extend(feed.frames(&QUIET, 30, false));
        assert_eq!(kinds(&events), ["speech", "utterance"]);
    }
}

//! A voice call: the microphone is listened to, what the user says is transcribed, and the agent's reply is
//! spoken sentence by sentence as it streams in.
//!
//! Three kinds of threads, all blocking:
//! - the call thread owns the audio devices, runs turn detection and emits every [`CallEvent`];
//! - the speaker thread turns queued sentences into audio, one streaming request at a time;
//! - one short-lived thread per transcription.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::audio::{self, Io, lock};
use crate::dsp::{Resampler, rms};
use crate::textfilter::{SpeechFilter, pieces};
use crate::turn::{self, FRAME_SAMPLES, TurnDetector, TurnEvent};
use crate::{Error, PLAYBACK_RATE, VoiceConfig, provider};

/// How often the call thread looks at the microphone.
const TICK: Duration = Duration::from_millis(10);
const FRAME_MS: u64 = 30;
/// At most about 30 level events per second.
const LEVEL_INTERVAL: Duration = Duration::from_millis(33);
/// The call counts as speaking this long after the last audio was queued: what the device still holds.
const SPEECH_TAIL: Duration = Duration::from_millis(250);
/// Audio gathered before playback starts from silence, so network jitter does not make it stutter.
const PREBUFFER_MS: usize = 150;
/// The next sentence is not requested while more than this much speech is waiting to be played, so a
/// barge-in throws away little paid-for audio.
const BACKLOG_SECONDS: usize = 5;
/// One synthesis request carries at most this many bytes of text (about 20 seconds of speech): waiting
/// sentences are sent together up to it, longer text is split. That keeps what a barge-in throws away
/// small and what one request streams back within the playback queue.
const REQUEST_BYTES: usize = 300;
const SILENCE: [i16; FRAME_SAMPLES] = [0; FRAME_SAMPLES];

/// Devices and thresholds of a call. The levels are RMS values on the 16-bit sample scale.
#[derive(Debug, Clone)]
pub struct CallOptions {
    /// The microphone, by [`Device::name`](crate::Device::name). `None`: the system default.
    pub input_device: Option<String>,
    /// The speakers, by [`Device::name`](crate::Device::name). `None`: the system default.
    pub output_device: Option<String>,
    /// Microphone input at least this loud is speech.
    pub speech_rms: f32,
    /// While the agent thinks or speaks, input must be this loud (for about 300 ms) to interrupt it.
    pub barge_in_rms: f32,
    /// This much silence ends what the user is saying.
    pub end_silence_ms: u32,
    /// May the user talk over the agent? If not, the microphone is ignored while it thinks or speaks.
    pub barge_in: bool,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            speech_rms: 300.0,
            barge_in_rms: 1500.0,
            end_silence_ms: 700,
            barge_in: true,
        }
    }
}

/// What the call is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Waiting for the user to say something.
    Listening,
    /// The user is speaking.
    Hearing,
    /// What the user said is being transcribed.
    Transcribing,
    /// Waiting for the agent's text.
    Thinking,
    /// The agent's reply is being spoken.
    Speaking,
}

/// Something that happened in a call. Events arrive in order, on the call's own thread.
#[derive(Debug, Clone, PartialEq)]
pub enum CallEvent {
    /// The call's state changed.
    State(CallState),
    /// Meter levels in 0..=1 for the UI's orb, at most ~30 per second.
    Level {
        /// How loud the microphone is.
        input: f32,
        /// How loud the agent's voice is.
        output: f32,
    },
    /// The user finished saying something: send it to the agent.
    Utterance(String),
    /// The user talked over the agent: playback was stopped and pending speech dropped; the app should
    /// interrupt the agent's reply.
    BargeIn,
    /// A recoverable problem (one failed transcription/synthesis); the call goes on.
    Problem(String),
    /// The call ended by itself (device lost); nothing follows.
    Ended(Option<String>),
}

enum Message {
    Transcript(u64, Result<String, Error>),
    Problem(String),
    DeviceLost(String),
}

/// The agent's reply on its way to the speaker thread.
struct Speech {
    filter: SpeechFilter,
    /// Sentences waiting to be synthesized.
    pending: VecDeque<String>,
    /// A reply is being fed in: `say` was called and `finish_reply` not yet.
    replying: bool,
    /// The speaker thread is synthesizing.
    busy: bool,
    /// An utterance went to the app and its reply has not started.
    thinking: bool,
    /// The user barged in: the rest of the interrupted reply must not be spoken.
    hush: bool,
}

impl Speech {
    fn queue(&mut self, chunks: &[String]) {
        let texts = chunks.iter().flat_map(|chunk| pieces(chunk, REQUEST_BYTES - 1));
        // Like the filter's chunks, every piece ends with one space, so pieces can be sent together.
        self.pending.extend(texts.map(|text| format!("{} ", text.trim_end())));
    }
}

struct Shared {
    stop: AtomicBool,
    muted: AtomicBool,
    // Lock order: `speech` before `io.playback`.
    speech: Mutex<Speech>,
    /// Wakes the speaker thread.
    wake: Condvar,
    io: Arc<Io>,
}

impl Shared {
    fn new() -> Self {
        let speech = Speech {
            filter: SpeechFilter::new(),
            pending: VecDeque::new(),
            replying: false,
            busy: false,
            thinking: false,
            hush: false,
        };
        Self {
            stop: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            speech: Mutex::new(speech),
            wake: Condvar::new(),
            io: Arc::new(Io::new()),
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake.notify_all();
    }

    fn say(&self, text_delta: &str) {
        let mut speech = lock(&self.speech);
        if speech.hush || self.stopped() {
            return;
        }
        speech.thinking = false;
        speech.replying = true;
        let chunks = speech.filter.feed(text_delta);
        speech.queue(&chunks);
        drop(speech);
        self.wake.notify_all();
    }

    fn finish_reply(&self) {
        let mut speech = lock(&self.speech);
        let rest = std::mem::replace(&mut speech.filter, SpeechFilter::new()).flush();
        if !self.stopped() {
            speech.queue(&rest);
        }
        speech.replying = false;
        speech.thinking = false;
        speech.hush = false;
        drop(speech);
        self.wake.notify_all();
    }

    /// Drops queued and playing speech. `hush`: also ignore `say` until the reply is over.
    fn flush(&self, hush: bool) {
        let mut speech = lock(&self.speech);
        speech.filter = SpeechFilter::new();
        speech.pending.clear();
        speech.replying = false;
        speech.busy = false;
        speech.thinking = false;
        speech.hush = hush;
        let mut playback = lock(&self.io.playback);
        playback.generation += 1;
        playback.queue.clear();
    }

    /// Moves `held` into the playback queue. Returns false when its speech was dropped meanwhile.
    ///
    /// When nothing is playing, audio is held back until there is `prebuffer` of it, or `last` says no more
    /// is coming. Waits while the queue is full.
    fn play(&self, generation: u64, held: &mut Vec<i16>, prebuffer: usize, last: bool) -> bool {
        let mut queued = 0;
        while !self.stopped() {
            {
                let mut playback = lock(&self.io.playback);
                if playback.generation != generation {
                    break;
                }
                if playback.queue.is_empty() && held.len() < prebuffer && !last {
                    return true;
                }
                let room = playback.queue.capacity().saturating_sub(playback.queue.len());
                let fitting = held.iter().skip(queued).take(room);
                queued += fitting.len();
                playback.queue.extend(fitting);
            }
            if queued >= held.len() {
                held.clear();
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        held.clear();
        false
    }
}

/// A running voice call. Dropping it ends the call.
pub struct Call {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Call {
    /// Opens the audio devices and starts listening.
    ///
    /// `on_event` is called on the call's own thread, never from inside one of the methods below. It must
    /// return quickly: while it runs, the call does not listen. Calling methods of the call from it is fine.
    ///
    /// # Errors
    /// When the key is missing, a device cannot be found or opened, or a thread cannot be started.
    pub fn start(
        config: VoiceConfig,
        options: CallOptions,
        on_event: impl Fn(CallEvent) + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        if config.api_key.is_empty() {
            return Err(Error::MissingKey(config.provider));
        }
        let shared = Arc::new(Shared::new());
        let config = Arc::new(config);
        let (sender, receiver) = mpsc::channel();
        let (ready, opened) = mpsc::channel();
        let call_thread = {
            let engine = Engine::new(Arc::clone(&shared), Arc::clone(&config), &options, on_event, sender.clone());
            let sender = sender.clone();
            thread::Builder::new()
                .name("voice-call".to_owned())
                .spawn(move || run(engine, &options, &sender, &receiver, &ready))
                .map_err(Error::Thread)?
        };
        let call = Self { shared, thread: Mutex::new(Some(call_thread)) };
        // The call thread answers once the devices are open, or drops `ready` if it cannot.
        let output_rate = opened.recv().map_err(|_| Error::Device("the audio thread stopped".to_owned()))??;
        let shared = Arc::clone(&call.shared);
        thread::Builder::new()
            .name("voice-speak".to_owned())
            .spawn(move || speak(&shared, &config, output_rate, &sender))
            .map_err(Error::Thread)?;
        Ok(call)
    }

    /// More of the agent's streamed reply; spoken sentence by sentence as it arrives (markdown/code
    /// filtered out).
    ///
    /// After a [`CallEvent::BargeIn`] this does nothing until the next [`CallEvent::Utterance`],
    /// [`finish_reply`](Self::finish_reply) or [`stop_speaking`](Self::stop_speaking), so the tail of the
    /// interrupted reply stays unsaid.
    pub fn say(&self, text_delta: &str) {
        self.shared.say(text_delta);
    }

    /// The agent's reply is complete: speak what remains, then go back to Listening.
    pub fn finish_reply(&self) {
        self.shared.finish_reply();
    }

    /// Drop queued and playing speech now (the reply was cancelled).
    pub fn stop_speaking(&self) {
        self.shared.flush(false);
    }

    /// Mutes or unmutes the microphone. Muted, the user is not heard and cannot barge in.
    pub fn set_muted(&self, muted: bool) {
        self.shared.muted.store(muted, Ordering::Relaxed);
    }

    /// Ends the call: stops the threads and closes the devices. Does nothing the second time.
    ///
    /// No event follows. Requests that are in flight are abandoned, not waited for.
    pub fn end(&self) {
        self.shared.stop();
        let call_thread = lock(&self.thread).take();
        // Called from `on_event`, this is the call thread itself: it stops when the callback returns.
        if let Some(call_thread) = call_thread
            && call_thread.thread().id() != thread::current().id()
        {
            // The thread only fails by panicking, and then it is gone as well.
            let _ = call_thread.join();
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.end();
    }
}

/// Maps an RMS level to 0..=1 for a meter; the square root keeps quiet speech visible.
fn meter(rms: f32) -> f32 {
    (rms / 10_000.0).sqrt().min(1.0)
}

/// The call thread's state: turn detection, transcription bookkeeping and the events.
struct Engine {
    shared: Arc<Shared>,
    config: Arc<VoiceConfig>,
    on_event: Box<dyn Fn(CallEvent) + Send + Sync>,
    sender: Sender<Message>,
    detector: TurnDetector,
    /// Audio time: 30 ms per frame.
    clock_ms: u64,
    state: CallState,
    next_job: u64,
    /// The transcription that was started at the user's last pause.
    early: Option<u64>,
    /// Transcriptions of finished utterances, in the order they were spoken.
    awaiting: VecDeque<u64>,
    /// Results that arrived before their turn.
    transcripts: Vec<(u64, Result<String, Error>)>,
    /// When speech was last queued or playing.
    last_speech: Option<Instant>,
    generation: u64,
    input_peak: f32,
    last_level: Instant,
    levels_were_zero: bool,
}

impl Engine {
    fn new(
        shared: Arc<Shared>,
        config: Arc<VoiceConfig>,
        options: &CallOptions,
        on_event: impl Fn(CallEvent) + Send + Sync + 'static,
        sender: Sender<Message>,
    ) -> Self {
        Self {
            shared,
            config,
            on_event: Box::new(on_event),
            sender,
            detector: TurnDetector::new(options),
            clock_ms: 0,
            state: CallState::Listening,
            next_job: 0,
            early: None,
            awaiting: VecDeque::new(),
            transcripts: Vec::new(),
            last_speech: None,
            generation: 0,
            input_peak: 0.0,
            last_level: Instant::now(),
            levels_were_zero: true,
        }
    }

    fn emit(&self, event: CallEvent) {
        (self.on_event)(event);
    }

    /// Is the agent `(speaking, thinking)`?
    fn agent(&mut self) -> (bool, bool) {
        let (queued, generation) = {
            let playback = lock(&self.shared.io.playback);
            (!playback.queue.is_empty(), playback.generation)
        };
        let (active, thinking) = {
            let speech = lock(&self.shared.speech);
            (speech.replying || speech.busy || !speech.pending.is_empty(), speech.thinking)
        };
        if generation != self.generation {
            // Speech was dropped: nothing of it is still on its way out.
            self.generation = generation;
            self.last_speech = None;
        }
        if queued || active {
            self.last_speech = Some(Instant::now());
        }
        (self.last_speech.is_some_and(|at| at.elapsed() < SPEECH_TAIL), thinking)
    }

    /// One 30 ms frame of 16 kHz microphone audio.
    fn frame(&mut self, frame: &[i16]) {
        let frame: &[i16] = if self.shared.muted.load(Ordering::Relaxed) { &SILENCE } else { frame };
        self.clock_ms += FRAME_MS;
        self.input_peak = self.input_peak.max(rms(frame));
        let (speaking, thinking) = self.agent();
        let busy = speaking || thinking;
        match self.detector.push(frame, self.clock_ms, busy) {
            Some(TurnEvent::Started) if busy => {
                self.shared.flush(true);
                self.emit(CallEvent::BargeIn);
            }
            Some(TurnEvent::Started) | None => {}
            Some(TurnEvent::Paused(audio)) => {
                self.forget_early();
                self.early = self.transcribe(audio);
            }
            Some(TurnEvent::Ended { audio, early }) => {
                let job = if early && self.early.is_some() {
                    self.early.take()
                } else {
                    self.forget_early();
                    self.transcribe(audio)
                };
                self.awaiting.extend(job);
                self.deliver();
            }
        }
    }

    fn forget_early(&mut self) {
        if let Some(stale) = self.early.take() {
            self.transcripts.retain(|(job, _)| *job != stale);
        }
    }

    /// Starts a transcription on its own thread; the result comes back as a [`Message::Transcript`].
    fn transcribe(&mut self, audio: Vec<i16>) -> Option<u64> {
        let job = self.next_job;
        self.next_job += 1;
        let (config, sender) = (Arc::clone(&self.config), self.sender.clone());
        let spawned = thread::Builder::new().name("voice-transcribe".to_owned()).spawn(move || {
            // The receiver is gone when the call has ended.
            let _ = sender.send(Message::Transcript(job, provider::transcribe(&config, &audio)));
        });
        match spawned {
            Ok(_) => Some(job),
            Err(error) => {
                self.emit(CallEvent::Problem(format!("speech recognition failed: {}", Error::Thread(error))));
                None
            }
        }
    }

    /// Handles a message from another thread. Returns false when the call is over.
    fn message(&mut self, message: Message) -> bool {
        match message {
            Message::Transcript(job, result) => {
                // Anything else is an early transcription the user talked on after.
                if self.early == Some(job) || self.awaiting.contains(&job) {
                    self.transcripts.push((job, result));
                    self.deliver();
                }
            }
            Message::Problem(problem) => self.emit(CallEvent::Problem(problem)),
            Message::DeviceLost(reason) => {
                self.shared.stop();
                self.emit(CallEvent::Ended(Some(reason)));
                return false;
            }
        }
        true
    }

    /// Emits the utterances whose transcripts are in, in the order they were spoken.
    fn deliver(&mut self) {
        while let Some(&job) = self.awaiting.front() {
            let Some(index) = self.transcripts.iter().position(|(done, _)| *done == job) else { break };
            let (_, result) = self.transcripts.swap_remove(index);
            self.awaiting.pop_front();
            match result {
                Ok(text) if text.is_empty() => {}
                Ok(text) => {
                    {
                        let mut speech = lock(&self.shared.speech);
                        speech.thinking = true;
                        speech.hush = false;
                    }
                    self.emit(CallEvent::Utterance(text));
                }
                Err(error) => self.emit(CallEvent::Problem(format!("speech recognition failed: {error}"))),
            }
        }
    }

    /// Emits the state when it changed, and the meter levels when they are due.
    fn refresh(&mut self) {
        let (speaking, thinking) = self.agent();
        let state = if self.detector.hearing() {
            CallState::Hearing
        } else if !self.awaiting.is_empty() {
            CallState::Transcribing
        } else if speaking {
            CallState::Speaking
        } else if thinking {
            CallState::Thinking
        } else {
            CallState::Listening
        };
        if state != self.state {
            self.state = state;
            self.emit(CallEvent::State(state));
        }
        if self.last_level.elapsed() >= LEVEL_INTERVAL {
            let (input, output) = (meter(self.input_peak), meter(self.shared.io.output_level()));
            self.input_peak = 0.0;
            self.last_level = Instant::now();
            let zero = input <= 0.0 && output <= 0.0;
            if !(zero && self.levels_were_zero) {
                self.emit(CallEvent::Level { input, output });
            }
            self.levels_were_zero = zero;
        }
    }
}

/// The call thread: opens the devices, then feeds the engine until the call ends.
fn run(
    mut engine: Engine,
    options: &CallOptions,
    sender: &Sender<Message>,
    receiver: &Receiver<Message>,
    ready: &Sender<Result<u32, Error>>,
) {
    let shared = Arc::clone(&engine.shared);
    let on_lost = {
        let sender = sender.clone();
        move |reason: String| {
            // The receiver is gone when the call has ended.
            let _ = sender.send(Message::DeviceLost(reason));
        }
    };
    let opened = audio::open(&shared.io, options.input_device.as_deref(), options.output_device.as_deref(), on_lost);
    let streams = match opened {
        Ok(streams) => streams,
        Err(error) => {
            // `Call::start` is waiting for this.
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(streams.output_rate)).is_err() {
        return;
    }
    engine.emit(CallEvent::State(CallState::Listening));
    let mut resampler = Resampler::new(streams.input_rate, turn::RATE);
    let mut captured = Vec::new();
    let mut pcm_16k = Vec::new();
    while !shared.stopped() {
        match receiver.recv_timeout(TICK) {
            Ok(message) => {
                if !engine.message(message) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        captured.clear();
        captured.extend(lock(&shared.io.capture).drain(..));
        resampler.process(&captured, &mut pcm_16k);
        let (frames, rest) = pcm_16k.as_chunks::<FRAME_SAMPLES>();
        for frame in frames {
            engine.frame(frame);
        }
        let rest = rest.len();
        pcm_16k.drain(..pcm_16k.len() - rest);
        engine.refresh();
    }
    drop(streams);
}

/// The speaker thread: synthesizes waiting sentences in order and queues their audio for playback.
// ponytail: one request at a time. The next one starts as soon as the previous has been received, which is
// while it still plays as long as the provider streams faster than real time. Run two requests in
// parallel if gaps between sentences show up.
fn speak(shared: &Shared, config: &VoiceConfig, output_rate: u32, sender: &Sender<Message>) {
    let prebuffer = output_rate as usize * PREBUFFER_MS / 1000;
    let backlog = output_rate as usize * BACKLOG_SECONDS;
    while let Some((text, generation)) = next_text(shared, backlog) {
        let cancel = AtomicBool::new(false);
        let mut resampler = Resampler::new(PLAYBACK_RATE, output_rate);
        let mut held = Vec::new();
        let result = provider::synthesize(config, &text, &cancel, &mut |pcm| {
            resampler.process(pcm, &mut held);
            if !shared.play(generation, &mut held, prebuffer, false) {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        let wanted = shared.play(generation, &mut held, prebuffer, true);
        if let Err(error) = result
            && wanted
        {
            // The receiver is gone when the call has ended.
            let _ = sender.send(Message::Problem(format!("speech synthesis failed: {error}")));
        }
    }
}

/// Waits for text to speak. Returns it with the playback generation it belongs to, or `None` when the
/// call has ended.
fn next_text(shared: &Shared, backlog: usize) -> Option<(String, u64)> {
    let mut speech = lock(&shared.speech);
    loop {
        if shared.stopped() {
            return None;
        }
        let (queued, generation) = {
            let playback = lock(&shared.io.playback);
            (playback.queue.len(), playback.generation)
        };
        if queued < backlog && !speech.pending.is_empty() {
            let mut text = String::new();
            while speech.pending.front().is_some_and(|next| text.is_empty() || text.len() + next.len() <= REQUEST_BYTES)
            {
                text.extend(speech.pending.pop_front());
            }
            speech.busy = true;
            return Some((text, generation));
        }
        speech.busy = false;
        // `say` wakes this up; the timeout is for the backlog check and the end of the call.
        speech = match shared.wake.wait_timeout(speech, Duration::from_millis(50)) {
            Ok((speech, _)) => speech,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOUD: [i16; FRAME_SAMPLES] = [2000; FRAME_SAMPLES];

    struct Harness {
        engine: Engine,
        shared: Arc<Shared>,
        receiver: Receiver<Message>,
        events: Arc<Mutex<Vec<CallEvent>>>,
    }

    impl Harness {
        /// An engine without devices or a key: every transcription fails at once, without a request.
        fn new(options: &CallOptions) -> Self {
            let shared = Arc::new(Shared::new());
            let (sender, receiver) = mpsc::channel();
            let events = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&events);
            let on_event = move |event| sink.lock().unwrap().push(event);
            let config = Arc::new(VoiceConfig::default());
            let engine = Engine::new(Arc::clone(&shared), config, options, on_event, sender);
            Self { engine, shared, receiver, events }
        }

        fn frames(&mut self, frame: &[i16], count: usize) {
            for _ in 0..count {
                self.engine.frame(frame);
            }
            self.engine.refresh();
        }

        /// Says a short sentence and lets the 700 ms of silence after it pass.
        fn utterance(&mut self) {
            self.frames(&LOUD, 20);
            self.frames(&SILENCE, 24);
        }

        /// Waits for the transcription that failed for want of a key and answers `transcript` in its place.
        fn transcribed(&mut self, transcript: &str) {
            let Message::Transcript(job, Err(Error::MissingKey(_))) = self.receiver.recv().unwrap() else {
                panic!("expected a failed transcription");
            };
            assert!(self.engine.message(Message::Transcript(job, Ok(transcript.to_owned()))));
            self.engine.refresh();
        }

        /// The events so far, without meter levels.
        fn events(&self) -> Vec<CallEvent> {
            let mut events = self.events.lock().unwrap();
            events.drain(..).filter(|event| !matches!(event, CallEvent::Level { .. })).collect()
        }

        fn pending(&self) -> Vec<String> {
            self.shared.speech.lock().unwrap().pending.iter().cloned().collect()
        }
    }

    fn state(state: CallState) -> CallEvent {
        CallEvent::State(state)
    }

    #[test]
    fn a_turn_goes_from_hearing_to_speaking_and_back_to_listening() {
        let mut call = Harness::new(&CallOptions::default());
        call.frames(&LOUD, 20);
        assert_eq!(call.events(), [state(CallState::Hearing)]);
        call.frames(&SILENCE, 24);
        assert_eq!(call.events(), [state(CallState::Transcribing)]);
        call.transcribed("What time is it?");
        assert_eq!(call.events(), [CallEvent::Utterance("What time is it?".into()), state(CallState::Thinking)]);

        call.shared.say("It is **noon**. And");
        call.engine.refresh();
        assert_eq!(call.events(), [state(CallState::Speaking)]);
        assert_eq!(call.pending(), ["It is noon. "]);
        call.shared.finish_reply();
        assert_eq!(call.pending(), ["It is noon. ", "And "]);

        // What the speaker thread does: take the text, queue its audio, and the device plays it.
        let (text, generation) = next_text(&call.shared, 1000).unwrap();
        assert_eq!(text, "It is noon. And ");
        call.shared.io.playback.lock().unwrap().queue.reserve_exact(64);
        assert!(call.shared.play(generation, &mut vec![1; 10], 100, true));
        call.shared.speech.lock().unwrap().busy = false;
        call.engine.refresh();
        assert!(call.events().is_empty(), "still speaking while audio is queued");
        call.shared.io.playback.lock().unwrap().queue.clear();
        thread::sleep(SPEECH_TAIL + Duration::from_millis(20));
        call.engine.refresh();
        assert_eq!(call.events(), [state(CallState::Listening)]);
    }

    #[test]
    fn talking_over_the_agent_is_a_barge_in() {
        let mut call = Harness::new(&CallOptions::default());
        call.shared.say("A long answer. More");
        call.shared.io.playback.lock().unwrap().queue.extend([1, 2, 3]);
        call.engine.refresh();
        assert_eq!(call.events(), [state(CallState::Speaking)]);

        call.frames(&LOUD, 9); // a thud is not enough
        assert!(call.events().is_empty());
        call.frames(&LOUD, 1);
        assert_eq!(call.events(), [CallEvent::BargeIn, state(CallState::Hearing)]);
        assert!(call.pending().is_empty());
        assert!(call.shared.io.playback.lock().unwrap().queue.is_empty());
        assert_eq!(call.shared.io.playback.lock().unwrap().generation, 1);

        // The rest of the interrupted reply stays unsaid.
        call.shared.say(" of the old reply. ");
        assert!(call.pending().is_empty());

        call.frames(&LOUD, 10);
        call.frames(&SILENCE, 24);
        assert_eq!(call.events(), [state(CallState::Transcribing)]);
        call.transcribed("Stop.");
        assert_eq!(call.events(), [CallEvent::Utterance("Stop.".into()), state(CallState::Thinking)]);
        call.shared.say("Okay. ");
        assert_eq!(call.pending(), ["Okay. "]);
    }

    #[test]
    fn audio_of_dropped_speech_is_not_played() {
        let call = Harness::new(&CallOptions::default());
        call.shared.io.playback.lock().unwrap().queue.reserve_exact(64);
        call.shared.say("One. ");
        let (_, generation) = next_text(&call.shared, 1000).unwrap();
        let mut held = vec![1; 10];
        assert!(call.shared.play(generation, &mut held, 100, false));
        assert_eq!(held.len(), 10, "held back until the prebuffer is full");
        call.shared.flush(false);
        assert!(!call.shared.play(generation, &mut held, 100, true));
        assert!(held.is_empty() && call.shared.io.playback.lock().unwrap().queue.is_empty());
    }

    #[test]
    fn waiting_sentences_are_sent_together_up_to_a_limit() {
        let call = Harness::new(&CallOptions::default());
        let long = format!("{}. ", "x".repeat(290));
        for sentence in ["One. ", "Two. ", &long, "Three. "] {
            call.shared.say(sentence);
        }
        assert_eq!(next_text(&call.shared, 1000).unwrap().0, "One. Two. ");
        assert_eq!(next_text(&call.shared, 1000).unwrap().0, format!("{long}Three. "));
    }

    #[test]
    fn noise_without_words_goes_back_to_listening() {
        let mut call = Harness::new(&CallOptions::default());
        call.utterance();
        call.transcribed("");
        assert_eq!(
            call.events(),
            [state(CallState::Hearing), state(CallState::Transcribing), state(CallState::Listening)]
        );
    }

    #[test]
    fn a_failed_transcription_is_a_problem_and_the_call_goes_on() {
        let mut call = Harness::new(&CallOptions::default());
        call.utterance();
        let message = call.receiver.recv().unwrap();
        assert!(call.engine.message(message));
        call.engine.refresh();
        let events = call.events();
        assert!(matches!(&events[2], CallEvent::Problem(problem) if problem.contains("API key")));
        assert_eq!(events[3], state(CallState::Listening));
    }

    #[test]
    fn the_early_transcription_is_reused_when_nothing_was_said_after_the_pause() {
        let mut call = Harness::new(&CallOptions::default());
        call.utterance();
        assert_eq!(call.engine.next_job, 1, "one transcription for the pause and the end");
        call.transcribed("Hello.");
        assert!(call.events().contains(&CallEvent::Utterance("Hello.".into())));
    }

    #[test]
    fn a_stale_early_transcription_is_ignored() {
        let mut call = Harness::new(&CallOptions::default());
        call.frames(&LOUD, 20);
        call.frames(&SILENCE, 15); // a pause: transcription 0 starts
        call.frames(&LOUD, 10); // the user talks on
        call.frames(&SILENCE, 14); // another pause: transcription 1 starts
        call.frames(&SILENCE, 10);
        let mut jobs = [call.receiver.recv().unwrap(), call.receiver.recv().unwrap()].map(|message| {
            let Message::Transcript(job, _) = message else { panic!("expected a transcript") };
            job
        });
        jobs.sort_unstable();
        assert_eq!(jobs, [0, 1]);
        assert!(call.engine.message(Message::Transcript(0, Ok("Hello".into()))));
        assert!(call.engine.message(Message::Transcript(1, Ok("Hello, are you there?".into()))));
        let utterances: Vec<CallEvent> =
            call.events().into_iter().filter(|event| matches!(event, CallEvent::Utterance(_))).collect();
        assert_eq!(utterances, [CallEvent::Utterance("Hello, are you there?".into())]);
    }

    #[test]
    fn a_muted_microphone_hears_nothing() {
        let mut call = Harness::new(&CallOptions::default());
        call.shared.muted.store(true, Ordering::Relaxed);
        call.frames(&LOUD, 40);
        assert!(call.events().is_empty());
        call.shared.muted.store(false, Ordering::Relaxed);
        call.frames(&LOUD, 7);
        assert_eq!(call.events(), [state(CallState::Hearing)]);
    }

    #[test]
    fn a_lost_device_ends_the_call() {
        let mut call = Harness::new(&CallOptions::default());
        assert!(!call.engine.message(Message::DeviceLost("unplugged".into())));
        assert_eq!(call.events(), [CallEvent::Ended(Some("unplugged".into()))]);
        assert!(call.shared.stopped());
        assert!(next_text(&call.shared, 1000).is_none());
        call.shared.say("Too late. ");
        assert!(call.pending().is_empty());
    }

    #[test]
    fn a_long_paragraph_is_spoken_in_bounded_requests() {
        let call = Harness::new(&CallOptions::default());
        call.shared.say(&format!(
            "{}
",
            "This is one sentence of a long paragraph. ".repeat(30)
        ));
        let mut requests = 0;
        while !call.pending().is_empty() {
            let (text, _) = next_text(&call.shared, 1000).unwrap();
            assert!(text.len() <= REQUEST_BYTES && text.ends_with("paragraph. "), "{text:?}");
            requests += 1;
        }
        assert_eq!(requests, 5);
    }

    #[test]
    fn a_call_can_be_moved_and_shared_between_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Call>();
    }

    #[test]
    fn meter_stays_in_range() {
        assert_eq!(meter(0.0), 0.0);
        assert!(meter(300.0) > 0.1 && meter(300.0) < meter(3000.0));
        assert_eq!(meter(40_000.0), 1.0);
    }

    /// Opens the real default devices; no request is made because nothing counts as speech.
    #[test]
    #[ignore = "needs a microphone and speakers"]
    fn live_devices_open_play_and_close() {
        assert!(audio::input_devices().iter().any(|device| device.default));
        assert!(audio::output_devices().iter().any(|device| device.default));
        let config = VoiceConfig { api_key: "not-used".into(), ..VoiceConfig::default() };
        // By name, which also covers looking a device up; the defaults are opened the same way.
        let default_name =
            |devices: Vec<audio::Device>| devices.into_iter().find(|device| device.default).unwrap().name;
        let options = CallOptions {
            input_device: Some(default_name(audio::input_devices())),
            output_device: Some(default_name(audio::output_devices())),
            speech_rms: f32::MAX,
            barge_in: false,
            ..CallOptions::default()
        };
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let call = Call::start(config, options, move |event| sink.lock().unwrap().push(event)).unwrap();

        // Half a second of silence goes through the speaker queue, so the output callback runs.
        let generation = call.shared.io.playback.lock().unwrap().generation;
        assert!(call.shared.play(generation, &mut vec![0; 24_000], 0, true));
        thread::sleep(Duration::from_millis(1500));
        assert!(call.shared.io.playback.lock().unwrap().queue.is_empty(), "the speakers did not take the audio");

        let started = Instant::now();
        call.end();
        call.end();
        assert!(started.elapsed() < Duration::from_secs(2), "ending the call hung");
        let events = events.lock().unwrap();
        assert_eq!(events.first(), Some(&state(CallState::Listening)));
        // The queued audio counted as speaking, then the call went back to listening.
        assert!(events.contains(&state(CallState::Speaking)));
        assert_eq!(
            events.iter().rfind(|event| matches!(event, CallEvent::State(_))),
            Some(&state(CallState::Listening))
        );
    }

    #[test]
    fn starting_without_a_key_fails_before_any_device_is_opened() {
        let result = Call::start(VoiceConfig::default(), CallOptions::default(), |_| {});
        assert!(matches!(result, Err(Error::MissingKey(_))));
    }
}

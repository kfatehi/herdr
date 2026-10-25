//! Push-to-talk voice input. Hold a key, speak, release; the words appear in the focused pane.
//!
//! This lives in the local client because it is the only process that has both halves: the
//! microphone of the machine you are sitting at, and the Windows console key events that report
//! key *release*. Transcript edits become ordinary typed input, so they reach the focused pane over
//! the same path as keystrokes, including panes on a remote machine.
//!
//! The client does not talk to a recogniser. It streams 16 kHz mono PCM to a claude-code-voice
//! daemon, which holds the speech-to-text credentials and answers with NDJSON partials:
//!
//!   POST /stt?rate=16000   chunked s16le mono in, `{"partial"}` / `{"final"}` / `{"error"}` out

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::{ClientInputEvent, ClientKeyCode, ClientKeyKind, ClientKeySource};

const TARGET_RATE: u32 = 16_000;
const DEFAULT_URL: &str = "http://127.0.0.1:8765";
/// Capture continues this long after the key comes up, so the word still being spoken as the key
/// travels is not cut off.
const GRACE: Duration = Duration::from_millis(400);
/// A key held by a cat is not a phrase.
const MAX_PHRASE: Duration = Duration::from_secs(120);
/// Newest-last level history drawn as the meter: 1.2 seconds at one sample per 50ms.
const METER_CELLS: usize = 24;
const METER_SAMPLE: Duration = Duration::from_millis(50);

/// Which key `voice.key` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TalkKey {
    /// Space is a character, so a press is withheld until it is known to be a hold.
    Space,
    /// A key a pane rarely needs, swallowed outright while voice input is available.
    Key(ClientKeyCode),
}

/// Read `voice.key`. Unknown names disable voice input rather than bind something surprising.
pub(super) fn parse_talk_key(name: &str) -> Option<TalkKey> {
    let name = name.trim().to_ascii_lowercase();
    match name.as_str() {
        "space" => Some(TalkKey::Space),
        "insert" => Some(TalkKey::Key(ClientKeyCode::Insert)),
        _ => name
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| (1..=12).contains(n))
            .map(|n| TalkKey::Key(ClientKeyCode::F(n))),
    }
}

/// Where the daemon is when `voice.url` does not say.
fn resolve_url(configured: &str) -> String {
    if !configured.trim().is_empty() {
        return configured.trim().to_owned();
    }
    std::env::var("CCV_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_owned())
}

/// What to do about a space bar event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpaceAction {
    /// Undecided, or a repeat to swallow.
    Wait,
    /// It was a tap: type the space now.
    TypeSpace,
    /// It was a hold: open the microphone. The space is never sent.
    Listen,
    /// The hold is over: close the microphone.
    Stop,
}

/// Space as the talk key: telling a tap from a hold.
///
/// Another key arriving or the key coming up settles a press as a tap; the threshold passing or the
/// key auto-repeating settles it as a hold. Because a Windows console reports release, the space a
/// hold would otherwise leave behind is never sent, so nothing has to be rubbed out afterwards.
struct SpaceTalk {
    hold: Duration,
    pending: Option<Instant>,
    listening: bool,
}

impl SpaceTalk {
    fn new(hold: Duration) -> Self {
        Self {
            hold,
            pending: None,
            listening: false,
        }
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn is_listening(&self) -> bool {
        self.listening
    }

    fn press(&mut self, now: Instant) -> SpaceAction {
        if self.listening {
            SpaceAction::Wait
        } else if self.pending.take().is_some() {
            // A second press with the first undecided can only be auto-repeat: a deliberate double
            // space releases in between.
            self.listening = true;
            SpaceAction::Listen
        } else {
            self.pending = Some(now);
            SpaceAction::Wait
        }
    }

    fn release(&mut self) -> SpaceAction {
        if self.listening {
            self.listening = false;
            return SpaceAction::Stop;
        }
        if self.pending.take().is_some() {
            return SpaceAction::TypeSpace;
        }
        SpaceAction::Wait
    }

    /// You do not type the next character of a word while still holding the space before it.
    fn other_key(&mut self) -> SpaceAction {
        if self.pending.take().is_some() {
            SpaceAction::TypeSpace
        } else {
            SpaceAction::Wait
        }
    }

    /// `down` is the physical key: a release while another window has focus produces no event.
    fn tick(&mut self, now: Instant, down: bool) -> SpaceAction {
        if let Some(pressed_at) = self.pending {
            if !down {
                self.pending = None;
                return SpaceAction::TypeSpace;
            }
            if now.duration_since(pressed_at) >= self.hold {
                self.pending = None;
                self.listening = true;
                return SpaceAction::Listen;
            }
        }
        if self.listening && !down {
            self.listening = false;
            return SpaceAction::Stop;
        }
        SpaceAction::Wait
    }

    /// The hold could not be served (no daemon, no microphone), so the space is owed back.
    fn refund(&mut self) -> SpaceAction {
        self.listening = false;
        self.pending = None;
        SpaceAction::TypeSpace
    }

    fn cancel(&mut self) {
        self.pending = None;
        self.listening = false;
    }
}

/// An edit to the pane's input line. Words appear while you speak, so the recogniser changing its
/// mind has to be expressible as rubbing out and retyping the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Edit {
    /// Characters, not bytes: a backspace erases one character.
    Rub(usize),
    Type(String),
}

/// What we have typed into the pane, and the edits still to apply.
#[derive(Default)]
struct Ledger {
    inserted: String,
    queue: Vec<Edit>,
}

impl Ledger {
    /// Bring the pane from what we typed to what the recogniser now believes. Only the tail after
    /// the common prefix moves.
    fn revise(&mut self, next: &str) {
        let keep = self
            .inserted
            .chars()
            .zip(next.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let rub = self.inserted.chars().count() - keep;
        if rub > 0 {
            self.queue.push(Edit::Rub(rub));
        }
        let tail: String = next.chars().skip(keep).collect();
        if !tail.is_empty() {
            self.queue.push(Edit::Type(tail));
        }
        self.inserted = next.to_owned();
    }

    /// Take back everything we typed, for an abandoned phrase.
    fn withdraw(&mut self) {
        let n = self.inserted.chars().count();
        if n > 0 {
            self.queue.push(Edit::Rub(n));
        }
        self.inserted.clear();
    }
}

/// One phrase's audio source and transcript. A trait so the controller is testable without a
/// microphone or a daemon.
pub(super) trait Recorder {
    fn start(&mut self);
    fn stop(&mut self);
    fn cancel(&mut self);
    fn is_listening(&self) -> bool;
    fn is_settled(&self) -> bool;
    fn take_edits(&mut self) -> Vec<Edit>;
    /// Loudness of the audio arriving now, 0.0 (silence) to 1.0 (clipping).
    fn level(&self) -> f32;
    /// The phrase failed before typing anything: daemon down, no microphone.
    fn failed_with_nothing_typed(&self) -> bool;
}

/// Streams the microphone to the voice daemon.
struct DaemonRecorder {
    url: String,
    token: Option<String>,
    holding: Arc<AtomicBool>,
    ledger: Arc<Mutex<Ledger>>,
    settled: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    level: Arc<AtomicU32>,
}

impl DaemonRecorder {
    fn new(url: String) -> Self {
        Self {
            url,
            // Read from the environment rather than config, so the shared secret is never written
            // to a file the config tooling prints back.
            token: std::env::var("CCV_TOKEN").ok().filter(|t| !t.is_empty()),
            holding: Arc::new(AtomicBool::new(false)),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            settled: Arc::new(AtomicBool::new(true)),
            error: Arc::new(Mutex::new(None)),
            level: Arc::new(AtomicU32::new(0)),
        }
    }
}

impl Recorder for DaemonRecorder {
    fn start(&mut self) {
        if self.is_listening() {
            return;
        }
        self.holding.store(true, Ordering::SeqCst);
        // A new phrase starts from an empty ledger: what the last one typed is the user's now.
        if let Ok(mut ledger) = self.ledger.lock() {
            *ledger = Ledger::default();
        }
        if let Ok(mut error) = self.error.lock() {
            *error = None;
        }
        self.settled.store(false, Ordering::SeqCst);
        self.level.store(0, Ordering::Relaxed);

        let phrase = Phrase {
            url: self.url.clone(),
            token: self.token.clone(),
            holding: Arc::clone(&self.holding),
            ledger: Arc::clone(&self.ledger),
            settled: Arc::clone(&self.settled),
            error: Arc::clone(&self.error),
            level: Arc::clone(&self.level),
        };
        // A recorded 16 kHz s16le phrase can stand in for the microphone, to exercise the daemon
        // wire without a person holding a key.
        let recorded = std::env::var_os("HERDR_VOICE_FILE").and_then(|p| std::fs::read(p).ok());
        let spawned = std::thread::Builder::new()
            .name("voice-phrase".into())
            .spawn(move || {
                if let Err(err) = phrase.run(recorded) {
                    tracing::warn!(error = %err, "voice phrase failed");
                    if let Ok(mut slot) = phrase.error.lock() {
                        *slot = Some(err);
                    }
                    phrase.holding.store(false, Ordering::SeqCst);
                    phrase.settled.store(true, Ordering::SeqCst);
                }
                phrase.level.store(0, Ordering::Relaxed);
            });
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "voice phrase thread could not start");
            self.holding.store(false, Ordering::SeqCst);
            self.settled.store(true, Ordering::SeqCst);
        }
    }

    fn stop(&mut self) {
        self.holding.store(false, Ordering::SeqCst);
        self.level.store(0, Ordering::Relaxed);
    }

    fn cancel(&mut self) {
        self.stop();
        if let Ok(mut ledger) = self.ledger.lock() {
            ledger.withdraw();
        }
    }

    fn is_listening(&self) -> bool {
        self.holding.load(Ordering::SeqCst)
    }

    /// Settled once the transcript has ended *and* its edits were taken. The client loop stops
    /// ticking voice when the phrase settles, so the final line, which lands just before the reader
    /// marks the phrase settled, would otherwise wait in the queue until the next phrase discards it.
    fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst)
            && self.ledger.lock().map(|l| l.queue.is_empty()).unwrap_or(true)
    }

    fn take_edits(&mut self) -> Vec<Edit> {
        self.ledger
            .lock()
            .map(|mut ledger| std::mem::take(&mut ledger.queue))
            .unwrap_or_default()
    }

    fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    fn failed_with_nothing_typed(&self) -> bool {
        let errored = self.error.lock().map(|e| e.is_some()).unwrap_or(false);
        errored
            && self
                .ledger
                .lock()
                .map(|l| l.inserted.is_empty())
                .unwrap_or(false)
    }
}

/// Everything one phrase's threads share.
struct Phrase {
    url: String,
    token: Option<String>,
    holding: Arc<AtomicBool>,
    ledger: Arc<Mutex<Ledger>>,
    settled: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    level: Arc<AtomicU32>,
}

impl Phrase {
    /// Microphone open, audio out, text back.
    fn run(&self, recorded: Option<Vec<u8>>) -> Result<(), String> {
        let (host, port, path) = split_url(&self.url)?;
        let socket = TcpStream::connect((host.as_str(), port))
            .map_err(|err| format!("voice daemon unreachable at {}: {err}", self.url))?;
        socket.set_nodelay(true).ok();
        let mut writer = socket.try_clone().map_err(|err| err.to_string())?;

        // Chunked, because the length is not known until the key comes up.
        let mut head = format!(
            "POST {path}/stt?rate={TARGET_RATE} HTTP/1.1\r\n\
             Host: {host}:{port}\r\n\
             Content-Type: audio/l16\r\n\
             Transfer-Encoding: chunked\r\n\
             Connection: close\r\n"
        );
        if let Some(token) = &self.token {
            head.push_str(&format!("x-ccv-token: {token}\r\n"));
        }
        head.push_str("\r\n");
        writer
            .write_all(head.as_bytes())
            .map_err(|err| err.to_string())?;

        let ledger = Arc::clone(&self.ledger);
        let settled = Arc::clone(&self.settled);
        let error = Arc::clone(&self.error);
        std::thread::Builder::new()
            .name("voice-transcript".into())
            .spawn(move || {
                read_transcript(socket, &ledger, &error);
                settled.store(true, Ordering::SeqCst);
            })
            .map(|_| ())
            .map_err(|err| err.to_string())?;

        match recorded {
            // Paced at roughly real time: endpoint detection listens for pauses.
            Some(pcm) => {
                for frame in pcm.chunks(3200) {
                    let samples: Vec<f32> = frame
                        .chunks_exact(2)
                        .map(|b| f32::from(i16::from_le_bytes([b[0], b[1]])) / f32::from(i16::MAX))
                        .collect();
                    publish_level(&self.level, &samples);
                    write_chunk(&mut writer, frame)?;
                    std::thread::sleep(Duration::from_millis(100));
                }
                self.holding.store(false, Ordering::SeqCst);
            }
            None => self.stream_microphone(&mut writer)?,
        }

        // Zero-length chunk: that is all the audio. The daemon answers once the recogniser's tail
        // arrives.
        writer
            .write_all(b"0\r\n\r\n")
            .map_err(|err| err.to_string())?;
        writer.flush().ok();
        Ok(())
    }

    fn stream_microphone(&self, writer: &mut TcpStream) -> Result<(), String> {
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let level = Arc::clone(&self.level);
        let rate = Arc::new(AtomicU32::new(TARGET_RATE));
        let callback_rate = Arc::clone(&rate);
        // The callback runs on the audio thread: no blocking, no I/O. The meter is fed here so it
        // moves with the microphone even if the upload stalls.
        let (capture, device_rate) = crate::platform::VoiceCapture::open(move |mono| {
            publish_level(&level, mono);
            let _ = tx.send(resample(mono, callback_rate.load(Ordering::Relaxed)));
        })?;
        rate.store(device_rate, Ordering::Relaxed);

        let deadline = Instant::now() + MAX_PHRASE;
        let mut grace_until = None;
        while Instant::now() < deadline {
            if !self.holding.load(Ordering::SeqCst) {
                match grace_until {
                    None => grace_until = Some(Instant::now() + GRACE),
                    Some(until) if Instant::now() >= until => break,
                    Some(_) => {}
                }
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(samples) => write_samples(writer, &samples)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.holding.store(false, Ordering::SeqCst);
        // Close the microphone first, then drain what it already handed over: the last ~100ms of
        // speech is still queued. Dropping the stream drops the sender, so the drain terminates.
        drop(capture);
        while let Ok(samples) = rx.recv() {
            write_samples(writer, &samples)?;
        }
        self.level.store(0, Ordering::Relaxed);
        Ok(())
    }
}

/// Every NDJSON line the daemon sends becomes an edit; partials included, which is what makes the
/// words appear as they are spoken.
fn read_transcript(socket: TcpStream, ledger: &Mutex<Ledger>, error: &Mutex<Option<String>>) {
    let mut lines = BufReader::new(socket);
    let mut line = String::new();
    let mut status_checked = false;
    let mut header_done = false;
    let fail = |message: String| {
        if let Ok(mut slot) = error.lock() {
            *slot = Some(message);
        }
    };
    loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !status_checked {
            status_checked = true;
            let status = line.split_whitespace().nth(1).unwrap_or_default();
            if !status.starts_with('2') {
                fail(format!("voice daemon answered {}", line.trim()));
                return;
            }
            continue;
        }
        if !header_done {
            header_done = line.trim().is_empty();
            continue;
        }
        // Chunk sizes are interleaved with the payload; the NDJSON lines start with a brace.
        let body = line.trim();
        if !body.starts_with('{') {
            continue;
        }
        match parse_daemon_line(body) {
            Some(DaemonLine::Partial(text)) => {
                if let Ok(mut l) = ledger.lock() {
                    l.revise(&text);
                }
            }
            Some(DaemonLine::Final(text)) => {
                if let Ok(mut l) = ledger.lock() {
                    l.revise(&text);
                }
                return;
            }
            Some(DaemonLine::Error(message)) => {
                // Leave what was typed: the user watched it appear and can edit it.
                fail(message);
                return;
            }
            None => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DaemonLine {
    Partial(String),
    Final(String),
    Error(String),
}

fn parse_daemon_line(line: &str) -> Option<DaemonLine> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_owned);
    field("partial")
        .map(DaemonLine::Partial)
        .or_else(|| field("final").map(DaemonLine::Final))
        .or_else(|| field("error").map(DaemonLine::Error))
}

fn write_samples(writer: &mut TcpStream, samples: &[i16]) -> Result<(), String> {
    if samples.is_empty() {
        return Ok(());
    }
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    write_chunk(writer, &bytes)
}

fn write_chunk(writer: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    writer
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .and_then(|_| writer.write_all(bytes))
        .and_then(|_| writer.write_all(b"\r\n"))
        .map_err(|err| format!("voice upload failed: {err}"))
}

/// Loudness on a scale the eye can read. Mapped in dB across -55..-8, where speech sits; the rise
/// is immediate and the fall smoothed so gaps between words do not strobe the meter. Called from the
/// audio callback, so it only touches an atomic.
fn publish_level(level: &AtomicU32, mono: &[f32]) {
    if mono.is_empty() {
        return;
    }
    let rms = (mono.iter().map(|s| s * s).sum::<f32>() / mono.len() as f32).sqrt();
    let db = 20.0 * rms.max(1e-6).log10();
    let scaled = ((db + 55.0) / 47.0).clamp(0.0, 1.0);
    let prev = f32::from_bits(level.load(Ordering::Relaxed));
    let next = if scaled > prev {
        scaled
    } else {
        prev * 0.7 + scaled * 0.3
    };
    level.store(next.to_bits(), Ordering::Relaxed);
}

/// Linear resample to 16 kHz, the rate the recogniser wants.
fn resample(mono: &[f32], from_rate: u32) -> Vec<i16> {
    let to_i16 = |v: f32| (v.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
    if from_rate == TARGET_RATE || from_rate == 0 || mono.is_empty() {
        return mono.iter().copied().map(to_i16).collect();
    }
    let ratio = from_rate as f32 / TARGET_RATE as f32;
    let out_len = (mono.len() as f32 / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f32 * ratio;
            let idx = pos as usize;
            let frac = pos - idx as f32;
            let a = mono[idx.min(mono.len() - 1)];
            let b = mono[(idx + 1).min(mono.len() - 1)];
            to_i16(a + (b - a) * frac)
        })
        .collect()
}

fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_owned()),
        None => (rest, String::new()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_owned(),
            p.parse()
                .map_err(|_| format!("bad port in voice url {url}"))?,
        ),
        None => (authority.to_owned(), 80),
    };
    Ok((host, port, path))
}

/// One column of the meter: which glyph, and how hot it is (0 quiet .. 3 peaking). The lowest
/// glyph is a baseline rather than a blank, so "listening, hearing silence" stays visible.
pub(super) fn meter_cell(level: f32) -> (char, u8) {
    const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let idx = ((level.clamp(0.0, 1.0) * 8.0) as usize).min(7);
    let heat = match idx {
        0..=1 => 0,
        2..=4 => 1,
        5..=6 => 2,
        _ => 3,
    };
    (GLYPHS[idx], heat)
}

/// Input to forward and whether the meter changed, from one timer tick.
#[derive(Default)]
pub(super) struct VoiceTick {
    pub(super) events: Vec<ClientInputEvent>,
    pub(super) repaint: bool,
}

/// The client loop's view of voice input: filters the talk key out of the input stream, and turns
/// transcript edits back into input for the pane the phrase started in.
pub(super) struct VoiceInput {
    talk_key: Option<TalkKey>,
    url: String,
    recorder: Option<Box<dyn Recorder>>,
    space: SpaceTalk,
    /// The withheld space press, replayed if it turns out to be a tap or the hold fails.
    held_space: Option<ClientInputEvent>,
    /// A space press reached the pane, so its release must too.
    space_press_forwarded: bool,
    /// A non-space talk key is down.
    key_held: bool,
    /// Identity of the pane the phrase is typing into. Edits never land anywhere else.
    phrase_target: Option<String>,
    levels: VecDeque<f32>,
    last_sample: Option<Instant>,
    /// Physical key state, `None` when the platform cannot tell.
    key_down: fn(&ClientKeyCode) -> Option<bool>,
}

impl VoiceInput {
    pub(super) fn new(config: &crate::config::VoiceConfig) -> Self {
        let mut voice = Self {
            talk_key: None,
            url: String::new(),
            recorder: None,
            space: SpaceTalk::new(Duration::from_millis(config.hold_time_ms)),
            held_space: None,
            space_press_forwarded: false,
            key_held: false,
            phrase_target: None,
            levels: VecDeque::new(),
            last_sample: None,
            key_down: crate::platform::voice_key_physically_down,
        };
        if std::env::var_os("HERDR_VOICE_FILE").is_some() {
            // A simulated phrase is driven by injected key records, which the physical keyboard
            // state never reflects.
            voice.key_down = |_| None;
        }
        voice.reload(config);
        voice
    }

    #[cfg(test)]
    fn with_recorder(key: TalkKey, recorder: Box<dyn Recorder>) -> Self {
        let mut voice = Self::new(&crate::config::VoiceConfig {
            enabled: false,
            ..Default::default()
        });
        voice.talk_key = Some(key);
        voice.recorder = Some(recorder);
        voice.key_down = |_| None;
        voice
    }

    /// Apply changed config. A phrase in flight keeps its recorder until it ends.
    pub(super) fn reload(&mut self, config: &crate::config::VoiceConfig) {
        self.space.hold = Duration::from_millis(config.hold_time_ms);
        if self.is_busy() {
            return;
        }
        let key = parse_talk_key(&config.key);
        if config.enabled && key.is_none() {
            tracing::warn!(key = %config.key, "voice.key is not a key voice input can use; voice input is off");
        }
        let url = resolve_url(&config.url);
        if !config.enabled || key.is_none() {
            self.recorder = None;
        } else if self.recorder.is_none() || url != self.url {
            self.recorder = Some(Box::new(DaemonRecorder::new(url.clone())));
        }
        self.talk_key = key;
        self.url = url;
    }

    fn listening(&self) -> bool {
        self.recorder.as_ref().is_some_and(|r| r.is_listening())
    }

    fn is_busy(&self) -> bool {
        self.space.is_pending()
            || self.space.is_listening()
            || self.key_held
            || self
                .recorder
                .as_ref()
                .is_some_and(|r| r.is_listening() || !r.is_settled())
    }

    /// Whether `tick` has anything to do: a key or phrase in flight, or a meter to clear.
    pub(super) fn needs_tick(&self) -> bool {
        self.is_busy() || !self.levels.is_empty()
    }

    /// How soon the loop must tick again, while there is anything to decide, drain or draw.
    pub(super) fn timer_delay(&self) -> Option<Duration> {
        self.is_busy().then_some(METER_SAMPLE)
    }

    /// The level history to draw, while the microphone is open.
    pub(super) fn meter(&self) -> Option<Vec<f32>> {
        (self.listening() && !self.levels.is_empty()).then(|| self.levels.iter().copied().collect())
    }

    /// Remove talk-key events from host input and return what should still reach the shell.
    ///
    /// `target` identifies where typed text would go right now, or is `None` when input is bound
    /// for Herdr itself (prefix, overlays, copy mode); the talk key is only intercepted when there
    /// is a pane to type into.
    // Only the Windows console reader produces key events with release; elsewhere this is test-only.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(super) fn filter_events(
        &mut self,
        events: Vec<ClientInputEvent>,
        target: Option<&str>,
        now: Instant,
    ) -> Vec<ClientInputEvent> {
        if self.recorder.is_none() || self.talk_key.is_none() {
            return events;
        }
        let mut out = Vec::with_capacity(events.len());
        for event in events {
            self.filter_event(event, target, now, &mut out);
        }
        out
    }

    // Only the Windows console reader produces key events with release; elsewhere this is test-only.
    #[cfg_attr(not(windows), allow(dead_code))]
    fn filter_event(
        &mut self,
        event: ClientInputEvent,
        target: Option<&str>,
        now: Instant,
        out: &mut Vec<ClientInputEvent>,
    ) {
        let (code, modifiers, kind) = match &event {
            ClientInputEvent::Key {
                code,
                modifiers,
                kind,
                ..
            } => (Some(code.clone()), *modifiers, Some(*kind)),
            _ => (None, 0, None),
        };
        let is_space = code == Some(ClientKeyCode::Char(' ')) && modifiers == 0;
        let is_press = matches!(kind, Some(ClientKeyKind::Press | ClientKeyKind::Repeat));

        if self.talk_key == Some(TalkKey::Space) && is_space {
            match kind {
                Some(ClientKeyKind::Release) => {
                    if self.space_press_forwarded {
                        self.space_press_forwarded = false;
                        out.push(event);
                        return;
                    }
                    match self.space.release() {
                        SpaceAction::TypeSpace => {
                            self.type_held_space(out);
                            self.space_press_forwarded = false;
                            out.push(event);
                        }
                        SpaceAction::Stop => self.apply(SpaceAction::Stop, target, out),
                        _ => out.push(event),
                    }
                }
                _ => {
                    let fresh = !self.space.is_pending() && !self.space.is_listening();
                    if fresh && (self.space_press_forwarded || target.is_none()) {
                        // Auto-repeat of a space already typed, or input bound for Herdr.
                        out.push(event);
                        return;
                    }
                    if fresh {
                        self.held_space = Some(event);
                    }
                    let action = self.space.press(now);
                    self.apply(action, target, out);
                }
            }
            return;
        }

        if let (Some(TalkKey::Key(talk)), Some(code)) = (&self.talk_key, &code) {
            if talk == code {
                match kind {
                    Some(ClientKeyKind::Release) if self.key_held => {
                        self.key_held = false;
                        self.stop_phrase();
                    }
                    Some(ClientKeyKind::Release) => out.push(event),
                    _ if self.key_held => {}
                    _ if target.is_some() => {
                        self.key_held = true;
                        self.start_phrase(target);
                    }
                    _ => out.push(event),
                }
                return;
            }
        }

        // Escape while dictating takes the sentence back rather than leaving half of it.
        if code == Some(ClientKeyCode::Esc)
            && kind == Some(ClientKeyKind::Press)
            && self.listening()
        {
            self.space.cancel();
            self.held_space = None;
            self.key_held = false;
            if let Some(recorder) = self.recorder.as_mut() {
                recorder.cancel();
            }
            return;
        }

        // Any other input settles a withheld space as a tap, typed first so order is preserved.
        let settles = is_press
            || matches!(
                event,
                ClientInputEvent::TextCommit(_) | ClientInputEvent::Paste { .. }
            );
        if settles && self.space.is_pending() {
            let action = self.space.other_key();
            self.apply(action, target, out);
        }
        out.push(event);
    }

    fn apply(
        &mut self,
        action: SpaceAction,
        target: Option<&str>,
        out: &mut Vec<ClientInputEvent>,
    ) {
        match action {
            SpaceAction::Wait => {}
            SpaceAction::TypeSpace => self.type_held_space(out),
            SpaceAction::Listen => self.start_phrase(target),
            SpaceAction::Stop => {
                // The hold was served; the withheld space is gone for good.
                self.held_space = None;
                self.stop_phrase();
            }
        }
    }

    fn type_held_space(&mut self, out: &mut Vec<ClientInputEvent>) {
        if let Some(press) = self.held_space.take() {
            out.push(press);
            self.space_press_forwarded = true;
        }
    }

    fn start_phrase(&mut self, target: Option<&str>) {
        self.phrase_target = target.map(str::to_owned);
        self.levels.clear();
        self.last_sample = None;
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.start();
        }
    }

    fn stop_phrase(&mut self) {
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.stop();
        }
    }

    /// Advance timers: settle a withheld space, sample the meter, and turn transcript edits into
    /// input for the pane the phrase started in.
    pub(super) fn tick(&mut self, target: Option<&str>, now: Instant) -> VoiceTick {
        let mut tick = VoiceTick::default();
        if self.recorder.is_none() {
            return tick;
        }

        if self.space.is_pending() || self.space.is_listening() {
            let down = (self.key_down)(&ClientKeyCode::Char(' ')).unwrap_or(true);
            let action = self.space.tick(now, down);
            if action == SpaceAction::TypeSpace {
                // The release was missed while another window had focus; type the tap and close it.
                self.type_held_space(&mut tick.events);
                self.space_press_forwarded = false;
                tick.events.push(synthesized_key(
                    ClientKeyCode::Char(' '),
                    ClientKeyKind::Release,
                ));
            } else {
                self.apply(action, target, &mut tick.events);
            }
        }
        if self.key_held {
            if let Some(TalkKey::Key(code)) = &self.talk_key {
                if (self.key_down)(code) == Some(false) {
                    self.key_held = false;
                    self.stop_phrase();
                }
            }
        }

        // A hold nobody could serve gives the space back while the key is still down, so a missing
        // daemon never silently eats a character.
        if self.space.is_listening()
            && self
                .recorder
                .as_ref()
                .is_some_and(|r| r.failed_with_nothing_typed())
        {
            let action = self.space.refund();
            self.apply(action, target, &mut tick.events);
            self.stop_phrase();
        }

        if self.listening() {
            if self.levels.is_empty() {
                // Start at full width, at the baseline.
                self.levels.resize(METER_CELLS, 0.0);
                tick.repaint = true;
            }
            if self
                .last_sample
                .is_none_or(|last| now.duration_since(last) >= METER_SAMPLE)
            {
                self.last_sample = Some(now);
                let level = self.recorder.as_ref().map_or(0.0, |r| r.level());
                self.levels.push_back(level);
                while self.levels.len() > METER_CELLS {
                    self.levels.pop_front();
                }
                tick.repaint = true;
            }
        } else if !self.levels.is_empty() {
            self.levels.clear();
            tick.repaint = true;
        }

        let edits = self
            .recorder
            .as_mut()
            .map(|r| r.take_edits())
            .unwrap_or_default();
        if !edits.is_empty() {
            if target.is_some() && target == self.phrase_target.as_deref() {
                tick.events.extend(edits_to_events(edits));
            } else {
                // Focus moved: never type into a pane the phrase did not start in.
                tracing::info!("voice transcript dropped because the focused pane changed");
                self.stop_phrase();
            }
        }
        tick
    }
}

fn synthesized_key(code: ClientKeyCode, kind: ClientKeyKind) -> ClientInputEvent {
    ClientInputEvent::Key {
        code,
        modifiers: 0,
        kind,
        repeat_count: 1,
        generated_text: None,
        source: ClientKeySource::Synthesized,
    }
}

/// Edits as typed input. Line breaks become spaces: dictation never submits.
fn edits_to_events(edits: Vec<Edit>) -> Vec<ClientInputEvent> {
    let mut events = Vec::new();
    for edit in edits {
        match edit {
            Edit::Rub(n) => {
                for _ in 0..n {
                    events.push(synthesized_key(
                        ClientKeyCode::Backspace,
                        ClientKeyKind::Press,
                    ));
                    events.push(synthesized_key(
                        ClientKeyCode::Backspace,
                        ClientKeyKind::Release,
                    ));
                }
            }
            Edit::Type(text) => {
                let text: String = text
                    .chars()
                    .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
                    .collect();
                events.push(ClientInputEvent::TextCommit(text));
            }
        }
    }
    events
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;

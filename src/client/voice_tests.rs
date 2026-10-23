use super::*;

use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct FakeState {
    listening: bool,
    starts: usize,
    stops: usize,
    cancels: usize,
    failed: bool,
    edits: Vec<Edit>,
    level: f32,
}

struct FakeRecorder(Rc<RefCell<FakeState>>);

impl Recorder for FakeRecorder {
    fn start(&mut self) {
        let mut state = self.0.borrow_mut();
        state.listening = true;
        state.starts += 1;
    }
    fn stop(&mut self) {
        let mut state = self.0.borrow_mut();
        state.listening = false;
        state.stops += 1;
    }
    fn cancel(&mut self) {
        let mut state = self.0.borrow_mut();
        state.listening = false;
        state.cancels += 1;
    }
    fn is_listening(&self) -> bool {
        self.0.borrow().listening
    }
    fn is_settled(&self) -> bool {
        !self.0.borrow().listening
    }
    fn take_edits(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.0.borrow_mut().edits)
    }
    fn level(&self) -> f32 {
        self.0.borrow().level
    }
    fn failed_with_nothing_typed(&self) -> bool {
        self.0.borrow().failed
    }
}

fn voice(key: TalkKey) -> (VoiceInput, Rc<RefCell<FakeState>>) {
    let state = Rc::new(RefCell::new(FakeState::default()));
    let voice = VoiceInput::with_recorder(key, Box::new(FakeRecorder(Rc::clone(&state))));
    (voice, state)
}

fn key(code: ClientKeyCode, kind: ClientKeyKind) -> ClientInputEvent {
    ClientInputEvent::Key {
        code,
        modifiers: 0,
        kind,
        repeat_count: 1,
        generated_text: None,
        source: ClientKeySource::Synthesized,
    }
}

fn space(kind: ClientKeyKind) -> ClientInputEvent {
    key(ClientKeyCode::Char(' '), kind)
}

const PANE: Option<&str> = Some("local/p1");

#[test]
fn voice_key_names_are_read_case_insensitively() {
    assert_eq!(parse_talk_key("space"), Some(TalkKey::Space));
    assert_eq!(parse_talk_key("  SPACE "), Some(TalkKey::Space));
    assert_eq!(
        parse_talk_key("F9"),
        Some(TalkKey::Key(ClientKeyCode::F(9)))
    );
    assert_eq!(
        parse_talk_key("insert"),
        Some(TalkKey::Key(ClientKeyCode::Insert))
    );
    assert_eq!(parse_talk_key("f13"), None);
    assert_eq!(parse_talk_key("banana"), None);
    assert_eq!(parse_talk_key(""), None);
}

#[test]
fn the_configured_url_wins_over_the_environment() {
    assert_eq!(
        resolve_url("  http://box.local:9000 "),
        "http://box.local:9000"
    );
}

#[test]
fn splits_the_daemon_url() {
    assert_eq!(
        split_url("http://127.0.0.1:8765").unwrap(),
        ("127.0.0.1".into(), 8765, String::new())
    );
    assert_eq!(
        split_url("http://box.local:9000/voice/").unwrap(),
        ("box.local".into(), 9000, "/voice".into())
    );
    assert!(split_url("http://box:port").is_err());
}

#[test]
fn reads_the_lines_the_daemon_sends() {
    assert_eq!(
        parse_daemon_line(r#"{"partial":"hello there"}"#),
        Some(DaemonLine::Partial("hello there".into()))
    );
    assert_eq!(
        parse_daemon_line(r#"{"final":"a \"quoted\" word"}"#),
        Some(DaemonLine::Final("a \"quoted\" word".into()))
    );
    assert_eq!(
        parse_daemon_line(r#"{"error":"nope"}"#),
        Some(DaemonLine::Error("nope".into()))
    );
    assert_eq!(parse_daemon_line("{not json"), None);
}

#[test]
fn resampling_48k_to_16k_keeps_a_third_of_the_samples() {
    let input: Vec<f32> = (0..480).map(|i| (i as f32 / 480.0) - 0.5).collect();
    assert_eq!(resample(&input, 48_000).len(), 160);
    assert_eq!(resample(&[0.5; 160], 16_000).len(), 160);
}

#[test]
fn a_growing_phrase_only_types_the_new_words() {
    let mut ledger = Ledger::default();
    ledger.revise("the plus");
    ledger.revise("the plus clip");
    assert_eq!(
        ledger.queue,
        vec![Edit::Type("the plus".into()), Edit::Type(" clip".into())]
    );
}

#[test]
fn a_recogniser_changing_its_mind_rubs_out_only_the_tail() {
    let mut ledger = Ledger::default();
    ledger.revise("the plus clip");
    ledger.queue.clear();
    ledger.revise("the +CLIP");
    assert_eq!(ledger.queue, vec![Edit::Rub(9), Edit::Type("+CLIP".into())]);
}

#[test]
fn rubbing_counts_characters_rather_than_bytes() {
    let mut ledger = Ledger::default();
    ledger.revise("café — x");
    ledger.queue.clear();
    ledger.revise("café");
    assert_eq!(ledger.queue, vec![Edit::Rub(4)]);
}

#[test]
fn an_abandoned_phrase_takes_back_everything_it_typed() {
    let mut ledger = Ledger::default();
    ledger.revise("half a sentence");
    ledger.queue.clear();
    ledger.withdraw();
    assert_eq!(ledger.queue, vec![Edit::Rub(15)]);
    assert!(ledger.inserted.is_empty());
}

#[test]
fn an_unchanged_partial_types_nothing_at_all() {
    let mut ledger = Ledger::default();
    ledger.revise("steady");
    ledger.queue.clear();
    ledger.revise("steady");
    assert!(ledger.queue.is_empty());
}

#[test]
fn the_meter_never_empties_to_a_blank_cell() {
    assert_eq!(meter_cell(0.0), ('▁', 0));
    assert_eq!(meter_cell(1.0), ('█', 3));
    assert_eq!(meter_cell(2.0), ('█', 3));
    assert_eq!(meter_cell(0.5), ('▅', 1));
}

#[test]
fn silence_reads_at_the_bottom_of_the_meter_and_speech_does_not() {
    let quiet = AtomicU32::new(0);
    publish_level(&quiet, &[0.0; 512]);
    assert_eq!(f32::from_bits(quiet.load(Ordering::Relaxed)), 0.0);
    let speech = AtomicU32::new(0);
    publish_level(&speech, &[0.05; 512]);
    let v = f32::from_bits(speech.load(Ordering::Relaxed));
    assert!(v > 0.5 && v < 0.8, "got {v}");
}

#[test]
fn a_tapped_space_still_types_a_space_in_order() {
    let (mut voice, state) = voice(TalkKey::Space);
    let now = Instant::now();
    assert!(voice
        .filter_events(vec![space(ClientKeyKind::Press)], PANE, now)
        .is_empty());
    let out = voice.filter_events(
        vec![
            space(ClientKeyKind::Release),
            key(ClientKeyCode::Char('b'), ClientKeyKind::Press),
        ],
        PANE,
        now,
    );
    assert_eq!(
        out,
        vec![
            space(ClientKeyKind::Press),
            space(ClientKeyKind::Release),
            key(ClientKeyCode::Char('b'), ClientKeyKind::Press),
        ]
    );
    assert_eq!(state.borrow().starts, 0);
}

#[test]
fn typing_the_next_letter_settles_the_space_before_it() {
    let (mut voice, _) = voice(TalkKey::Space);
    let now = Instant::now();
    let out = voice.filter_events(
        vec![
            space(ClientKeyKind::Press),
            key(ClientKeyCode::Char('b'), ClientKeyKind::Press),
            space(ClientKeyKind::Release),
        ],
        PANE,
        now,
    );
    assert_eq!(
        out,
        vec![
            space(ClientKeyKind::Press),
            key(ClientKeyCode::Char('b'), ClientKeyKind::Press),
            space(ClientKeyKind::Release),
        ]
    );
}

#[test]
fn an_auto_repeat_opens_the_microphone_and_the_space_is_never_sent() {
    let (mut voice, state) = voice(TalkKey::Space);
    let now = Instant::now();
    let out = voice.filter_events(
        vec![
            space(ClientKeyKind::Press),
            space(ClientKeyKind::Press),
            space(ClientKeyKind::Press),
        ],
        PANE,
        now,
    );
    assert!(out.is_empty());
    assert!(state.borrow().listening);
    assert_eq!(state.borrow().starts, 1);
    let out = voice.filter_events(vec![space(ClientKeyKind::Release)], PANE, now);
    assert!(out.is_empty());
    assert!(!state.borrow().listening);
}

#[test]
fn space_bound_for_herdr_is_not_intercepted() {
    let (mut voice, _) = voice(TalkKey::Space);
    let out = voice.filter_events(
        vec![space(ClientKeyKind::Press), space(ClientKeyKind::Release)],
        None,
        Instant::now(),
    );
    assert_eq!(
        out,
        vec![space(ClientKeyKind::Press), space(ClientKeyKind::Release)]
    );
}

#[test]
fn a_hold_nobody_could_serve_hands_the_space_back() {
    let (mut voice, state) = voice(TalkKey::Space);
    let now = Instant::now();
    voice.filter_events(
        vec![space(ClientKeyKind::Press), space(ClientKeyKind::Press)],
        PANE,
        now,
    );
    state.borrow_mut().failed = true;
    let tick = voice.tick(PANE, now);
    assert_eq!(tick.events, vec![space(ClientKeyKind::Press)]);
    // And the real release that follows reaches the pane exactly once.
    let out = voice.filter_events(vec![space(ClientKeyKind::Release)], PANE, now);
    assert_eq!(out, vec![space(ClientKeyKind::Release)]);
}

#[test]
fn transcript_edits_become_typed_input_for_the_phrase_pane() {
    let (mut voice, state) = voice(TalkKey::Key(ClientKeyCode::F(9)));
    let now = Instant::now();
    assert!(voice
        .filter_events(
            vec![key(ClientKeyCode::F(9), ClientKeyKind::Press)],
            PANE,
            now
        )
        .is_empty());
    state.borrow_mut().edits = vec![
        Edit::Type("hello wrld".into()),
        Edit::Rub(4),
        Edit::Type("world\n".into()),
    ];
    let tick = voice.tick(PANE, now);
    let mut expected = vec![ClientInputEvent::TextCommit("hello wrld".into())];
    for _ in 0..4 {
        expected.push(key(ClientKeyCode::Backspace, ClientKeyKind::Press));
        expected.push(key(ClientKeyCode::Backspace, ClientKeyKind::Release));
    }
    expected.push(ClientInputEvent::TextCommit("world ".into()));
    assert_eq!(
        tick.events, expected,
        "and a line break is never sent as Enter"
    );
    assert!(voice
        .filter_events(
            vec![key(ClientKeyCode::F(9), ClientKeyKind::Release)],
            PANE,
            now
        )
        .is_empty());
    assert!(!state.borrow().listening);
}

#[test]
fn edits_never_land_in_a_pane_the_phrase_did_not_start_in() {
    let (mut voice, state) = voice(TalkKey::Key(ClientKeyCode::F(9)));
    let now = Instant::now();
    voice.filter_events(
        vec![key(ClientKeyCode::F(9), ClientKeyKind::Press)],
        PANE,
        now,
    );
    state.borrow_mut().edits = vec![Edit::Type("secret".into())];
    let tick = voice.tick(Some("local/p2"), now);
    assert!(tick.events.is_empty());
    assert!(!state.borrow().listening);
}

#[test]
fn escape_while_dictating_cancels_the_phrase_and_is_swallowed() {
    let (mut voice, state) = voice(TalkKey::Space);
    let now = Instant::now();
    voice.filter_events(
        vec![space(ClientKeyKind::Press), space(ClientKeyKind::Press)],
        PANE,
        now,
    );
    let out = voice.filter_events(
        vec![key(ClientKeyCode::Esc, ClientKeyKind::Press)],
        PANE,
        now,
    );
    assert!(out.is_empty());
    assert_eq!(state.borrow().cancels, 1);
    let out = voice.filter_events(vec![space(ClientKeyKind::Release)], PANE, now);
    assert_eq!(
        out,
        vec![space(ClientKeyKind::Release)],
        "a cancelled phrase leaves no space behind"
    );
}

#[test]
fn the_meter_fills_while_listening_and_clears_after() {
    let (mut voice, state) = voice(TalkKey::Key(ClientKeyCode::F(9)));
    let now = Instant::now();
    voice.filter_events(
        vec![key(ClientKeyCode::F(9), ClientKeyKind::Press)],
        PANE,
        now,
    );
    state.borrow_mut().level = 0.9;
    assert!(voice.tick(PANE, now).repaint);
    let meter = voice.meter().expect("meter while listening");
    assert_eq!(meter.len(), METER_CELLS);
    assert_eq!(meter.last().copied(), Some(0.9));
    assert_eq!(voice.timer_delay(), Some(METER_SAMPLE));
    voice.filter_events(
        vec![key(ClientKeyCode::F(9), ClientKeyKind::Release)],
        PANE,
        now,
    );
    assert!(voice.tick(PANE, now).repaint);
    assert!(voice.meter().is_none());
    assert_eq!(voice.timer_delay(), None);
}

#[test]
fn disabled_voice_passes_everything_through() {
    let mut voice = VoiceInput::new(&crate::config::VoiceConfig {
        enabled: false,
        ..Default::default()
    });
    let events = vec![space(ClientKeyKind::Press), space(ClientKeyKind::Press)];
    assert_eq!(
        voice.filter_events(events.clone(), PANE, Instant::now()),
        events
    );
}

/// Streams `HERDR_VOICE_FILE` (16 kHz mono s16le) through a running voice daemon and prints the
/// transcript. Run manually: `cargo test -- --ignored voice_daemon_roundtrip --nocapture`.
#[test]
#[ignore = "needs a running voice daemon and HERDR_VOICE_FILE"]
fn voice_daemon_roundtrip() {
    assert!(std::env::var_os("HERDR_VOICE_FILE").is_some());
    let mut recorder = DaemonRecorder::new(resolve_url(""));
    recorder.start();
    let mut typed = String::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        for edit in recorder.take_edits() {
            match edit {
                Edit::Rub(n) => (0..n).for_each(|_| {
                    typed.pop();
                }),
                Edit::Type(text) => typed.push_str(&text),
            }
            println!("  -> {typed}");
        }
        if recorder.is_settled() && !recorder.is_listening() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let error = recorder.error.lock().unwrap().clone();
    assert_eq!(error, None);
    println!("final: {typed}");
    assert!(!typed.trim().is_empty());
}

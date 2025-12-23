use lector::{app::{App, Clock}, screen_reader::ScreenReader, speech, views};
use std::{
    cell::Cell,
    cell::RefCell,
    rc::Rc,
};

#[derive(Default)]
struct RecorderState {
    speaks: Vec<(String, bool)>,
    stops: usize,
    rate: f32,
}

#[derive(Clone, Default)]
struct Recorder {
    inner: Rc<RefCell<RecorderState>>,
}

struct FakeDriver {
    recorder: Recorder,
}

impl speech::Driver for FakeDriver {
    fn speak(&mut self, text: &str, interrupt: bool) -> anyhow::Result<()> {
        self.recorder
            .inner
            .borrow_mut()
            .speaks
            .push((text.to_string(), interrupt));
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        self.recorder.inner.borrow_mut().stops += 1;
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        self.recorder.inner.borrow().rate
    }

    fn set_rate(&mut self, rate: f32) -> anyhow::Result<()> {
        self.recorder.inner.borrow_mut().rate = rate;
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeClock {
    now: Rc<Cell<u128>>,
}

impl FakeClock {
    fn advance_ms(&self, delta: u128) {
        self.now.set(self.now.get().saturating_add(delta));
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u128 {
        self.now.get()
    }
}

fn make_app() -> (App, ScreenReader, Recorder, FakeClock) {
    let recorder = Recorder::default();
    let driver = FakeDriver {
        recorder: recorder.clone(),
    };
    let speech = speech::Speech::new(Box::new(driver));
    let screen_reader = ScreenReader::new(speech);
    let view_stack = views::ViewStack::new(Box::new(views::PtyView::new(24, 80)));
    let clock = FakeClock::default();
    let app = App::new_with_clock(view_stack, Box::new(clock.clone())).expect("create app");
    (app, screen_reader, recorder, clock)
}

#[test]
fn stdin_unmapped_forwards_to_pty() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    app.handle_stdin(&mut sr, b"a", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"a");
    assert!(term_out.is_empty());
    assert_eq!(sr.last_key, b"a");
    assert_eq!(recorder.inner.borrow().stops, 1);
}

#[test]
fn paste_writes_to_pty_and_speaks() {
    let (mut app, mut sr, recorder, _clock) = make_app();
    let mut pty_out = Vec::new();
    let mut term_out = Vec::new();

    sr.clipboard.put("hello".to_string());
    app.handle_stdin(&mut sr, b"\x1B[18~", &mut pty_out, &mut term_out)
        .expect("handle stdin");

    assert_eq!(pty_out, b"hello");
    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text == "pasted"));
}

#[test]
fn pty_output_writes_terminal_and_autoreads() {
    let (mut app, mut sr, recorder, clock) = make_app();
    let mut term_out = Vec::new();

    app.handle_pty(&mut sr, b"hello\r\n", &mut term_out)
        .expect("handle pty");
    assert_eq!(term_out, b"hello\r\n");

    clock.advance_ms(2);
    let _ = app.maybe_finalize_changes(&mut sr).expect("finalize");

    let speaks = &recorder.inner.borrow().speaks;
    assert!(speaks.iter().any(|(text, _)| text.contains("hello")));
}

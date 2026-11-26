//! NVDA's v2 controller delivers markers only from a blocking SSML call.
//! A dedicated speech worker serializes those calls, while cancellation uses
//! the separate v1 RPC binding so it can stop a blocked call.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use windows::core::HSTRING;

use super::{Controller, audio::Audio};
use crate::{
    Callbacks, Error, UtteranceId,
    nvda_schedule::{Job, Schedule},
    nvda_ssml::{MarkProgress, make_ssml, parse_mark, spoken_text},
    protocol::{SoundCue, SoundCueAt},
};

pub(super) type MarkFn = unsafe extern "system" fn(*const u16) -> u32;
pub(super) type SetMarkFn = unsafe extern "system" fn(Option<MarkFn>) -> u32;
pub(super) type SpeakSsmlFn = unsafe extern "system" fn(*const u16, i32, i32, u8) -> u32;
pub(super) type ProcessIdFn = unsafe extern "system" fn(*mut u32) -> u32;

const SYMBOL_LEVEL_UNCHANGED: i32 = -1;
const PRIORITY_NORMAL: i32 = 0;
const SYNCHRONOUS: u8 = 0;
const ERROR_CANCELLED: u32 = 1223;
const RPC_S_UNKNOWN_IF: u32 = 1717;
const DRAIN_INTERVAL: Duration = Duration::from_millis(5);
const DRAIN_DEADLINE: Duration = Duration::from_secs(2);
const CANCEL_INTERVAL: Duration = Duration::from_millis(20);
const CANCEL_DEADLINE: Duration = Duration::from_secs(2);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CALL: Mutex<()> = Mutex::new(());
static SESSION: Mutex<Option<MarkerSession>> = Mutex::new(None);

#[derive(Debug)]
pub(super) struct Marked {
    shared: Arc<Shared>,
    jobs: SyncSender<()>,
    cancellations: SyncSender<()>,
    sound_cues: bool,
}

#[derive(Debug)]
struct Shared {
    playback: Mutex<Playback>,
    callbacks: Arc<Mutex<Callbacks>>,
}

struct Playback {
    schedule: Schedule,
    legacy_speech: bool,
    audio: Option<Audio>,
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Playback")
            .field("schedule", &self.schedule)
            .finish_non_exhaustive()
    }
}

struct MarkerSession {
    id: u64,
    progress: MarkProgress,
    shared: Arc<Shared>,
    marks: Vec<Vec<SoundCue>>,
    missed: Vec<SoundCue>,
    announce_start: bool,
}

#[derive(Clone, Copy)]
enum Completion {
    Ended,
    Cancelled,
    Failed,
}

impl Marked {
    pub(super) fn new(
        controller: Arc<Controller>,
        callbacks: Arc<Mutex<Callbacks>>,
    ) -> Result<Self, Error> {
        let audio = Audio::new().ok();
        let sound_cues = audio.is_some();
        let shared = Arc::new(Shared {
            playback: Mutex::new(Playback {
                schedule: Schedule::default(),
                legacy_speech: false,
                audio,
            }),
            callbacks,
        });
        let (jobs, pending) = sync_channel(1);
        let worker = Arc::clone(&shared);
        let rpc = Arc::clone(&controller);
        thread::Builder::new()
            .name("lector-nvda-speech".to_owned())
            .spawn(move || run_worker(&rpc, &worker, &pending))?;
        let (cancellations, cancel_requests) = sync_channel(1);
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("lector-nvda-cancel".to_owned())
            .spawn(move || cancel_worker(&controller, &worker, &cancel_requests))?;
        Ok(Self {
            shared,
            jobs,
            cancellations,
            sound_cues,
        })
    }

    pub(super) fn supports_cues(&self) -> bool {
        self.sound_cues
    }

    pub(super) fn submit(&self, text: &str, cues: &[SoundCueAt]) -> Result<UtteranceId, Error> {
        if !SoundCueAt::valid_for(text, cues) {
            return Err(Error::OutOfRange);
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut playback = self.shared.playback.lock();
        playback.schedule.submit(id, text, cues)?;
        if matches!(
            self.jobs.try_send(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(()))
        ) {
            playback.schedule.undo_submit();
            return Err(Error::OperationFailed("queue NVDA speech"));
        }
        Ok(UtteranceId::Nvda(id))
    }

    pub(super) fn cancel(&self) {
        let discarded = {
            let mut playback = self.shared.playback.lock();
            let discarded = playback.schedule.cancel();
            if let Some(audio) = &mut playback.audio {
                audio.stop();
            }
            if playback.schedule.cancelled_native().is_some() || playback.legacy_speech {
                let _ = self.cancellations.try_send(());
            }
            discarded
        };
        for job in discarded {
            notify(&self.shared, job.id, Completion::Cancelled);
        }
    }
}

impl Drop for Marked {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn run_worker(controller: &Controller, shared: &Arc<Shared>, requests: &Receiver<()>) {
    while requests.recv().is_ok() {
        loop {
            let job = shared.playback.lock().schedule.next();
            let Some(job) = job else { break };
            run_job(controller, shared, &job);
        }
    }
}

fn run_job(controller: &Controller, shared: &Arc<Shared>, job: &Job) {
    // The controller DLL exposes one callback pointer per process. Hold this
    // across the call and cleanup, even for multiple TTS instances.
    let _call = CALL.lock();
    if !shared.playback.lock().schedule.start(job) {
        notify(shared, job.id, Completion::Cancelled);
        return;
    }
    let completion = perform(controller, shared, job);
    let current = shared.playback.lock().schedule.complete(job.id);
    if let Some(current) = current {
        notify(
            shared,
            job.id,
            if current {
                completion
            } else {
                Completion::Cancelled
            },
        );
    }
}

fn cancel_worker(controller: &Controller, shared: &Shared, requests: &Receiver<()>) {
    while requests.recv().is_ok() {
        let deadline = Instant::now() + CANCEL_DEADLINE;
        loop {
            let mut playback = shared.playback.lock();
            if playback.legacy_speech {
                unsafe { (controller.cancel_speech)() };
                playback.legacy_speech = false;
            }
            let Some(id) = playback.schedule.cancelled_native() else {
                break;
            };
            if Instant::now() >= deadline {
                // Some old controller servers can lose a cancellation near
                // submission. Fail closed while callbacks from the old call
                // can still arrive, and release every accepted successor.
                let pending = playback.schedule.poison();
                drop(playback);
                notify(shared, id, Completion::Cancelled);
                for job in pending {
                    notify(shared, job.id, Completion::Failed);
                }
                break;
            }
            // Keep replacement registration serialized with cancellation so
            // a retry cannot stop the next job.
            unsafe { (controller.cancel_speech)() };
            drop(playback);
            thread::sleep(CANCEL_INTERVAL);
        }
    }
}

fn notify(shared: &Shared, id: u64, completion: Completion) {
    let id = UtteranceId::Nvda(id);
    let mut callbacks = shared.callbacks.lock();
    match completion {
        Completion::Ended => callbacks.utterance_end(id),
        Completion::Cancelled => callbacks.utterance_stop(id),
        Completion::Failed => callbacks.utterance_failure(id),
    }
}

fn perform(controller: &Controller, shared: &Arc<Shared>, job: &Job) -> Completion {
    if job.text.is_empty() {
        let mut playback = shared.playback.lock();
        if !playback.schedule.is_current(job.id) {
            return Completion::Cancelled;
        }
        let cues: Vec<_> = job.cues.iter().map(|cue| cue.cue).collect();
        if let Some(audio) = &mut playback.audio
            && audio.play(&cues).is_ok()
        {
            drop(playback);
            shared
                .callbacks
                .lock()
                .utterance_begin(UtteranceId::Nvda(job.id));
            return drain(shared, job.id);
        }
        drop(playback);
        return speak_ssml(controller, shared, job, true);
    }
    speak_ssml(controller, shared, job, false)
}

fn speak_ssml(
    controller: &Controller,
    shared: &Arc<Shared>,
    job: &Job,
    spoken: bool,
) -> Completion {
    let (status, delivered) = call_marked(controller, shared, job, spoken, true);
    if status == RPC_S_UNKNOWN_IF {
        // A different NVDA version may have started after the v2 probe.
        // Preserve feedback via plain speech and report that synchronized
        // completion was unavailable for this request.
        return speak_legacy_fallback(controller, shared, job);
    }
    if status != 0 {
        return stopped_or_failed(shared, status);
    }
    let missing = delivered.missed;
    if !missing.is_empty() {
        let labels = missing
            .iter()
            .map(|cue| cue.spoken_label())
            .collect::<Vec<_>>()
            .join(" ");
        let fallback = job.label_fallback(labels);
        let (status, _) = call_marked(
            controller,
            shared,
            &fallback,
            true,
            delivered.progress.next == 0,
        );
        if status != 0 {
            return stopped_or_failed(shared, status);
        }
    }
    drain(shared, job.id)
}

fn call_marked(
    controller: &Controller,
    shared: &Arc<Shared>,
    job: &Job,
    spoken: bool,
    announce_start: bool,
) -> (u32, MarkerSession) {
    let api = controller
        .marked
        .as_ref()
        .expect("marked controller was probed");
    let token = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let xml = make_ssml(&job.text, &job.cues, token, spoken);
    let input = HSTRING::from(xml.text);
    *SESSION.lock() = Some(MarkerSession {
        id: job.id,
        progress: MarkProgress { token, next: 0 },
        shared: Arc::clone(shared),
        marks: xml.marks,
        missed: Vec::new(),
        announce_start,
    });
    let registered = unsafe { (api.set_mark)(Some(mark_reached)) } == 0;
    let status = if registered {
        let current = shared.playback.lock().schedule.begin_native(job.id);
        if current {
            unsafe {
                (api.speak)(
                    input.as_ptr(),
                    SYMBOL_LEVEL_UNCHANGED,
                    PRIORITY_NORMAL,
                    SYNCHRONOUS,
                )
            }
        } else {
            ERROR_CANCELLED
        }
    } else {
        RPC_S_UNKNOWN_IF
    };
    shared.playback.lock().schedule.end_native(job.id);
    if registered {
        unsafe { (api.set_mark)(None) };
    }
    let mut session = SESSION
        .lock()
        .take()
        .expect("NVDA marker session installed");
    session.missed.extend(
        session.marks[session.progress.next..]
            .iter()
            .flatten()
            .copied(),
    );
    (status, session)
}

fn stopped_or_failed(shared: &Shared, status: u32) -> Completion {
    if let Some(audio) = &mut shared.playback.lock().audio {
        audio.stop();
    }
    if status == ERROR_CANCELLED {
        Completion::Cancelled
    } else {
        Completion::Failed
    }
}

fn speak_legacy_fallback(controller: &Controller, shared: &Shared, job: &Job) -> Completion {
    let text = HSTRING::from(spoken_text(&job.text, &job.cues));
    let mut playback = shared.playback.lock();
    if !playback.schedule.is_current(job.id) {
        return Completion::Cancelled;
    }
    if let Some(audio) = &mut playback.audio {
        audio.stop();
    }
    playback.legacy_speech = unsafe { (controller.speak_text)(text.as_ptr()) } == 0;
    Completion::Failed
}

fn drain(shared: &Shared, id: u64) -> Completion {
    let deadline = Instant::now() + DRAIN_DEADLINE;
    loop {
        {
            let mut playback = shared.playback.lock();
            if !playback.schedule.is_current(id) {
                return Completion::Cancelled;
            }
            let Some(audio) = &mut playback.audio else {
                return Completion::Ended;
            };
            if !audio.is_playing() {
                return Completion::Ended;
            }
            if Instant::now() >= deadline {
                audio.stop();
                return Completion::Failed;
            }
        }
        thread::sleep(DRAIN_INTERVAL);
    }
}

unsafe extern "system" fn mark_reached(mark: *const u16) -> u32 {
    if mark.is_null() {
        return 0;
    }
    // The DLL supplies a null-terminated WCHAR string for this call. Mark
    // names contain only two bounded decimal numbers; never scan further.
    let mut bytes = [0; 42];
    let mut len = 0;
    while len < bytes.len() {
        let unit = unsafe { *mark.add(len) };
        if unit == 0 {
            break;
        }
        let Ok(ascii) = u8::try_from(unit) else {
            return 0;
        };
        bytes[len] = ascii;
        len += 1;
    }
    if len == bytes.len() {
        return 0;
    }
    if let Some((id, index)) = parse_mark(&bytes[..len])
        && let Some(session) = SESSION.lock().as_mut()
    {
        session.reach(id, index);
    }
    0
}

impl MarkerSession {
    fn reach(&mut self, id: u64, index: usize) {
        let mut playback = self.shared.playback.lock();
        if !playback.schedule.is_current(self.id) {
            return;
        }
        let Some(skipped) = self.progress.reach(id, index, self.marks.len()) else {
            return;
        };
        let starts = skipped.start == 0 && self.announce_start;
        self.missed
            .extend(self.marks[skipped].iter().flatten().copied());
        let cues = &self.marks[index];
        if !cues.is_empty()
            && playback
                .audio
                .as_mut()
                .is_none_or(|audio| audio.play(cues).is_err())
        {
            self.missed.extend_from_slice(cues);
        }
        drop(playback);
        if starts {
            self.shared
                .callbacks
                .lock()
                .utterance_begin(UtteranceId::Nvda(self.id));
        }
    }
}

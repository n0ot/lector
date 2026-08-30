use lector::{
    app::{App, Clock},
    harness::Harness,
    output_scheduler::{
        EnqueueOutcome, OutputScheduler, OutputSchedulerConfig, ScheduledOutputClass,
    },
    presentation::{
        OutputTransaction, PresentedAccessibilityBundle, PresentedScene, PresentedViewFrame,
        RenderBatch, SurfaceId, ViewId, ViewRevision,
    },
    screen_reader::ScreenReader,
    speech,
    terminal::{
        ClipboardContent, ClipboardLocation, GhosttyEngine, ProgressState, TerminalEvent,
        TerminalGeometry, TerminalSnapshot,
    },
    terminal_protocol::PhysicalTerminalProfile,
    views,
};
use std::{cell::Cell, collections::VecDeque, io, io::Write, rc::Rc};

const SYNC_START: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn config() -> OutputSchedulerConfig {
    OutputSchedulerConfig {
        latency_budget_ms: 4,
        synchronization_timeout_ms: 40,
        synchronization_hard_timeout_ms: 200,
        write_budget_bytes: 64,
        maximum_pending_bytes: 256,
    }
}

fn batch(bytes: &[u8], marker: u16) -> RenderBatch {
    RenderBatch::new(
        vec![OutputTransaction::new(bytes)],
        PresentedScene::blank(TerminalGeometry::from_cells(3, marker)),
    )
}

fn accessibility_bundle(marker: u64) -> PresentedAccessibilityBundle {
    PresentedAccessibilityBundle::new(
        Some(ViewId(7)),
        vec![PresentedViewFrame {
            view_id: ViewId(7),
            revision: ViewRevision(marker),
            surface_id: SurfaceId(1),
            snapshot: TerminalSnapshot {
                geometry: TerminalGeometry::from_cells(3, marker as u16),
                title: Some(format!("frame-{marker}")),
                ..TerminalSnapshot::default()
            },
            history_revision: 0,
            history_basis: Default::default(),
            history: None,
            accessibility_epoch: Default::default(),
            application_auto_read_suppressed: false,
            application_cursor_tracking_suppressed: false,
            synchronized_output_closed: false,
            cursor_visibility_restored: false,
        }],
    )
}

#[test]
fn event_boundary_coalescing_keeps_only_the_newest_unstarted_scene_and_preserves_bells() {
    let mut scheduler = OutputScheduler::new(config(), true);
    assert_eq!(
        scheduler.enqueue_render(batch(b"stale", 10), 100),
        EnqueueOutcome::Queued
    );
    scheduler.enqueue_bell(2, 101);
    assert_eq!(
        scheduler.enqueue_render(batch(b"newest", 20), 102),
        EnqueueOutcome::ReplacedObsoleteRender
    );
    assert_eq!(scheduler.next_deadline_ms(), Some(104));

    let mut output = Vec::new();
    let early = scheduler
        .drain_ready(103, false, &mut output)
        .expect("early drain");
    assert_eq!(early.bytes_written, 0);
    assert!(output.is_empty());

    let ready = scheduler
        .drain_ready(104, false, &mut output)
        .expect("ready drain");
    assert_eq!(ready.completed_renders.len(), 1);
    assert_eq!(ready.completed_renders[0].geometry.cols, 20);
    assert_eq!(
        output,
        [SYNC_START, b"newest", SYNC_END, b"\x07\x07"].concat()
    );
    assert!(!output.windows(5).any(|window| window == b"stale"));
    assert_eq!(scheduler.pending_bytes(), 0);
}

#[test]
fn replacing_an_unstarted_render_replaces_its_accessibility_bundle() {
    let mut scheduler = OutputScheduler::new(config(), false);
    let obsolete = accessibility_bundle(10);
    let current = accessibility_bundle(20);
    assert_eq!(
        scheduler.enqueue_render_with_accessibility(batch(b"obsolete", 10), obsolete, 0),
        EnqueueOutcome::Queued
    );
    assert_eq!(
        scheduler.enqueue_render_with_accessibility(batch(b"current", 20), current.clone(), 1),
        EnqueueOutcome::ReplacedObsoleteRender
    );

    let mut output = Vec::new();
    let report = scheduler
        .drain_ready(4, false, &mut output)
        .expect("drain replacement");

    assert_eq!(output, b"current");
    assert_eq!(report.completed_renders.len(), 1);
    assert_eq!(report.completed_renders[0].accessibility, current);
}

#[test]
fn active_application_synchronization_refreshes_its_idle_deadline_until_close() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 10);
    scheduler.enqueue_render(batch(b"partial-one", 10), 10);
    scheduler.set_application_synchronized(true, 40);
    scheduler.enqueue_render(batch(b"partial-two", 11), 40);

    let mut output = Vec::new();
    let held = scheduler
        .drain_ready(50, false, &mut output)
        .expect("held synchronized output");
    assert_eq!(held.bytes_written, 0);
    assert!(output.is_empty());
    assert_eq!(scheduler.next_deadline_ms(), Some(80));

    scheduler.set_application_synchronized(true, 70);
    scheduler.enqueue_render(batch(b"complete", 12), 70);
    let still_active = scheduler
        .drain_ready(80, false, &mut output)
        .expect("active synchronized output");
    assert_eq!(still_active.bytes_written, 0);
    assert!(!still_active.synchronization_timed_out);
    assert!(output.is_empty());
    assert_eq!(scheduler.next_deadline_ms(), Some(110));

    scheduler.set_application_synchronized(false, 90);
    scheduler
        .drain_ready(90, false, &mut output)
        .expect("closed synchronized output");
    assert_eq!(output, [SYNC_START, b"complete", SYNC_END].concat());
}

#[test]
fn abandoned_application_synchronization_times_out_once_and_is_ignored_until_close() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 10);
    scheduler.enqueue_render(batch(b"partial-one", 10), 10);

    let mut output = Vec::new();
    let timed_out = scheduler
        .drain_ready(50, false, &mut output)
        .expect("time out idle synchronized output");
    assert!(timed_out.synchronization_timed_out);
    assert_eq!(output, [SYNC_START, b"partial-one", SYNC_END].concat());

    output.clear();
    scheduler.set_application_synchronized(true, 60);
    scheduler.enqueue_render(batch(b"partial-two", 11), 60);
    assert_eq!(scheduler.next_deadline_ms(), Some(64));
    let continued = scheduler
        .drain_ready(64, false, &mut output)
        .expect("continue an already timed-out synchronization epoch");
    assert!(!continued.synchronization_timed_out);
    assert_eq!(output, [SYNC_START, b"partial-two", SYNC_END].concat());

    scheduler.set_application_synchronization(true, true, 65);
    scheduler.enqueue_render(batch(b"same-batch-reopened", 12), 65);
    output.clear();
    let restarted = scheduler
        .drain_ready(69, false, &mut output)
        .expect("a close/reopen boundary starts a fresh held epoch");
    assert_eq!(restarted.bytes_written, 0);
    assert!(output.is_empty());

    scheduler.set_application_synchronized(false, 69);
    scheduler.set_application_synchronized(true, 70);
    scheduler.enqueue_render(batch(b"next-epoch", 12), 70);
    output.clear();
    let held = scheduler
        .drain_ready(74, false, &mut output)
        .expect("a closed transaction permits a new synchronized epoch");
    assert_eq!(held.bytes_written, 0);
    assert!(output.is_empty());
    assert_eq!(scheduler.next_deadline_ms(), Some(110));
}

#[test]
fn synchronization_timeout_publishes_only_after_the_released_render_is_flushed() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 0);
    scheduler.enqueue_render(batch(b"partial", 10), 0);
    let mut writer = ScriptedWriter::new([WriteStep::Error(io::ErrorKind::WouldBlock)]);

    let blocked = scheduler
        .drain_ready(40, false, &mut writer)
        .expect("attempt timed-out render");
    assert!(blocked.blocked);
    assert!(!blocked.synchronization_timed_out);
    assert!(blocked.completed_renders.is_empty());

    scheduler.notify_writable();
    let completed = scheduler
        .drain_ready(40, false, &mut writer)
        .expect("flush timed-out render");
    assert!(completed.synchronization_timed_out);
    assert_eq!(completed.completed_renders.len(), 1);
}

#[test]
fn synchronization_timeout_waits_for_the_newest_render_behind_an_older_partial_write() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.enqueue_render(batch(b"old", 9), 0);
    let mut writer = ScriptedWriter::new([
        WriteStep::Count(1),
        WriteStep::Error(io::ErrorKind::WouldBlock),
    ]);

    let old_blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("partially write the older render");
    assert!(old_blocked.blocked);

    scheduler.set_application_synchronized(true, 5);
    scheduler.enqueue_render(batch(b"released-partial", 10), 5);
    scheduler.notify_writable();
    let old_completed = scheduler
        .drain_ready(45, false, &mut writer)
        .expect("finish the render which predates the timeout target");
    assert_eq!(old_completed.completed_renders.len(), 1);
    assert_eq!(old_completed.completed_renders[0].geometry.cols, 9);
    assert!(!old_completed.synchronization_timed_out);
    assert!(old_completed.write_budget_exhausted);

    let released = scheduler
        .drain_ready(45, false, &mut writer)
        .expect("flush the newest render released by the timeout");
    assert_eq!(released.completed_renders.len(), 1);
    assert_eq!(released.completed_renders[0].geometry.cols, 10);
    assert!(released.synchronization_timed_out);
}

#[test]
fn continuously_active_application_synchronization_has_an_absolute_hard_cap() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 10);
    scheduler.enqueue_render(batch(b"partial-one", 10), 10);
    for now_ms in [40, 70, 100, 130] {
        scheduler.set_application_synchronized(true, now_ms);
        scheduler.enqueue_render(batch(b"latest-partial", 11), now_ms);
    }

    let mut output = Vec::new();
    let held = scheduler
        .drain_ready(139, false, &mut output)
        .expect("activity keeps the idle deadline open");
    assert_eq!(held.bytes_written, 0);
    assert!(!held.synchronization_timed_out);
    assert!(output.is_empty());
    assert_eq!(scheduler.next_deadline_ms(), Some(170));

    scheduler.set_application_synchronized(true, 160);
    assert_eq!(scheduler.next_deadline_ms(), Some(200));
    scheduler.set_application_synchronized(true, 190);
    assert_eq!(scheduler.next_deadline_ms(), Some(210));
    let hard_timed_out = scheduler
        .drain_ready(210, false, &mut output)
        .expect("hard-cap synchronized output");
    assert!(hard_timed_out.synchronization_timed_out);
    assert_eq!(output, [SYNC_START, b"latest-partial", SYNC_END].concat());
}

#[test]
fn synchronization_bypass_is_owned_by_one_render_until_its_flush() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 0);
    scheduler.set_application_synchronization_bypassed(true);
    scheduler.enqueue_render(batch(b"overlay", 10), 0);
    scheduler.enqueue_bell(1, 0);
    scheduler.set_application_synchronization_bypassed(false);

    let mut writer = FlushScriptedWriter {
        bytes: Vec::new(),
        flushes: VecDeque::from([io::ErrorKind::WouldBlock]),
    };
    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("write the bypass render before its flush blocks");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());
    assert_eq!(writer.bytes, [SYNC_START, b"overlay", SYNC_END].concat());

    scheduler.enqueue_render(batch(b"application", 20), 5);
    scheduler.notify_writable();
    let flushed = scheduler
        .drain_ready(5, false, &mut writer)
        .expect("confirm the bypass render and restore the application hold");
    assert_eq!(flushed.completed_renders.len(), 1);
    assert_eq!(flushed.completed_renders[0].geometry.cols, 10);
    assert_eq!(flushed.bytes_written, 0);
    assert_eq!(writer.bytes, [SYNC_START, b"overlay", SYNC_END].concat());
    assert_eq!(scheduler.next_deadline_ms(), Some(40));
}

#[test]
fn replacing_an_unstarted_bypass_render_transfers_its_release_boundary() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.set_application_synchronized(true, 0);
    scheduler.set_application_synchronization_bypassed(true);
    scheduler.enqueue_render(batch(b"obsolete-overlay", 10), 0);
    assert_eq!(
        scheduler.enqueue_render(batch(b"current-overlay", 20), 1),
        EnqueueOutcome::ReplacedObsoleteRender
    );

    let mut output = Vec::new();
    let report = scheduler
        .drain_ready(4, false, &mut output)
        .expect("present the replacement through the inherited bypass");

    assert_eq!(report.completed_renders.len(), 1);
    assert_eq!(report.completed_renders[0].geometry.cols, 20);
    assert_eq!(output, [SYNC_START, b"current-overlay", SYNC_END].concat());
    assert!(!contains(&output, b"obsolete-overlay"));
    assert_eq!(scheduler.next_deadline_ms(), Some(40));
}

#[test]
fn a_capacity_dropped_bypass_render_does_not_release_older_held_work() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 32,
            ..config()
        },
        true,
    );
    scheduler.set_application_synchronized(true, 0);
    scheduler.enqueue_render(batch(b"held", 10), 0);
    scheduler.set_application_synchronization_bypassed(true);
    assert_eq!(
        scheduler.enqueue_render(batch(&[b'x'; 17], 20), 1),
        EnqueueOutcome::DroppedForCapacity
    );

    let mut output = Vec::new();
    let held = scheduler
        .drain_ready(4, false, &mut output)
        .expect("a rejected compositor render cannot arm a global bypass");

    assert_eq!(held.bytes_written, 0);
    assert!(held.completed_renders.is_empty());
    assert!(output.is_empty());
    assert_eq!(scheduler.next_deadline_ms(), Some(40));
}

#[test]
fn scheduler_owns_exactly_one_outer_synchronized_output_boundary() {
    let mut scheduler = OutputScheduler::new(config(), true);
    scheduler.enqueue_render(
        batch(
            &[SYNC_START, b"one", SYNC_END, SYNC_START, b"two", SYNC_END].concat(),
            20,
        ),
        0,
    );

    let mut output = Vec::new();
    scheduler
        .drain_ready(4, false, &mut output)
        .expect("drain globally synchronized render");

    assert_eq!(output, [SYNC_START, b"onetwo", SYNC_END].concat());
    assert_eq!(output.matches(SYNC_START), 1);
    assert_eq!(output.matches(SYNC_END), 1);
}

#[test]
fn pending_render_accounting_tracks_a_synchronization_capability_change() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"frame", 20), 0);
    assert_eq!(scheduler.pending_bytes(), b"frame".len());

    scheduler.set_synchronized_output_supported(true);
    assert_eq!(
        scheduler.pending_bytes(),
        b"frame".len() + SYNC_START.len() + SYNC_END.len()
    );
    scheduler.set_synchronized_output_supported(false);
    assert_eq!(scheduler.pending_bytes(), b"frame".len());

    scheduler.set_synchronized_output_supported(true);
    let mut output = Vec::new();
    scheduler
        .drain_ready(4, false, &mut output)
        .expect("drain render after capability change");
    assert_eq!(output, [SYNC_START, b"frame", SYNC_END].concat());
}

#[test]
fn partial_writes_eintr_and_eagain_never_interleave_generated_transactions() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_bytes(
        ScheduledOutputClass::Control,
        b"\x1b]2;complete-title\x1b\\".to_vec(),
        0,
    );
    scheduler.enqueue_bytes(ScheduledOutputClass::Control, b"after-title".to_vec(), 0);
    let expected = b"\x1b]2;complete-title\x1b\\after-title";
    let mut writer = ScriptedWriter::new([
        WriteStep::Error(io::ErrorKind::Interrupted),
        WriteStep::Count(2),
        WriteStep::Error(io::ErrorKind::WouldBlock),
        WriteStep::Count(3),
        WriteStep::Count(usize::MAX),
    ]);

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("EAGAIN is backpressure, not failure");
    assert!(blocked.blocked);
    assert_eq!(writer.bytes, &expected[..2]);
    assert!(scheduler.pending_bytes() > 0);
    assert_eq!(scheduler.next_deadline_ms(), None);

    scheduler.notify_writable();
    let complete = scheduler
        .drain_ready(5, false, &mut writer)
        .expect("resume partial transaction");
    assert!(!complete.blocked);
    assert_eq!(writer.bytes, expected);
    assert_eq!(scheduler.pending_bytes(), 0);
}

#[test]
fn a_partially_written_render_finishes_before_the_newest_scene_starts() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"first-render", 10), 0);
    let mut writer = ScriptedWriter::new([
        WriteStep::Count(5),
        WriteStep::Error(io::ErrorKind::WouldBlock),
    ]);

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("partially write first render");
    assert!(blocked.blocked);
    assert_eq!(writer.bytes, b"first");

    assert_eq!(
        scheduler.enqueue_render(batch(b"latest-render", 20), 5),
        EnqueueOutcome::Queued,
        "a started transaction cannot be discarded"
    );
    scheduler.notify_writable();
    let first_complete = scheduler
        .drain_ready(9, false, &mut writer)
        .expect("finish the transaction that already started");
    assert_eq!(writer.bytes, b"first-render");
    assert_eq!(first_complete.completed_renders.len(), 1);
    assert_eq!(first_complete.completed_renders[0].geometry.cols, 10);
    assert!(
        first_complete.write_budget_exhausted,
        "a boundary drain must keep going when a newer scene is queued behind the completed one"
    );

    let latest_complete = scheduler
        .drain_ready(9, false, &mut writer)
        .expect("start the queued authoritative scene after the first flush");

    assert_eq!(writer.bytes, b"first-renderlatest-render");
    assert_eq!(latest_complete.completed_renders.len(), 1);
    assert_eq!(latest_complete.completed_renders[0].geometry.cols, 20);
}

#[test]
fn backpressured_renders_complete_with_their_exact_accessibility_bundles_in_order() {
    let mut scheduler = OutputScheduler::new(config(), false);
    let first_accessibility = accessibility_bundle(10);
    let second_accessibility = accessibility_bundle(20);
    scheduler.enqueue_render_with_accessibility(
        batch(b"first-render", 10),
        first_accessibility.clone(),
        0,
    );
    let mut writer = ScriptedWriter::new([
        WriteStep::Count(5),
        WriteStep::Error(io::ErrorKind::WouldBlock),
    ]);

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("partially write first render");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());
    assert_eq!(
        scheduler.enqueue_render_with_accessibility(
            batch(b"second-render", 20),
            second_accessibility.clone(),
            5,
        ),
        EnqueueOutcome::Queued,
        "a started render and its accessibility bundle cannot be replaced"
    );

    scheduler.notify_writable();
    let first = scheduler
        .drain_ready(5, false, &mut writer)
        .expect("finish first render");
    assert_eq!(first.completed_renders.len(), 1);
    assert_eq!(
        first.completed_renders[0].accessibility,
        first_accessibility
    );

    let second = scheduler
        .drain_ready(9, false, &mut writer)
        .expect("finish second render");
    assert_eq!(second.completed_renders.len(), 1);
    assert_eq!(
        second.completed_renders[0].accessibility,
        second_accessibility
    );
    assert_eq!(writer.bytes, b"first-rendersecond-render");
}

#[test]
fn disconnect_requires_reconciliation_and_recovery_accepts_a_fresh_full_scene() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"incremental", 10), 0);
    let mut broken = ScriptedWriter::new([
        WriteStep::Count(4),
        WriteStep::Error(io::ErrorKind::BrokenPipe),
    ]);
    let error = scheduler
        .drain_ready(4, false, &mut broken)
        .expect_err("disconnect must be reported");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(scheduler.needs_reconciliation());
    assert_eq!(scheduler.pending_bytes(), 0);

    scheduler.recover();
    assert!(!scheduler.needs_reconciliation());
    scheduler.enqueue_render(batch(b"full-redraw", 20), 5);
    let mut recovered = Vec::new();
    let report = scheduler
        .drain_ready(9, false, &mut recovered)
        .expect("recovered full scene");
    assert_eq!(recovered, b"full-redraw");
    assert_eq!(report.completed_renders[0].geometry.cols, 20);
}

#[test]
fn render_confirmation_waits_for_a_successful_flush_after_flush_backpressure_and_eintr() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"render", 20), 0);
    let mut writer = FlushScriptedWriter {
        bytes: Vec::new(),
        flushes: VecDeque::from([
            io::ErrorKind::WouldBlock,
            io::ErrorKind::Interrupted,
            io::ErrorKind::Other,
        ]),
    };

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("flush backpressure");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());
    assert_eq!(writer.bytes, b"render");

    let confirmed = scheduler
        .drain_ready(5, true, &mut writer)
        .expect("flush retries interrupted writes");
    assert_eq!(confirmed.completed_renders.len(), 1);
    assert_eq!(confirmed.completed_renders[0].geometry.cols, 20);
    assert_eq!(writer.bytes, b"render", "accepted bytes must not be resent");
}

#[test]
fn lifecycle_cleanup_discards_a_render_receipt_which_terminal_restore_will_supersede() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render_with_accessibility(batch(b"render", 20), accessibility_bundle(20), 0);
    let mut writer = FlushScriptedWriter {
        bytes: Vec::new(),
        flushes: VecDeque::from([io::ErrorKind::WouldBlock]),
    };

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("write the render before its flush blocks");
    assert!(blocked.blocked);
    assert!(blocked.completed_renders.is_empty());

    scheduler.prepare_for_lifecycle_cleanup();
    scheduler.enqueue_bytes(ScheduledOutputClass::Control, b"restore".to_vec(), 5);
    let restored = scheduler
        .drain_ready(5, true, &mut writer)
        .expect("flush prior bytes and complete terminal restoration");

    assert_eq!(writer.bytes, b"renderrestore");
    assert!(restored.completed_renders.is_empty());
    assert!(!scheduler.has_render_work());
}

#[test]
fn a_zero_byte_render_is_confirmed_without_waiting_for_an_impossible_flush() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"", 20), 0);
    let mut output = Vec::new();

    let report = scheduler
        .drain_ready(4, false, &mut output)
        .expect("complete no-op render");

    assert!(output.is_empty());
    assert_eq!(report.completed_renders.len(), 1);
    assert_eq!(report.completed_renders[0].geometry.cols, 20);
    assert!(!scheduler.has_render_work());
}

#[test]
fn render_work_is_started_only_after_a_render_byte_is_accepted() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            write_budget_bytes: 2,
            ..config()
        },
        false,
    );
    scheduler.enqueue_render(batch(b"render", 20), 0);
    assert!(scheduler.has_render_work());
    assert!(!scheduler.has_started_render_work());

    let mut output = Vec::new();
    scheduler
        .drain_ready(4, false, &mut output)
        .expect("start the render");
    assert_eq!(output, b"re");
    assert!(scheduler.has_started_render_work());

    while scheduler.has_render_work() {
        scheduler
            .drain_ready(4, true, &mut output)
            .expect("finish the render");
    }
    assert!(!scheduler.has_render_work());
    assert!(!scheduler.has_started_render_work());
}

#[test]
fn typed_terminal_effects_are_ordered_bounded_and_never_replay_unsafe_payloads() {
    let mut scheduler = OutputScheduler::new(config(), false);
    let effects = [
        TerminalEvent::TitleChanged("typed title".into()),
        TerminalEvent::WorkingDirectoryChanged("file://localhost/tmp/typed".into()),
        TerminalEvent::ProgressReport {
            state: ProgressState::Set,
            progress: Some(42),
        },
        TerminalEvent::ClipboardWrite {
            location: ClipboardLocation::Standard,
            contents: vec![ClipboardContent {
                mime: "text/plain".into(),
                data: b"private clipboard".to_vec(),
            }],
        },
        TerminalEvent::DesktopNotification {
            title: "notice".into(),
            body: "review locally".into(),
        },
    ];
    for effect in effects.clone() {
        scheduler.enqueue_terminal_effect(SurfaceId(7), effect, 0);
    }
    let mut output = Vec::new();
    let report = scheduler
        .drain_ready(4, false, &mut output)
        .expect("drain typed effects");

    assert!(contains(&output, b"\x1b]2;typed title\x1b\\"));
    assert!(contains(
        &output,
        b"\x1b]7;file://localhost/tmp/typed\x1b\\"
    ));
    assert!(contains(&output, b"\x1b]9;4;1;42\x1b\\"));
    assert!(!contains(&output, b"private clipboard"));
    assert!(!contains(&output, b"review locally"));
    assert_eq!(report.completed_effects.len(), effects.len());
    assert_eq!(
        report
            .completed_effects
            .iter()
            .map(|effect| effect.owner)
            .collect::<Vec<_>>(),
        vec![SurfaceId(7); effects.len()]
    );
    assert_eq!(
        report
            .completed_effects
            .into_iter()
            .map(|effect| effect.event)
            .collect::<Vec<_>>(),
        effects
    );
}

#[test]
fn compositor_bypass_replaces_unstarted_working_frame_model_effects() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            write_budget_bytes: 1024,
            ..config()
        },
        true,
    );
    scheduler.set_application_synchronized(true, 0);
    scheduler.enqueue_terminal_effect(
        SurfaceId(1),
        TerminalEvent::TitleChanged("working title".into()),
        0,
    );
    scheduler.enqueue_terminal_effect(
        SurfaceId(1),
        TerminalEvent::WorkingDirectoryChanged("file://localhost/working".into()),
        0,
    );
    scheduler.enqueue_render(batch(b"working frame", 10), 0);

    scheduler.enqueue_terminal_effect(
        SurfaceId(1),
        TerminalEvent::TitleChanged("committed title".into()),
        1,
    );
    scheduler.enqueue_terminal_effect(
        SurfaceId(1),
        TerminalEvent::WorkingDirectoryChanged("file://localhost/committed".into()),
        1,
    );
    scheduler.set_application_synchronization_bypassed(true);
    scheduler.enqueue_render(batch(b"committed frame", 20), 1);

    let mut output = Vec::new();
    let report = scheduler
        .drain_ready(4, false, &mut output)
        .expect("present committed compositor transition");

    assert!(contains(&output, b"committed title"));
    assert!(contains(&output, b"file://localhost/committed"));
    assert!(contains(&output, b"committed frame"));
    assert!(!contains(&output, b"working title"));
    assert!(!contains(&output, b"file://localhost/working"));
    assert!(!contains(&output, b"working frame"));
    assert!(
        report
            .application_synchronization_bypass_completed
            .is_some()
    );
}

#[test]
fn a_capacity_rejected_bypass_does_not_steal_the_started_generations_receipt() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            latency_budget_ms: 0,
            write_budget_bytes: 1,
            maximum_pending_bytes: 16,
            ..config()
        },
        false,
    );
    scheduler.set_application_synchronized(true, 0);
    scheduler.set_application_synchronization_bypassed(true);
    assert_eq!(
        scheduler.enqueue_render(batch(b"old", 10), 0),
        EnqueueOutcome::Queued
    );

    let mut output = Vec::new();
    let partial = scheduler
        .drain_ready(0, false, &mut output)
        .expect("start the first bypass");
    assert_eq!(partial.bytes_written, 1);
    scheduler.set_application_synchronization_bypassed(true);
    assert_eq!(
        scheduler.enqueue_render(batch(b"new-replacement", 20), 1),
        EnqueueOutcome::DroppedForCapacity
    );

    let mut first_receipt = None;
    for now_ms in 1..8 {
        let report = scheduler
            .drain_ready(now_ms, true, &mut output)
            .expect("finish the retained bypass");
        first_receipt = first_receipt.or(report.application_synchronization_bypass_completed);
    }
    assert_eq!(first_receipt, Some(1));

    scheduler.set_application_synchronization_bypassed(true);
    assert_eq!(
        scheduler.enqueue_render(batch(b"retry", 20), 8),
        EnqueueOutcome::Queued
    );
    let mut retry_receipt = None;
    for now_ms in 8..16 {
        let report = scheduler
            .drain_ready(now_ms, true, &mut output)
            .expect("finish the retry bypass");
        retry_receipt = retry_receipt.or(report.application_synchronization_bypass_completed);
    }
    assert_eq!(retry_receipt, Some(2));
}

#[test]
fn typed_effect_retention_is_bounded_even_when_zero_byte_effects_hold_large_payloads() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 96,
            write_budget_bytes: 256,
            ..config()
        },
        false,
    );

    for marker in 0..100_u8 {
        scheduler.enqueue_terminal_effect(
            SurfaceId(7),
            TerminalEvent::ClipboardWrite {
                location: ClipboardLocation::Standard,
                contents: vec![ClipboardContent {
                    mime: "text/plain".into(),
                    data: vec![marker; 4 * 1024],
                }],
            },
            marker.into(),
        );
        assert!(scheduler.pending_bytes() <= 96);
        assert_eq!(scheduler.pending_effect_count(), 1);
    }

    let mut output = Vec::new();
    let report = scheduler
        .drain_ready(103, false, &mut output)
        .expect("drain bounded zero-byte effect");

    assert!(
        output.is_empty(),
        "clipboard payloads must never be replayed"
    );
    assert_eq!(scheduler.pending_bytes(), 0);
    assert_eq!(scheduler.pending_effect_count(), 0);
    assert_eq!(report.completed_effects.len(), 1);
    let TerminalEvent::ClipboardWrite { contents, .. } = &report.completed_effects[0].event else {
        panic!("the newest coalesced clipboard effect must be retained");
    };
    assert_eq!(contents.len(), 1);
    assert_eq!(contents[0].data, vec![99; 70]);
}

#[test]
fn backpressured_active_effects_count_against_the_retention_limit() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 96,
            write_budget_bytes: 256,
            ..config()
        },
        false,
    );
    scheduler.enqueue_terminal_effect(
        SurfaceId(7),
        TerminalEvent::TitleChanged("x".repeat(4 * 1024)),
        0,
    );
    let mut writer = ScriptedWriter::new([WriteStep::Error(io::ErrorKind::WouldBlock)]);
    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("backpressure the active typed effect");
    assert!(blocked.blocked);
    assert_eq!(scheduler.pending_effect_count(), 1);

    scheduler.enqueue_terminal_effect(
        SurfaceId(8),
        TerminalEvent::WorkingDirectoryChanged("y".repeat(4 * 1024)),
        5,
    );
    scheduler.enqueue_terminal_effect(
        SurfaceId(7),
        TerminalEvent::TitleChanged("newer".repeat(1024)),
        5,
    );
    assert_eq!(
        scheduler.pending_effect_count(),
        1,
        "an in-flight retained payload must prevent a second effect from exceeding the cap"
    );

    scheduler.notify_writable();
    let completed = scheduler
        .drain_ready(5, true, &mut writer)
        .expect("finish the bounded active effect");
    assert_eq!(completed.completed_effects.len(), 1);
    assert_eq!(scheduler.pending_effect_count(), 0);
}

#[test]
fn a_render_waiting_for_flush_is_still_reported_as_in_flight_work() {
    let mut scheduler = OutputScheduler::new(config(), false);
    scheduler.enqueue_render(batch(b"accepted", 20), 0);
    let mut writer = FlushScriptedWriter {
        bytes: Vec::new(),
        flushes: VecDeque::from([io::ErrorKind::WouldBlock]),
    };

    let blocked = scheduler
        .drain_ready(4, false, &mut writer)
        .expect("flush backpressure");

    assert!(blocked.blocked);
    assert!(scheduler.has_render_work());
}

#[test]
fn bounded_backlog_discards_obsolete_visual_work_without_dropping_control_or_bells() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 32,
            ..config()
        },
        false,
    );
    scheduler.enqueue_bytes(ScheduledOutputClass::Control, b"control".to_vec(), 0);
    scheduler.enqueue_render(batch(b"first-obsolete-render", 10), 0);
    scheduler.enqueue_bell(1, 0);
    assert_eq!(
        scheduler.enqueue_render(batch(b"latest-render", 20), 1),
        EnqueueOutcome::ReplacedObsoleteRender
    );
    assert!(scheduler.pending_bytes() <= 32);

    let mut output = Vec::new();
    scheduler
        .drain_ready(4, false, &mut output)
        .expect("bounded drain");
    assert_eq!(output, b"controllatest-render\x07");
}

#[test]
fn bell_flood_is_capped_before_it_can_materialize_an_unbounded_transaction() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 96,
            write_budget_bytes: 256,
            ..config()
        },
        false,
    );
    scheduler.enqueue_bell(usize::MAX, 0);
    assert_eq!(scheduler.pending_bytes(), 96);

    let mut output = Vec::new();
    scheduler
        .drain_ready(4, false, &mut output)
        .expect("drain bounded bell flood");
    assert_eq!(output, vec![b'\x07'; 96]);
    assert_eq!(scheduler.pending_bytes(), 0);
}

#[test]
fn oversized_render_is_dropped_and_does_not_poison_a_later_bounded_scene() {
    let mut scheduler = OutputScheduler::new(
        OutputSchedulerConfig {
            maximum_pending_bytes: 32,
            ..config()
        },
        false,
    );
    assert_eq!(
        scheduler.enqueue_render(batch(&[b'x'; 33], 10), 0),
        EnqueueOutcome::DroppedForCapacity
    );
    assert_eq!(scheduler.pending_bytes(), 0);
    assert_eq!(
        scheduler.enqueue_render(batch(b"recovered", 20), 1),
        EnqueueOutcome::Queued
    );

    let mut output = Vec::new();
    scheduler
        .drain_ready(5, false, &mut output)
        .expect("drain render after capacity rejection");
    assert_eq!(output, b"recovered");
}

#[test]
fn application_harness_serializes_latest_scene_bells_resize_and_replies_without_delaying_input() {
    let clock = FakeClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(4, 20)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).expect("application");
    let mut physical_profile =
        PhysicalTerminalProfile::conservative(TerminalGeometry::from_cells(4, 20));
    physical_profile.synchronized_output = true;
    app.set_physical_profile(physical_profile);
    app.enable_output_scheduler(OutputSchedulerConfig {
        write_budget_bytes: 4096,
        maximum_pending_bytes: 8192,
        ..config()
    });
    let mut reader = ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)));
    let mut pty_input = Vec::new();
    let mut physical = Vec::new();

    app.handle_pty(
        &mut reader,
        b"\x1b]2;scheduled-title\x07base\x07\x1b[6n",
        &mut physical,
    )
    .expect("simultaneous title, bell, text, and terminal reply");
    app.handle_stdin(&mut reader, b"x", &mut pty_input, &mut physical)
        .expect("input remains responsive");
    app.handle_tick(&mut reader, &mut pty_input, &mut physical)
        .expect("route terminal reply");
    assert!(pty_input.starts_with(b"x"));
    assert!(pty_input.windows(2).any(|bytes| bytes == b"\x1b["));
    assert!(physical.is_empty());

    app.on_resize_with_geometry(TerminalGeometry::from_cells(5, 24), &mut physical)
        .expect("queue resize");
    app.show_message(&mut reader, "Notice", "latest overlay", &mut physical)
        .expect("queue overlay");
    clock.advance_ms(3);
    let early = app
        .drain_scheduled_output(&mut physical, false)
        .expect("early event boundary");
    assert_eq!(early.bytes_written, 0);
    assert!(physical.is_empty());

    clock.advance_ms(1);
    let ready = app
        .drain_scheduled_output(&mut physical, false)
        .expect("ready event boundary");
    assert_eq!(ready.completed_renders.len(), 1);
    assert_eq!(physical.matches(SYNC_START), 1);
    assert_eq!(physical.matches(SYNC_END), 1);
    assert_eq!(physical.last(), Some(&b'\x07'));
    let sync_end = physical
        .windows(SYNC_END.len())
        .position(|bytes| bytes == SYNC_END)
        .expect("global synchronized-output close");
    assert!(sync_end + SYNC_END.len() < physical.len());

    let mut oracle = GhosttyEngine::new(5, 24).expect("physical oracle");
    oracle.advance(&physical).expect("parse scheduled output");
    let snapshot = oracle.normalized_snapshot();
    let contents = snapshot.contents();
    assert_eq!(snapshot.title.as_deref(), Some("scheduled-title"));
    assert!(contents.contains("latest overlay"), "{contents:?}");
    assert!(!contents.contains("base"));
}

#[test]
fn application_sync_intent_is_held_and_timed_out_without_leaking_into_the_physical_shadow() {
    let clock = FakeClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(3, 20)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).expect("application");
    let mut profile = PhysicalTerminalProfile::conservative(TerminalGeometry::from_cells(3, 20));
    profile.synchronized_output = true;
    app.set_physical_profile(profile);
    app.enable_output_scheduler(OutputSchedulerConfig {
        write_budget_bytes: 4096,
        maximum_pending_bytes: 8192,
        ..config()
    });
    let mut reader = ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)));
    let mut physical = Vec::new();

    app.handle_pty(&mut reader, b"\x1b[?2026hheld", &mut physical)
        .expect("open application synchronization");
    clock.advance_ms(39);
    let held = app
        .drain_scheduled_output(&mut physical, false)
        .expect("hold incomplete frame");
    assert_eq!(held.bytes_written, 0);
    assert!(physical.is_empty());

    clock.advance_ms(1);
    let timed_out = app
        .drain_scheduled_output(&mut physical, false)
        .expect("time out abandoned synchronization");
    assert!(timed_out.synchronization_timed_out);
    assert_eq!(physical.matches(SYNC_START), 1);
    assert_eq!(physical.matches(SYNC_END), 1);
    let shadow = app.presented_scene().clone().into_terminal_snapshot();
    assert!(!shadow.modes.synchronized_output);

    let mut oracle = GhosttyEngine::new(3, 20).expect("physical oracle");
    oracle.advance(&physical).expect("parse timed-out frame");
    assert!(!oracle.normalized_snapshot().modes.synchronized_output);
    assert!(oracle.normalized_snapshot().contents().contains("held"));
}

#[test]
fn lector_overlay_is_not_frozen_by_application_synchronization_intent() {
    let clock = FakeClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(6, 24)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).expect("application");
    app.enable_output_scheduler(OutputSchedulerConfig {
        write_budget_bytes: 4096,
        maximum_pending_bytes: 8192,
        ..config()
    });
    let mut reader = ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)));
    let mut physical = Vec::new();

    app.handle_pty(&mut reader, b"\x1b[?2026hheld", &mut physical)
        .expect("open application synchronization");
    app.show_message(&mut reader, "Lector", "responsive overlay", &mut physical)
        .expect("queue compositor-owned overlay");
    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(4))
    );
    clock.advance_ms(4);
    let report = app
        .drain_scheduled_output(&mut physical, false)
        .expect("present overlay without waiting for application timeout");

    assert_eq!(
        report.completed_renders.len(),
        1,
        "report={report:?}, physical={physical:?}"
    );
    assert!(!report.synchronization_timed_out);
    let mut oracle = GhosttyEngine::new(6, 24).expect("physical oracle");
    oracle.advance(&physical).expect("parse overlay frame");
    let contents = oracle.normalized_snapshot().contents();
    assert!(contents.contains("responsive overlay"), "{contents:?}");

    assert_eq!(
        app.scheduled_output_timeout(),
        Some(std::time::Duration::from_millis(36)),
        "the compositor bypass must retain the application's timeout epoch"
    );
    clock.advance_ms(36);
    let timeout = app
        .drain_scheduled_output(&mut physical, false)
        .expect("time out the still-open application frame behind the overlay");
    assert!(timeout.synchronization_timed_out);
}

#[test]
fn application_input_and_latest_scene_remain_responsive_when_the_physical_writer_is_backpressured()
{
    let clock = FakeClock::default();
    let stack = views::ViewStack::new(Box::new(views::PtyView::new(3, 20)));
    let mut app = App::new_with_clock(stack, Box::new(clock.clone())).expect("application");
    app.enable_output_scheduler(OutputSchedulerConfig {
        write_budget_bytes: 4096,
        maximum_pending_bytes: 8192,
        ..config()
    });
    let mut reader = ScreenReader::new(speech::Speech::new(Box::new(SilentDriver)));
    let mut discarded_output = ScriptedWriter::new([WriteStep::Error(io::ErrorKind::WouldBlock)]);
    let mut pty_input = Vec::new();

    app.handle_pty(&mut reader, b"stale", &mut Vec::new())
        .expect("queue stale scene");
    clock.advance_ms(4);
    let blocked = app
        .drain_scheduled_output(&mut discarded_output, false)
        .expect("physical backpressure");
    assert!(blocked.blocked);
    assert!(discarded_output.bytes.is_empty());

    app.handle_stdin(&mut reader, b"x", &mut pty_input, &mut Vec::new())
        .expect("input while output is blocked");
    assert_eq!(pty_input, b"x");
    app.handle_pty(&mut reader, b"\x1b[2J\x1b[Hlatest", &mut Vec::new())
        .expect("queue newest authoritative scene");

    let mut recovered = Vec::new();
    let report = app
        .drain_scheduled_output(&mut recovered, true)
        .expect("resume output");
    assert_eq!(report.completed_renders.len(), 1);
    assert!(!recovered.windows(5).any(|bytes| bytes == b"stale"));
    let mut oracle = GhosttyEngine::new(3, 20).expect("physical oracle");
    oracle.advance(&recovered).expect("parse recovered output");
    assert!(oracle.normalized_snapshot().contents().contains("latest"));
}

#[test]
fn project_harness_coalesces_event_burst_and_matches_the_ghostty_oracle() {
    let mut harness = Harness::new_scheduled(3, 20).expect("scheduled harness");

    harness
        .handle_pty_output(b"stale")
        .expect("queue first application update");
    harness
        .handle_pty_output(b"\x1b[2J\x1b[Hlatest")
        .expect("queue authoritative replacement");
    assert!(harness.terminal_output().is_empty());

    harness.tick(4).expect("reach the scheduler deadline");
    let report = harness
        .drain_scheduled_output(false)
        .expect("drain harness presentation output");
    assert_eq!(report.completed_renders.len(), 1);

    let mut oracle = GhosttyEngine::new(3, 20).expect("physical oracle");
    oracle
        .advance(harness.terminal_output())
        .expect("parse harness output");
    let contents = oracle.normalized_snapshot().contents();
    assert!(contents.contains("latest"), "{contents:?}");
    assert!(!contents.contains("stale"), "{contents:?}");
}

#[derive(Clone, Copy)]
enum WriteStep {
    Count(usize),
    Error(io::ErrorKind),
}

struct ScriptedWriter {
    steps: VecDeque<WriteStep>,
    bytes: Vec<u8>,
}

impl ScriptedWriter {
    fn new(steps: impl IntoIterator<Item = WriteStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            bytes: Vec::new(),
        }
    }
}

impl Write for ScriptedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self
            .steps
            .pop_front()
            .unwrap_or(WriteStep::Count(usize::MAX))
        {
            WriteStep::Count(limit) => {
                let count = bytes.len().min(limit);
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            WriteStep::Error(kind) => Err(io::Error::new(kind, "scripted writer failure")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FlushScriptedWriter {
    bytes: Vec<u8>,
    flushes: VecDeque<io::ErrorKind>,
}

impl Write for FlushScriptedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.flushes.pop_front() {
            Some(io::ErrorKind::Other) | None => Ok(()),
            Some(kind) => Err(io::Error::new(kind, "scripted flush")),
        }
    }
}

trait ByteMatches {
    fn matches(&self, needle: &[u8]) -> usize;
}

impl ByteMatches for Vec<u8> {
    fn matches(&self, needle: &[u8]) -> usize {
        self.windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }
}

#[derive(Clone, Default)]
struct FakeClock(Rc<Cell<u128>>);

impl FakeClock {
    fn advance_ms(&self, delta: u128) {
        self.0.set(self.0.get().saturating_add(delta));
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u128 {
        self.0.get()
    }
}

struct SilentDriver;

impl speech::Driver for SilentDriver {
    fn speak(&mut self, _text: &str, _interrupt: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn get_rate(&self) -> f32 {
        1.0
    }

    fn set_rate(&mut self, _rate: f32) -> anyhow::Result<()> {
        Ok(())
    }
}

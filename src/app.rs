use crate::{
    application_accessibility::ApplicationAccessibilityPolicy,
    commands,
    keymap::Binding,
    output_scheduler::{DrainReport, OutputScheduler, OutputSchedulerConfig, ScheduledOutputClass},
    presentation::{
        CursorOwner, GridPoint, IncrementalVtRenderer, PhysicalTerminalLifecycle, PresentedScene,
        RenderCapabilities, RendererBackend, Scene, SceneDamage, SceneOverlay, SceneSurface,
        SurfaceId, ViewId,
    },
    screen_reader::{ScreenReader, TmuxBellMode},
    terminal::{ScreenIdentity, TerminalGeometry, UpdateSummary},
    terminal_input::KeyInput,
    terminal_protocol::{
        ApplicationReplyBroker, PhysicalTerminalProfile, ProbePolicy, StartupProbeBroker,
        TerminalEffectPolicy,
    },
    tmux_gateway::TmuxGatewayRouter,
    tmux_lifecycle::{ConnectionHierarchy, GatewayOrigin},
    tmux_model::TmuxTopology,
    view::View,
    views,
};
use anyhow::{Context, Result};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    io::Write,
    sync::LazyLock,
    time,
};
use terminput::{Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

mod input;
mod protocol;
mod pty;
mod tmux_interaction;
mod tmux_prefix;
mod view_stack;

use protocol::{
    FOCUS_IN_EVENT, FOCUS_OUT_EVENT, ModifyOtherKeysStatus, SequenceStatus, focus_event_status,
    is_invalid_ss3_prefix, modify_other_keys_status, osc_status, timed_out_event,
};

const ROOT_SOURCE: SurfaceId = SurfaceId(1);
const MAX_NESTED_TMUX_GATEWAYS: usize = 4_096;
const TMUX_TERMINATOR_TIMEOUT_MS: u128 = 1_000;
const TMUX_FORCE_ABANDON_GRACE_MS: u128 = 750;
const TMUX_SHUTDOWN_DETACH_TIMEOUT_MS: u128 = 200;
const TMUX_BELL_COALESCE_MS: u128 = 250;
const TMUX_RECOVERY_QUIET_MS: u128 = 100;
const TMUX_RECOVERY_HARD_DEADLINE_MS: u128 = 1_000;
const TMUX_FLOW_RETRY_BASE_MS: u128 = 100;
const TMUX_FLOW_RETRY_MAX_MS: u128 = 2_000;
const TMUX_HIDDEN_IMMEDIATE_BUDGET_BYTES: usize = 4 * 1024;
const TMUX_BACKGROUND_DRAIN_BUDGET_BYTES: usize = 4 * 1024;
const TMUX_BACKGROUND_PAUSE_THRESHOLD_BYTES: usize = 16 * 1024;
const TMUX_BACKGROUND_OUTPUT_LIMIT_BYTES: usize = 64 * 1024;
const TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES: usize = 64 * 1024;
const TMUX_EXPECTED_REPLY_LIMIT: usize = 512;
const TMUX_ROUTED_COMMAND_LIMIT_BYTES: usize = 1024 * 1024;
const TMUX_ROUTED_COMMAND_LIMIT_COUNT: usize = 4_096;
const TMUX_INVENTORY_LIMIT_BYTES: usize = 8 * 1024 * 1024;
const TMUX_INVENTORY_LIMIT_LINES: usize = 65_536;
const KITTY_CTRL_C_RELEASE_HANDOFF_MS: u128 = 100;
const KITTY_CTRL_C_INPUT_HANDOFF_MS: u128 = 500;
const MAX_DEFERRED_KITTY_RELEASES: usize = 64;
pub const TMUX_FLOW_CONTROL_COMMAND: &[u8] = b"refresh-client -f pause-after=1\n";
pub const TMUX_FLOW_CONTROL_VERIFY_COMMAND: &[u8] = b"display-message -p -F '#{client_flags}'\n";
pub const MAX_PENDING_TERMINAL_INPUT_BYTES: usize = 64 * 1024;

pub const DIFF_DELAY: u16 = 30;
pub const MAX_DIFF_DELAY: u16 = 300;
const MIN_ADAPTIVE_DIFF_DELAY: u16 = 8;
const MAX_ADAPTIVE_DIFF_DELAY: u16 = 60;
const ADAPTIVE_DIFF_MARGIN: u16 = 4;
const ADAPTIVE_DIFF_DECAY: u16 = 2;
const ADAPTIVE_DIFF_CLEAN_BURSTS: u8 = 3;
const LATE_CONTINUATION_WINDOW_MS: u128 = 100;
const ESC_TIMEOUT_MS: u128 = 50;
static ANSI_CSI_RE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"^\x1B\[[\x30-\x3F]*[\x20-\x2F]*[\x40-\x7E--[A-D~]]$")
        .expect("ANSI CSI pattern must be valid")
});

fn synchronize_pending_review_cursor(sr: &mut ScreenReader, view: &mut View) -> Result<()> {
    if sr.review_follows_screen_cursor() && view.review_cursor_follow_pending() {
        let old = view.review_cursor_position();
        view.follow_application_cursor();
        sr.hook_on_review_cursor_move(old, view.review_cursor_position())?;
    }
    Ok(())
}

fn prepare_review_cursor_for_active_context(sr: &mut ScreenReader, view: &mut View) -> Result<()> {
    let (old, new) = view.prepare_review_cursor_for_activation();
    if old != new {
        sr.hook_on_review_cursor_move(old, new)?;
    }
    Ok(())
}

fn consume_application_accessibility(
    sr: &mut ScreenReader,
    view: &mut View,
    allow_speech: bool,
) -> Result<ApplicationAccessibilityPolicy> {
    let policy = view.application_accessibility_policy();
    let messages = view.take_presented_application_speech();
    if policy.suppress_cursor_tracking {
        // A delete intent may have been captured before the application's
        // suppression reached the physical presentation boundary.
        sr.clear_pending_delete();
    }
    let speak = allow_speech && sr.auto_read_enabled();
    for message in messages {
        let indentation_changed = message
            .indentation
            .is_some_and(|level| view.application_semantic_indentation_changed(level));
        if speak {
            if let Some(level) = message
                .indentation
                .filter(|_| indentation_changed && sr.indentation_reporting_enabled())
            {
                sr.speak(&format!("indent {level}"), false)?;
            }
            // `speak` applies the outer-terminal focus policy. Unfocused
            // messages are consumed here and are never replayed later.
            sr.speak(&message.text, false)?;
        }
    }
    Ok(policy)
}

#[cfg(test)]
mod stabilization_tests {
    use super::{
        App, DIFF_DELAY, MIN_ADAPTIVE_DIFF_DELAY, PresentedUpdateStatus, StabilizationBurst,
        StabilizationCommitReason, StabilizationDecision, StabilizationProfile,
        stabilization_decision, stabilization_input_is_recent,
    };
    use crate::{
        output_scheduler::OutputSchedulerConfig,
        presentation::{SurfaceId, ViewId},
        terminal::ScreenIdentity,
        views::{MessageView, PtyView, ViewStack},
    };

    fn app_with_presented_update(bytes: &[u8]) -> App {
        let stack = ViewStack::new(Box::new(PtyView::new(4, 20)));
        let mut app = App::new(stack).expect("create app");
        app.enable_output_scheduler(OutputSchedulerConfig::default());
        let view = app.view_stack.root_mut().model();
        view.process_changes(bytes);
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(frame));
        app
    }

    fn burst(first_output_ms: u128, last_output_ms: u128, delay_ms: u16) -> StabilizationBurst {
        StabilizationBurst {
            first_output_ms,
            last_output_ms,
            delay_ms,
        }
    }

    #[test]
    fn application_transaction_blocks_every_commit_reason_and_deadline() {
        let update = PresentedUpdateStatus {
            explicitly_stable: true,
            completes_linear_output_record: true,
            ..PresentedUpdateStatus::default()
        };
        let decision = stabilization_decision(400, burst(100, 390, 30), update, true);
        assert_eq!(
            decision,
            StabilizationDecision::BlockedByApplicationTransaction
        );
        assert_eq!(decision.deadline_ms(400), None);
    }

    #[test]
    fn explicit_boundary_commits_even_if_an_older_prompt_start_remains_open() {
        let update = PresentedUpdateStatus {
            explicitly_stable: true,
            prompt_transaction_open: true,
            ..PresentedUpdateStatus::default()
        };
        assert_eq!(
            stabilization_decision(100, burst(100, 100, 30), update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::ExplicitlyStable)
        );
    }

    #[test]
    fn prompt_without_an_end_marker_uses_the_quiet_fallback() {
        let update = PresentedUpdateStatus {
            prompt_transaction_open: true,
            ..PresentedUpdateStatus::default()
        };
        assert_eq!(
            stabilization_decision(129, burst(100, 100, 30), update, false),
            StabilizationDecision::WaitUntil(130)
        );
        assert_eq!(
            stabilization_decision(130, burst(100, 100, 30), update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::QuietWindow)
        );
    }

    #[test]
    fn exact_commit_reasons_have_stable_precedence_over_fallback_timers() {
        let mut update = PresentedUpdateStatus {
            explicitly_stable: true,
            completes_linear_output_record: true,
            ..PresentedUpdateStatus::default()
        };
        let current_burst = burst(100, 190, 30);
        assert_eq!(
            stabilization_decision(200, current_burst, update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::ExplicitlyStable)
        );
        update.explicitly_stable = false;
        assert_eq!(
            stabilization_decision(200, current_burst, update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::LinearOutputRecord)
        );
        update.completes_linear_output_record = false;
        assert_eq!(
            stabilization_decision(200, current_burst, update, false),
            StabilizationDecision::WaitUntil(220)
        );
    }

    #[test]
    fn parser_continuation_ignores_quiet_and_commits_only_at_the_hard_boundary() {
        let update = PresentedUpdateStatus {
            parser_continuation: true,
            ..PresentedUpdateStatus::default()
        };
        assert_eq!(
            stabilization_decision(399, burst(100, 120, 8), update, false),
            StabilizationDecision::WaitUntil(400)
        );
        assert_eq!(
            stabilization_decision(400, burst(100, 120, 8), update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::HardDeadline)
        );
    }

    #[test]
    fn ordinary_quiet_and_hard_deadlines_commit_at_their_exact_boundaries() {
        let update = PresentedUpdateStatus {
            adaptive_quiet_trainable: true,
            ..PresentedUpdateStatus::default()
        };
        let quiet_burst = burst(100, 120, 30);
        assert_eq!(
            stabilization_decision(149, quiet_burst, update, false),
            StabilizationDecision::WaitUntil(150)
        );
        let quiet = stabilization_decision(150, quiet_burst, update, false);
        assert_eq!(
            quiet,
            StabilizationDecision::Commit(StabilizationCommitReason::QuietWindow)
        );
        assert!(
            StabilizationCommitReason::QuietWindow.trains_adaptive_quiet(update),
            "only an ordinary quiet commit should train"
        );

        let hard_burst = burst(100, 395, 30);
        assert_eq!(
            stabilization_decision(399, hard_burst, update, false),
            StabilizationDecision::WaitUntil(400)
        );
        assert_eq!(
            stabilization_decision(400, hard_burst, update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::HardDeadline)
        );
    }

    #[test]
    fn quiet_wins_at_a_shared_boundary_but_prompt_commits_never_train() {
        let update = PresentedUpdateStatus {
            adaptive_quiet_trainable: true,
            ..PresentedUpdateStatus::default()
        };
        assert_eq!(
            stabilization_decision(400, burst(100, 370, 30), update, false),
            StabilizationDecision::Commit(StabilizationCommitReason::QuietWindow)
        );
        assert!(!StabilizationCommitReason::ExplicitlyStable.trains_adaptive_quiet(update));
        assert!(!StabilizationCommitReason::HardDeadline.trains_adaptive_quiet(update));
        assert!(
            !StabilizationCommitReason::QuietWindow.trains_adaptive_quiet(PresentedUpdateStatus {
                prompt_transaction_open: true,
                ..update
            })
        );
        assert!(stabilization_input_is_recent(400, Some(100)));
        assert!(!stabilization_input_is_recent(401, Some(100)));
    }

    #[test]
    fn quiet_single_write_bursts_reduce_the_fallback_delay_gradually() {
        let mut profile = StabilizationProfile::default();
        let mut now = 0;
        for _ in 0..40 {
            profile.observe_output(now, None, true);
            profile.finish_burst(now + u128::from(profile.delay_ms), now, true);
            now += 1_000;
        }
        assert!(profile.delay_ms < DIFF_DELAY);
        assert!(profile.delay_ms >= MIN_ADAPTIVE_DIFF_DELAY);
    }

    #[test]
    fn a_late_continuation_raises_the_learned_quiet_window_immediately() {
        let mut profile = StabilizationProfile {
            delay_ms: MIN_ADAPTIVE_DIFF_DELAY,
            ..StabilizationProfile::default()
        };
        profile.observe_output(100, None, true);
        profile.finish_burst(108, 100, true);

        profile.observe_output(125, None, true);

        assert_eq!(profile.delay_ms, 29);
        assert_eq!(profile.learned_floor_ms, 29);
    }

    #[test]
    fn a_nontraining_late_burst_cannot_retain_a_provisional_delay_raise() {
        let mut profile = StabilizationProfile {
            delay_ms: MIN_ADAPTIVE_DIFF_DELAY,
            ..StabilizationProfile::default()
        };
        profile.observe_output(100, None, true);
        profile.finish_burst(108, 100, true);

        profile.observe_output(125, None, false);
        assert_eq!(profile.delay_ms, MIN_ADAPTIVE_DIFF_DELAY);
        profile.finish_burst(155, 125, false);

        // A burst may look ordinary at first and become structural later. Its
        // provisional protection applies to the in-flight burst only and is
        // rolled back when the complete classifier rejects training.
        profile.observe_output(1_000, None, true);
        profile.finish_burst(1_008, 1_000, true);
        profile.observe_output(1_025, None, true);
        assert!(profile.delay_ms > MIN_ADAPTIVE_DIFF_DELAY);
        profile.observe_output(1_026, None, false);
        profile.finish_burst(1_056, 1_026, false);
        assert_eq!(profile.delay_ms, MIN_ADAPTIVE_DIFF_DELAY);
        assert_eq!(profile.learned_floor_ms, MIN_ADAPTIVE_DIFF_DELAY);
    }

    #[test]
    fn mixed_bursts_remain_nontraining_in_either_batch_order() {
        fn seeded_profile() -> StabilizationProfile {
            let mut profile = StabilizationProfile {
                delay_ms: MIN_ADAPTIVE_DIFF_DELAY,
                ..StabilizationProfile::default()
            };
            profile.observe_output(100, None, true);
            profile.finish_burst(108, 100, true);
            profile
        }

        let mut nontraining_first = seeded_profile();
        nontraining_first.observe_output(125, None, false);
        nontraining_first.observe_output(126, None, true);
        nontraining_first.finish_burst(156, 126, true);
        assert_eq!(nontraining_first.delay_ms, MIN_ADAPTIVE_DIFF_DELAY);
        assert_eq!(nontraining_first.learned_floor_ms, MIN_ADAPTIVE_DIFF_DELAY);

        let mut nontraining_last = seeded_profile();
        nontraining_last.observe_output(125, None, true);
        assert!(nontraining_last.delay_ms > MIN_ADAPTIVE_DIFF_DELAY);
        nontraining_last.observe_output(126, None, false);
        // The cumulative presented summary can end in an ordinary-looking
        // state. Burst-local eligibility must still reject both the earlier
        // title/structural gap and the provisional late-continuation raise.
        nontraining_last.finish_burst(156, 126, true);
        assert_eq!(nontraining_last.delay_ms, MIN_ADAPTIVE_DIFF_DELAY);
        assert_eq!(nontraining_last.learned_floor_ms, MIN_ADAPTIVE_DIFF_DELAY);
    }

    #[test]
    fn a_new_input_does_not_misclassify_the_next_response_as_a_continuation() {
        let mut profile = StabilizationProfile {
            delay_ms: MIN_ADAPTIVE_DIFF_DELAY,
            ..StabilizationProfile::default()
        };
        profile.observe_output(100, Some(90), true);
        profile.finish_burst(108, 100, true);

        profile.observe_output(125, Some(120), true);

        assert_eq!(profile.delay_ms, MIN_ADAPTIVE_DIFF_DELAY);
    }

    #[test]
    fn presented_update_status_collects_each_accessibility_commit_hint() {
        let mut linear = app_with_presented_update(b"line\r\n");
        let status = linear.active_presented_update_status();
        assert!(status.context.is_some());
        assert!(status.finalization_pending);
        assert!(status.completes_linear_output_record);

        let mut explicit = app_with_presented_update(b"\x1b[?2026hworking\x1b[?2026l");
        let status = explicit.active_presented_update_status();
        assert!(status.finalization_pending);
        assert!(status.explicitly_stable);

        let mut prompt = app_with_presented_update(b"\x1b]133;A\x07prompt");
        let status = prompt.active_presented_update_status();
        assert!(status.finalization_pending);
        assert!(status.prompt_transaction_open);

        let mut continuation = app_with_presented_update(b"visible\x1b[");
        let status = continuation.active_presented_update_status();
        assert!(status.finalization_pending);
        assert!(status.parser_continuation);
        assert!(!status.adaptive_quiet_trainable);
    }

    #[test]
    fn presented_update_status_is_inactive_without_a_presented_active_base() {
        let stack = ViewStack::new(Box::new(PtyView::new(4, 20)));
        let mut unscheduled = App::new(stack).expect("create app");
        assert_eq!(
            unscheduled.active_presented_update_status(),
            PresentedUpdateStatus::default()
        );

        let mut overlay = app_with_presented_update(b"line\r\n");
        overlay
            .view_stack
            .push(Box::new(MessageView::new(4, 20, "notice", "foreground")));
        assert_eq!(
            overlay.active_presented_update_status(),
            PresentedUpdateStatus::default()
        );
    }

    #[test]
    fn nontraining_bursts_keep_the_legacy_quiet_window() {
        let mut profile = StabilizationProfile {
            delay_ms: MIN_ADAPTIVE_DIFF_DELAY,
            ..StabilizationProfile::default()
        };

        profile.observe_output(100, None, true);
        assert_eq!(
            profile.burst().expect("trainable burst").delay_ms,
            MIN_ADAPTIVE_DIFF_DELAY
        );
        profile.observe_output(101, None, false);
        assert_eq!(profile.burst().expect("mixed burst").delay_ms, DIFF_DELAY);
        profile.finish_burst(131, 101, false);

        profile.observe_output(200, None, false);
        assert_eq!(
            profile.burst().expect("structural burst").delay_ms,
            DIFF_DELAY
        );
    }

    #[test]
    fn stabilization_profile_owns_and_clears_its_burst_deadlines() {
        let mut profile = StabilizationProfile::default();
        profile.observe_output(100, None, true);
        profile.observe_output(125, None, true);

        let burst = profile.burst().expect("active burst");
        assert_eq!(burst.first_output_ms, 100);
        assert_eq!(burst.last_output_ms, 125);
        assert_eq!(burst.delay_ms, DIFF_DELAY);

        profile.finish_burst(155, 125, true);
        assert_eq!(profile.burst(), None);
    }

    #[test]
    fn retired_profile_churn_is_pruned_before_burst_cancellation() {
        let stack = ViewStack::new(Box::new(PtyView::new(4, 20)));
        let mut app = App::new(stack).expect("create app");
        let mut retained_capacity_ceiling = None;
        let root_id = app.view_stack.root_mut().model().view_id();
        for screen in [ScreenIdentity::Primary, ScreenIdentity::Alternate] {
            let mut profile = StabilizationProfile::default();
            profile.observe_output(1, None, true);
            app.stabilization_profiles.insert(
                super::AccessibilityContext {
                    view_id: root_id,
                    screen,
                },
                profile,
            );
        }

        for generation in 0..32_u64 {
            for offset in 0..128_u64 {
                let mut profile = StabilizationProfile::default();
                profile.observe_output(generation.saturating_add(2).into(), None, true);
                app.stabilization_profiles.insert(
                    super::AccessibilityContext {
                        view_id: ViewId(
                            u64::MAX
                                .saturating_sub(generation.saturating_mul(128))
                                .saturating_sub(offset),
                        ),
                        screen: ScreenIdentity::Primary,
                    },
                    profile,
                );
            }

            let expanded_capacity = app.stabilization_profiles.capacity();
            app.prune_retired_accessibility_views();
            assert_eq!(
                app.stabilization_profiles.len(),
                2,
                "only the two live root-screen profiles may reach the cancellation scan"
            );
            let maximum_reasonable_capacity =
                app.stabilization_profiles.len().saturating_mul(4).max(16);
            assert!(
                app.stabilization_profiles.capacity()
                    <= maximum_reasonable_capacity.saturating_mul(2),
                "profile storage retained capacity {} for only {} live profiles",
                app.stabilization_profiles.capacity(),
                app.stabilization_profiles.len()
            );
            assert!(
                app.stabilization_profiles.capacity() < expanded_capacity,
                "pruning retired profiles must release their backing storage"
            );
            let ceiling =
                *retained_capacity_ceiling.get_or_insert(app.stabilization_profiles.capacity());
            assert!(
                app.stabilization_profiles.capacity() <= ceiling,
                "repeated retired-view churn must not grow retained profile storage"
            );
        }

        app.cancel_stabilization_bursts();
        assert!(
            app.stabilization_profiles
                .values()
                .all(|profile| profile.burst().is_none())
        );
    }
}

fn speak_application_cursor_line(sr: &mut ScreenReader, view: &View) -> Result<()> {
    sr.speak_application_cursor_line(view)?;
    Ok(())
}

fn announce_screen_transition(sr: &mut ScreenReader, view: &View) -> Result<()> {
    match view.screen().screen {
        ScreenIdentity::Alternate => {
            sr.speak_application_screen(view)?;
        }
        ScreenIdentity::Primary => {
            sr.speak_application_cursor_line(view)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AccessibilityContext {
    view_id: ViewId,
    screen: ScreenIdentity,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PresentedUpdateStatus {
    context: Option<AccessibilityContext>,
    finalization_pending: bool,
    application_transaction_open: bool,
    explicitly_stable: bool,
    completes_linear_output_record: bool,
    prompt_transaction_open: bool,
    parser_continuation: bool,
    adaptive_quiet_trainable: bool,
}

fn adaptive_quiet_is_trainable(update: &UpdateSummary) -> bool {
    !update.output_report_structural
        && !update.parser_continuation
        && update.screen_before == update.screen_after
        && !update.changed_rows.is_empty()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StabilizationBurst {
    first_output_ms: u128,
    last_output_ms: u128,
    delay_ms: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StabilizationCommitReason {
    ExplicitlyStable,
    LinearOutputRecord,
    QuietWindow,
    HardDeadline,
}

impl StabilizationCommitReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitlyStable => "explicitly-stable",
            Self::LinearOutputRecord => "linear-output-record",
            Self::QuietWindow => "quiet-window",
            Self::HardDeadline => "hard-deadline",
        }
    }

    const fn trains_adaptive_quiet(self, update: PresentedUpdateStatus) -> bool {
        matches!(self, Self::QuietWindow)
            && update.adaptive_quiet_trainable
            && !update.prompt_transaction_open
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StabilizationDecision {
    BlockedByApplicationTransaction,
    WaitUntil(u128),
    Commit(StabilizationCommitReason),
}

impl StabilizationDecision {
    const fn deadline_ms(self, now_ms: u128) -> Option<u128> {
        match self {
            Self::BlockedByApplicationTransaction => None,
            Self::WaitUntil(deadline_ms) => Some(deadline_ms),
            Self::Commit(_) => Some(now_ms),
        }
    }
}

fn stabilization_input_is_recent(now_ms: u128, last_input_ms: Option<u128>) -> bool {
    last_input_ms
        .is_some_and(|input_ms| now_ms.saturating_sub(input_ms) <= u128::from(MAX_DIFF_DELAY))
}

fn stabilization_decision(
    now_ms: u128,
    burst: StabilizationBurst,
    update: PresentedUpdateStatus,
    application_transaction_open: bool,
) -> StabilizationDecision {
    if application_transaction_open {
        return StabilizationDecision::BlockedByApplicationTransaction;
    }

    let hard_deadline = burst
        .first_output_ms
        .saturating_add(u128::from(MAX_DIFF_DELAY));
    if update.explicitly_stable {
        return StabilizationDecision::Commit(StabilizationCommitReason::ExplicitlyStable);
    }
    if update.completes_linear_output_record {
        return StabilizationDecision::Commit(StabilizationCommitReason::LinearOutputRecord);
    }
    if update.parser_continuation {
        return if now_ms >= hard_deadline {
            StabilizationDecision::Commit(StabilizationCommitReason::HardDeadline)
        } else {
            StabilizationDecision::WaitUntil(hard_deadline)
        };
    }

    let quiet_deadline = burst
        .last_output_ms
        .saturating_add(u128::from(burst.delay_ms));
    if now_ms >= quiet_deadline {
        StabilizationDecision::Commit(StabilizationCommitReason::QuietWindow)
    } else if now_ms >= hard_deadline {
        StabilizationDecision::Commit(StabilizationCommitReason::HardDeadline)
    } else {
        StabilizationDecision::WaitUntil(quiet_deadline.min(hard_deadline))
    }
}

#[derive(Clone, Debug)]
struct StabilizationProfile {
    delay_ms: u16,
    learned_floor_ms: u16,
    maximum_gap_ms: u16,
    clean_bursts: u8,
    burst_adaptive_quiet_trainable: bool,
    late_raise_checkpoint: Option<(u16, u16)>,
    burst_first_output_ms: Option<u128>,
    burst_last_output_ms: Option<u128>,
    last_ordinary_finalized_ms: Option<u128>,
    last_ordinary_output_ms: Option<u128>,
}

impl Default for StabilizationProfile {
    fn default() -> Self {
        Self {
            delay_ms: DIFF_DELAY,
            learned_floor_ms: MIN_ADAPTIVE_DIFF_DELAY,
            maximum_gap_ms: 0,
            clean_bursts: 0,
            burst_adaptive_quiet_trainable: true,
            late_raise_checkpoint: None,
            burst_first_output_ms: None,
            burst_last_output_ms: None,
            last_ordinary_finalized_ms: None,
            last_ordinary_output_ms: None,
        }
    }
}

impl StabilizationProfile {
    fn observe_output(
        &mut self,
        now_ms: u128,
        last_input_ms: Option<u128>,
        adaptive_quiet_trainable: bool,
    ) {
        if self.burst_first_output_ms.is_none() {
            self.burst_adaptive_quiet_trainable = adaptive_quiet_trainable;
        } else {
            self.burst_adaptive_quiet_trainable &= adaptive_quiet_trainable;
        }
        if let (Some(finalized), Some(previous_output)) = (
            self.last_ordinary_finalized_ms,
            self.last_ordinary_output_ms,
        ) && now_ms > finalized
            && now_ms.saturating_sub(finalized) <= LATE_CONTINUATION_WINDOW_MS
            && last_input_ms.is_none_or(|input| input < finalized)
            && self.burst_adaptive_quiet_trainable
        {
            let late_gap = now_ms.saturating_sub(previous_output);
            let raised = late_gap
                .saturating_add(u128::from(ADAPTIVE_DIFF_MARGIN))
                .min(u128::from(MAX_ADAPTIVE_DIFF_DELAY)) as u16;
            self.late_raise_checkpoint
                .get_or_insert((self.delay_ms, self.learned_floor_ms));
            self.delay_ms = self.delay_ms.max(raised);
            self.learned_floor_ms = self.learned_floor_ms.max(raised);
            self.clean_bursts = 0;
            self.last_ordinary_finalized_ms = None;
            self.last_ordinary_output_ms = None;
        }

        if let Some(previous) = self.burst_last_output_ms {
            let gap = now_ms
                .saturating_sub(previous)
                .min(u128::from(MAX_ADAPTIVE_DIFF_DELAY)) as u16;
            self.maximum_gap_ms = self.maximum_gap_ms.max(gap);
        }
        self.burst_first_output_ms.get_or_insert(now_ms);
        self.burst_last_output_ms = Some(now_ms);
    }

    fn finish_burst(&mut self, now_ms: u128, last_output_ms: u128, ordinary: bool) {
        if ordinary && self.burst_adaptive_quiet_trainable {
            let observed_target = self
                .maximum_gap_ms
                .saturating_add(ADAPTIVE_DIFF_MARGIN)
                .clamp(MIN_ADAPTIVE_DIFF_DELAY, MAX_ADAPTIVE_DIFF_DELAY);
            self.learned_floor_ms = self.learned_floor_ms.max(observed_target);
            self.clean_bursts = self.clean_bursts.saturating_add(1);
            if self.clean_bursts >= ADAPTIVE_DIFF_CLEAN_BURSTS {
                if observed_target < self.learned_floor_ms {
                    self.learned_floor_ms =
                        self.learned_floor_ms.saturating_sub(1).max(observed_target);
                }
                self.delay_ms = self
                    .delay_ms
                    .saturating_sub(ADAPTIVE_DIFF_DECAY)
                    .max(self.learned_floor_ms);
                self.clean_bursts = 0;
            }
            self.last_ordinary_finalized_ms = Some(now_ms);
            self.last_ordinary_output_ms = Some(last_output_ms);
            self.late_raise_checkpoint = None;
        } else {
            if let Some((delay_ms, learned_floor_ms)) = self.late_raise_checkpoint.take() {
                self.delay_ms = delay_ms;
                self.learned_floor_ms = learned_floor_ms;
            }
            self.clean_bursts = 0;
            self.last_ordinary_finalized_ms = None;
            self.last_ordinary_output_ms = None;
        }
        self.maximum_gap_ms = 0;
        self.burst_adaptive_quiet_trainable = true;
        self.burst_first_output_ms = None;
        self.burst_last_output_ms = None;
    }

    fn burst(&self) -> Option<StabilizationBurst> {
        let last_output_ms = self.burst_last_output_ms?;
        Some(StabilizationBurst {
            first_output_ms: self.burst_first_output_ms.unwrap_or(last_output_ms),
            last_output_ms,
            delay_ms: if self.burst_adaptive_quiet_trainable {
                self.delay_ms
            } else {
                self.delay_ms.max(DIFF_DELAY)
            },
        })
    }

    fn rebase_burst_deadline(&mut self, now_ms: u128) {
        self.burst_first_output_ms = Some(now_ms);
        self.burst_last_output_ms = Some(now_ms);
    }

    fn cancel_burst_context(&mut self) {
        if let Some((delay_ms, learned_floor_ms)) = self.late_raise_checkpoint.take() {
            self.delay_ms = delay_ms;
            self.learned_floor_ms = learned_floor_ms;
        }
        self.maximum_gap_ms = 0;
        self.clean_bursts = 0;
        self.burst_adaptive_quiet_trainable = true;
        self.burst_first_output_ms = None;
        self.burst_last_output_ms = None;
        self.last_ordinary_finalized_ms = None;
        self.last_ordinary_output_ms = None;
    }

    fn reset_context(&mut self) {
        *self = Self::default();
    }
}

fn format_terminal_accessibility_label(title: &str) -> String {
    if title.is_empty() {
        "terminal".to_owned()
    } else {
        format!("terminal, {title}")
    }
}

pub trait Clock {
    fn now_ms(&self) -> u128;
}

/// Stable tmux topology metadata for the most recently presented pane bell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxBellSource {
    pub connection_id: u64,
    pub connection_label: String,
    pub session_id: crate::tmux_model::SessionId,
    pub session_name: String,
    pub window_id: crate::tmux_model::WindowId,
    pub window_name: String,
    pub pane_id: crate::tmux_model::PaneId,
    pub pane_title: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TmuxFlowStatus {
    #[default]
    Running,
    Resynchronizing,
    ResyncFailed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TmuxResyncLimitation {
    KittyImages,
    ParserContinuation,
    SemanticMetadata,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TmuxPaneFlowState {
    pub status: TmuxFlowStatus,
    pub is_paused: bool,
    pub pause_requested: bool,
    pub resume_requested: bool,
    pub resync_requested: bool,
    pub resync_in_flight: bool,
    pub final_resync_requested: bool,
    pub resync_after_ms: Option<u128>,
    pub recapture_hard_deadline_ms: Option<u128>,
    pub last_extended_output_age_ms: Option<u64>,
    pub skipped_incremental_bytes: usize,
    pub resync_count: u64,
    pub resync_failures: u64,
    pub consecutive_resync_failures: u32,
    pub resync_failure_announced: bool,
    pub limitations: BTreeSet<TmuxResyncLimitation>,
}

#[derive(Default)]
struct TmuxGatewayOutputCoalescer {
    pending_ordinary_output: Option<crate::tmux_gateway::GatewayEvent>,
}

impl TmuxGatewayOutputCoalescer {
    /// Combines adjacent same-pane records which the gateway parser produced
    /// from one transport read. The tail is always flushed before
    /// `handle_pty` returns, so terminal mutation and replies never cross a
    /// read boundary.
    fn route_gateway_event(
        &mut self,
        event: crate::tmux_gateway::GatewayEvent,
    ) -> (
        Option<crate::tmux_gateway::GatewayEvent>,
        Option<crate::tmux_gateway::GatewayEvent>,
    ) {
        if let Some((key, incoming_len)) = ordinary_tmux_output_metadata(&event) {
            if incoming_len > TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES {
                let pending = self.pending_ordinary_output.take();
                return if pending.is_some() {
                    (pending, Some(event))
                } else {
                    (Some(event), None)
                };
            }
            if let Some(pending) = &mut self.pending_ordinary_output {
                let (pending_key, pending_len) = ordinary_tmux_output_metadata(pending)
                    .expect("only ordinary tmux output is retained for coalescing");
                if pending_key == key
                    && pending_len.saturating_add(incoming_len)
                        <= TMUX_PANE_OUTPUT_COALESCE_LIMIT_BYTES
                {
                    append_ordinary_tmux_output(pending, event);
                    return (None, None);
                }
                let ready = self.pending_ordinary_output.replace(event);
                return (ready, None);
            }
            self.pending_ordinary_output = Some(event);
            return (None, None);
        }

        let pending = self.pending_ordinary_output.take();
        if pending.is_some() {
            (pending, Some(event))
        } else {
            (Some(event), None)
        }
    }

    fn finish(&mut self) -> Option<crate::tmux_gateway::GatewayEvent> {
        self.pending_ordinary_output.take()
    }
}

struct PendingPanePresentation {
    connection_id: u64,
    pane_id: crate::tmux_model::PaneId,
    update: UpdateSummary,
}

/// Presentation-only state for one bounded PTY drain. The direct-root and
/// first-pane paths retain their already-owned update summary without a map or
/// auxiliary heap allocation. Multiple pane sources use one insertion-ordered
/// vector which the renderer maps to surfaces in a linear pass.
#[derive(Default)]
struct PendingPresentationBatch {
    root_update: Option<UpdateSummary>,
    first_pane_update: Option<PendingPanePresentation>,
    additional_pane_updates: Vec<PendingPanePresentation>,
    bell_count: usize,
    authoritative_scene_required: bool,
}

impl PendingPresentationBatch {
    fn has_scene_work(&self) -> bool {
        self.authoritative_scene_required
            || self.root_update.is_some()
            || self.first_pane_update.is_some()
            || !self.additional_pane_updates.is_empty()
            || self.bell_count != 0
    }

    fn push_root(&mut self, update: UpdateSummary, bell_count: usize) {
        self.bell_count = self.bell_count.saturating_add(bell_count);
        if self.authoritative_scene_required {
            return;
        }
        if let Some(pending) = &mut self.root_update {
            pending.merge(update);
        } else {
            self.root_update = Some(update);
        }
    }

    fn push_pane(
        &mut self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        update: UpdateSummary,
        bell_count: usize,
    ) {
        self.bell_count = self.bell_count.saturating_add(bell_count);
        if self.authoritative_scene_required {
            return;
        }
        if let Some(pending) = &mut self.first_pane_update {
            if pending.connection_id == connection_id && pending.pane_id == pane_id {
                pending.update.merge(update);
                return;
            }
        } else {
            self.first_pane_update = Some(PendingPanePresentation {
                connection_id,
                pane_id,
                update,
            });
            return;
        }
        if let Some(pending) = self
            .additional_pane_updates
            .iter_mut()
            .find(|pending| pending.connection_id == connection_id && pending.pane_id == pane_id)
        {
            pending.update.merge(update);
        } else {
            self.additional_pane_updates.push(PendingPanePresentation {
                connection_id,
                pane_id,
                update,
            });
        }
    }

    fn require_authoritative_scene(&mut self, bell_count: usize) {
        self.bell_count = self.bell_count.saturating_add(bell_count);
        self.authoritative_scene_required = true;
        self.root_update = None;
        self.first_pane_update = None;
        self.additional_pane_updates.clear();
    }

    fn pane_updates(self) -> impl Iterator<Item = PendingPanePresentation> {
        self.first_pane_update
            .into_iter()
            .chain(self.additional_pane_updates)
    }
}

fn ordinary_tmux_output_metadata(
    event: &crate::tmux_gateway::GatewayEvent,
) -> Option<((u64, u64), usize)> {
    match event {
        crate::tmux_gateway::GatewayEvent::Control {
            connection_id,
            event: crate::tmux_control::ControlEvent::Output { pane_id, bytes },
        } => Some(((*connection_id, *pane_id), bytes.len())),
        _ => None,
    }
}

fn append_ordinary_tmux_output(
    pending: &mut crate::tmux_gateway::GatewayEvent,
    incoming: crate::tmux_gateway::GatewayEvent,
) {
    let (
        crate::tmux_gateway::GatewayEvent::Control {
            connection_id: pending_connection,
            event:
                crate::tmux_control::ControlEvent::Output {
                    pane_id: pending_pane,
                    bytes: pending_bytes,
                },
        },
        crate::tmux_gateway::GatewayEvent::Control {
            connection_id: incoming_connection,
            event:
                crate::tmux_control::ControlEvent::Output {
                    pane_id: incoming_pane,
                    bytes: incoming_bytes,
                },
        },
    ) = (pending, incoming)
    else {
        unreachable!("only matching ordinary tmux output events are coalesced");
    };
    debug_assert_eq!(*pending_connection, incoming_connection);
    debug_assert_eq!(*pending_pane, incoming_pane);
    pending_bytes.extend(incoming_bytes);
}

pub struct StdClock {
    start: time::Instant,
}

impl Default for StdClock {
    fn default() -> Self {
        Self::new()
    }
}

impl StdClock {
    pub fn new() -> Self {
        Self {
            start: time::Instant::now(),
        }
    }
}

impl Clock for StdClock {
    fn now_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }
}

pub struct App {
    view_stack: views::ViewStack,
    pending_input: VecDeque<u8>,
    pending_input_last_at: Option<u128>,
    application_replies: ApplicationReplyBroker<SurfaceId>,
    terminal_effect_policy: TerminalEffectPolicy,
    popup_responses: VecDeque<views::PopupResponse>,
    consumed_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    view_transition_key_presses: HashSet<(KeyCode, KeyModifiers, KeyEventState)>,
    forwarded_key_presses: HashMap<(KeyCode, KeyModifiers, KeyEventState), ForwardedKeyPress>,
    deferred_kitty_releases: VecDeque<DeferredKittyRelease>,
    kitty_ctrl_c_input_handoff: Option<KittyInputHandoff>,
    log_enabled: bool,
    lua_repl_session: Option<views::LuaReplSession>,
    last_stdin_update: Option<u128>,
    stabilization_profiles: HashMap<AccessibilityContext, StabilizationProfile>,
    scene_renderer: IncrementalVtRenderer,
    presented_scene: PresentedScene,
    presented_accessibility_view: Option<ViewId>,
    presented_accessibility_label: Option<String>,
    presented_accessibility_label_tracks_terminal_title: bool,
    pending_view_announcement: bool,
    pending_active_view_read: Option<ViewId>,
    physical_profile: PhysicalTerminalProfile,
    startup_probe_broker: Option<StartupProbeBroker>,
    output_scheduler: Option<OutputScheduler>,
    /// A DEC 2026 watchdog has abandoned the application's atomicity promise,
    /// but accessibility has not yet rebased ordinary stabilization on the
    /// exact fail-open render receipt.
    pending_synchronization_timeout_stabilization: bool,
    /// Exact scheduler bypass generation which owns a still-pending logical
    /// compositor transition. A receipt for an older generation must not
    /// clear a newer overlay/session handoff.
    compositor_transition_bypass_owner: Option<(u64, views::CompositorTransitionToken)>,
    /// A transition render rejected only because older physical bytes still
    /// consume the scheduler budget. Retried once after those bytes drain.
    compositor_transition_retry: Option<views::CompositorTransitionToken>,
    /// Marks the one retry while it is being enqueued so another capacity
    /// rejection cannot re-arm itself into a busy loop.
    compositor_transition_retry_attempt: Option<views::CompositorTransitionToken>,
    physical_lifecycle: PhysicalTerminalLifecycle,
    tmux_gateway: TmuxGatewayRouter,
    tmux_termination_deadline_ms: Option<u128>,
    nested_tmux_gateways: BTreeMap<(u64, u64), NestedTmuxGatewayState>,
    tmux_hierarchy: ConnectionHierarchy,
    tmux_connections: Vec<TmuxConnectionState>,
    pending_tmux_commands: VecDeque<PendingTmuxCommand>,
    active_tmux_connection: Option<u64>,
    pending_tmux_confirmation: Option<PendingTmuxConfirmation>,
    pending_gateway_confirmation: Option<PendingGatewayConfirmation>,
    pending_direct_gateway_input: VecDeque<PendingDirectGatewayInput>,
    pending_graceful_teardown: Option<PendingGracefulTeardown>,
    pending_force_abandon: Option<PendingForceAbandon>,
    last_tmux_bell_source: Option<TmuxBellSource>,
    recent_tmux_bells: BTreeMap<(u64, crate::tmux_model::PaneId), u128>,
    tmux_background_bell_windows: BTreeSet<(u64, crate::tmux_model::WindowId)>,
    pending_tmux_background_output: BTreeMap<(u64, crate::tmux_model::PaneId), VecDeque<u8>>,
    pending_tmux_background_order: VecDeque<(u64, crate::tmux_model::PaneId)>,
    pending_tmux_background_bytes: usize,
    tmux_hidden_output_bytes_this_turn: usize,
    pending_presentation_batch: Option<PendingPresentationBatch>,
    clock: Box<dyn Clock>,
}

struct NestedTmuxGatewayState {
    router: TmuxGatewayRouter,
    active_local_connection_id: Option<u64>,
    active_global_connection_id: Option<u64>,
    termination_deadline_ms: Option<u128>,
}

impl NestedTmuxGatewayState {
    fn new() -> Self {
        Self {
            router: TmuxGatewayRouter::new(),
            active_local_connection_id: None,
            active_global_connection_id: None,
            termination_deadline_ms: None,
        }
    }
}

struct TmuxConnectionState {
    id: u64,
    topology: TmuxTopology,
    initial_command_seen: bool,
    inventory_replies_remaining: usize,
    pending_inventory: Vec<Vec<u8>>,
    pending_inventory_bytes: usize,
    pending_inventory_lines: usize,
    inventory_failed: bool,
    inventory_failure_detail: Option<String>,
    expected_replies: VecDeque<ExpectedTmuxReply>,
    has_inventory: bool,
    inventory_retry_count: u8,
    command_history: Vec<String>,
    prefix_state: Option<TmuxPrefixState>,
    key_table_override: Option<String>,
    pane_flow: BTreeMap<crate::tmux_model::PaneId, TmuxPaneFlowState>,
    pending_pane_captures: BTreeMap<crate::tmux_model::PaneId, PendingTmuxPaneCapture>,
    flow_control_policy_accepted: Option<bool>,
    flow_control_verified: Option<bool>,
    flow_control_warning_announced: bool,
    capture_line_flags_supported: Option<bool>,
    last_announced_location: Option<crate::tmux_model::TmuxLocation>,
    preferred_location: Option<crate::tmux_model::TmuxLocation>,
}

struct PendingTmuxPaneCapture {
    metadata: crate::tmux_model::PaneCaptureMetadata,
    output: Option<Vec<Vec<u8>>>,
    pending_escape: Vec<u8>,
    line_flags: bool,
    parser_continuation_available: bool,
    failed: bool,
}

impl TmuxConnectionState {
    fn append_inventory(&mut self, output: Vec<Vec<u8>>) {
        if self.inventory_failed {
            return;
        }
        let added_bytes = output.iter().map(Vec::len).sum::<usize>();
        let next_bytes = self.pending_inventory_bytes.checked_add(added_bytes);
        let next_lines = self.pending_inventory_lines.checked_add(output.len());
        if next_bytes.is_none_or(|bytes| bytes > TMUX_INVENTORY_LIMIT_BYTES)
            || next_lines.is_none_or(|lines| lines > TMUX_INVENTORY_LIMIT_LINES)
        {
            self.inventory_failed = true;
            self.inventory_failure_detail =
                Some("tmux inventory exceeded Lector's bounded transaction limits".to_owned());
            self.clear_inventory();
            return;
        }
        self.pending_inventory_bytes = next_bytes.expect("inventory byte total checked");
        self.pending_inventory_lines = next_lines.expect("inventory line total checked");
        self.pending_inventory.extend(output);
    }

    fn take_inventory(&mut self) -> Vec<Vec<u8>> {
        self.pending_inventory_bytes = 0;
        self.pending_inventory_lines = 0;
        std::mem::take(&mut self.pending_inventory)
    }

    fn clear_inventory(&mut self) {
        self.pending_inventory.clear();
        self.pending_inventory_bytes = 0;
        self.pending_inventory_lines = 0;
    }

    fn record_inventory_error(&mut self, output: &[Vec<u8>]) {
        self.inventory_failed = true;
        self.clear_inventory();
        if self.inventory_failure_detail.is_some() {
            return;
        }
        let detail = output
            .iter()
            .take(8)
            .map(|line| String::from_utf8_lossy(line))
            .collect::<Vec<_>>()
            .join("\n");
        self.inventory_failure_detail = Some(if detail.is_empty() {
            "tmux rejected an inventory command without an error message".to_owned()
        } else {
            detail.chars().take(4_096).collect()
        });
    }

    fn reset_inventory_attempt(&mut self) {
        self.clear_inventory();
        self.inventory_failed = false;
        self.inventory_failure_detail = None;
    }
}

#[derive(Clone)]
enum ExpectedTmuxReply {
    Inventory,
    FlowControlPolicy,
    FlowControlVerification,
    Bootstrap {
        pane_id: crate::tmux_model::PaneId,
        line_flags: bool,
    },
    PaneResyncProbe(crate::tmux_model::PaneId),
    PaneResyncCapture(crate::tmux_model::PaneId),
    PaneResyncPendingEscape(crate::tmux_model::PaneId),
    PaneResyncVerify(crate::tmux_model::PaneId),
    PaneResyncContinue(crate::tmux_model::PaneId),
    PanePause(crate::tmux_model::PaneId),
    PaneContinue(crate::tmux_model::PaneId),
    Ignored,
    UserCommand {
        description: String,
        show_success: bool,
    },
}

struct PendingTmuxCommand {
    connection_id: u64,
    bytes: Vec<u8>,
    expected_replies: Vec<ExpectedTmuxReply>,
    kind: PendingTmuxCommandKind,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PendingTmuxCommandKind {
    Ordinary,
    Resize,
    Input(crate::tmux_model::PaneId),
    ColorReport(crate::tmux_model::PaneId),
}

struct TmuxPrefixState {
    phase: TmuxPrefixPhase,
}

#[derive(Clone, Eq, PartialEq)]
enum TmuxPrefixPhase {
    Awaiting { table: String },
    Repeating { table: String, expires_at_ms: u128 },
}

struct PendingTmuxConfirmation {
    connection_id: u64,
    command: String,
    target: TmuxConfirmationTarget,
}

struct PendingGatewayConfirmation {
    connection_id: u64,
    action: crate::tmux_lifecycle::GatewayControlAction,
}

struct PendingDirectGatewayInput {
    connection_id: u64,
    bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForwardedInputTarget {
    RootPty,
    TmuxPane {
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    },
}

#[derive(Clone, Copy)]
struct ForwardedKeyPress {
    target: ForwardedInputTarget,
    kitty_keyboard_flags: u8,
}

struct DeferredKittyRelease {
    target: ForwardedInputTarget,
    kitty_keyboard_flags: u8,
    bytes: Vec<u8>,
    release_at_ms: u128,
}

#[derive(Clone, Copy)]
struct KittyInputHandoff {
    target: ForwardedInputTarget,
    deadline_ms: u128,
}

struct PendingGracefulTeardown {
    remaining: VecDeque<u64>,
    awaiting: Option<u64>,
    awaiting_deadline_ms: Option<u128>,
    mode: GracefulTeardownMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GracefulTeardownMode {
    Interactive,
    Shutdown,
}

struct PendingForceAbandon {
    connection_id: u64,
    deadline_ms: u128,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TmuxConfirmationTarget {
    Pane(crate::tmux_model::PaneId),
    Window(crate::tmux_model::WindowId),
}

impl App {
    pub fn new(view_stack: views::ViewStack) -> Result<Self> {
        Self::new_with_clock(view_stack, Box::new(StdClock::new()))
    }

    pub fn new_with_clock(mut view_stack: views::ViewStack, clock: Box<dyn Clock>) -> Result<Self> {
        let geometry = view_stack.root_mut().model().live_screen().geometry;
        let physical_profile = PhysicalTerminalProfile::conservative(geometry);
        let mut app = Self {
            view_stack,
            pending_input: VecDeque::new(),
            pending_input_last_at: None,
            application_replies: ApplicationReplyBroker::default(),
            terminal_effect_policy: TerminalEffectPolicy::secure_default(),
            popup_responses: VecDeque::new(),
            consumed_key_presses: HashSet::new(),
            view_transition_key_presses: HashSet::new(),
            forwarded_key_presses: HashMap::new(),
            deferred_kitty_releases: VecDeque::new(),
            kitty_ctrl_c_input_handoff: None,
            log_enabled: false,
            lua_repl_session: None,
            last_stdin_update: None,
            stabilization_profiles: HashMap::new(),
            scene_renderer: IncrementalVtRenderer::new(RenderCapabilities {
                synchronized_output: false,
                hyperlinks: physical_profile.hyperlinks,
                kitty_graphics: physical_profile.kitty_graphics,
                inline_terminal_effects: true,
            }),
            presented_scene: PresentedScene::blank(geometry),
            presented_accessibility_view: None,
            presented_accessibility_label: None,
            presented_accessibility_label_tracks_terminal_title: false,
            pending_view_announcement: false,
            pending_active_view_read: None,
            physical_profile,
            startup_probe_broker: None,
            output_scheduler: None,
            pending_synchronization_timeout_stabilization: false,
            compositor_transition_bypass_owner: None,
            compositor_transition_retry: None,
            compositor_transition_retry_attempt: None,
            physical_lifecycle: PhysicalTerminalLifecycle::new(None),
            tmux_gateway: TmuxGatewayRouter::new(),
            tmux_termination_deadline_ms: None,
            nested_tmux_gateways: BTreeMap::new(),
            tmux_hierarchy: ConnectionHierarchy::new(),
            tmux_connections: Vec::new(),
            pending_tmux_commands: VecDeque::new(),
            active_tmux_connection: None,
            pending_tmux_confirmation: None,
            pending_gateway_confirmation: None,
            pending_direct_gateway_input: VecDeque::new(),
            pending_graceful_teardown: None,
            pending_force_abandon: None,
            last_tmux_bell_source: None,
            recent_tmux_bells: BTreeMap::new(),
            tmux_background_bell_windows: BTreeSet::new(),
            pending_tmux_background_output: BTreeMap::new(),
            pending_tmux_background_order: VecDeque::new(),
            pending_tmux_background_bytes: 0,
            tmux_hidden_output_bytes_this_turn: 0,
            pending_presentation_batch: None,
            clock,
        };
        let now_ms = app.clock.now_ms();
        app.view_stack
            .active_mut()
            .model()
            .set_previous_screen_time(now_ms);
        Ok(app)
    }

    pub fn set_logging(&mut self, enabled: bool) {
        self.log_enabled = enabled;
    }

    /// Returns enough stable topology state to find the last presented bell source.
    #[must_use]
    pub fn last_tmux_bell_source(&self) -> Option<&TmuxBellSource> {
        self.last_tmux_bell_source.as_ref()
    }

    #[must_use]
    pub fn debug_tmux_pane_flow_state(
        &self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<&TmuxPaneFlowState> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)?
            .pane_flow
            .get(&crate::tmux_model::PaneId(pane_id))
    }

    #[must_use]
    pub fn debug_tmux_background_pending_bytes(&self) -> usize {
        self.pending_tmux_background_bytes
    }

    pub fn debug_tmux_resource_usage(
        &mut self,
        connection_id: u64,
    ) -> Option<crate::tmux_panes::TmuxResourceUsage> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .resource_usage()
            .ok()
    }

    #[must_use]
    pub fn debug_scheduled_output_pending_bytes(&self) -> usize {
        self.output_scheduler
            .as_ref()
            .map_or(0, OutputScheduler::pending_bytes)
    }

    #[must_use]
    pub const fn debug_compositor_transition_retry_pending(&self) -> bool {
        self.compositor_transition_retry.is_some()
    }

    #[must_use]
    pub fn debug_tmux_pending_command_bytes(&self) -> usize {
        self.pending_tmux_commands
            .iter()
            .map(|command| command.bytes.len())
            .sum()
    }

    #[must_use]
    pub fn debug_pending_terminal_input_bytes(&self) -> usize {
        self.pending_input.len()
    }

    #[must_use]
    pub fn debug_tmux_expected_reply_count(&self, connection_id: u64) -> Option<usize> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| connection.expected_replies.len())
    }

    fn tmux_bell_source(
        &self,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
    ) -> Option<TmuxBellSource> {
        let topology = &self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)?
            .topology;
        let session_id = topology.attached_session()?;
        let session = topology.session(session_id)?;
        let pane = topology.pane(pane_id)?;
        if !session
            .windows
            .values()
            .any(|window_id| *window_id == pane.window_id)
        {
            // A control client does not normally receive this stream. Keep the
            // boundary explicit if a synthetic or future source supplies it.
            return None;
        }
        let window = topology.window(pane.window_id)?;
        Some(TmuxBellSource {
            connection_id,
            connection_label: topology.label().to_owned(),
            session_id,
            session_name: session.name.clone(),
            window_id: window.id,
            window_name: window.name.clone(),
            pane_id,
            pane_title: pane.title.clone(),
        })
    }

    fn tmux_bell_concise_announcement(&self, source: &TmuxBellSource) -> Option<String> {
        let topology = &self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == source.connection_id)?
            .topology;
        let session = topology.session(source.session_id)?;
        let window_index = session
            .windows
            .iter()
            .find_map(|(index, window_id)| (*window_id == source.window_id).then_some(*index))?;
        let pane = topology.pane(source.pane_id)?;
        let pane_count = topology
            .panes()
            .values()
            .filter(|candidate| candidate.window_id == source.window_id)
            .count();
        Some(if pane_count > 1 {
            format!("bell in pane {window_index}.{}", pane.index)
        } else {
            format!("bell in window {window_index}")
        })
    }

    fn present_tmux_bell(
        &mut self,
        sr: &mut ScreenReader,
        connection_id: u64,
        pane_id: crate::tmux_model::PaneId,
        pane_is_visible: bool,
        term_out: &mut dyn Write,
    ) -> Result<usize> {
        let mode = sr.tmux_bell_mode();
        if mode == TmuxBellMode::Off {
            return Ok(0);
        }
        let Some(source) = self.tmux_bell_source(connection_id, pane_id) else {
            return Ok(0);
        };
        let now_ms = self.clock.now_ms();
        let key = (connection_id, pane_id);
        if self
            .recent_tmux_bells
            .get(&key)
            .is_some_and(|previous| now_ms.saturating_sub(*previous) < TMUX_BELL_COALESCE_MS)
        {
            return Ok(0);
        }
        let window_is_background = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .and_then(|connection| connection.topology.session(source.session_id))
            .is_some_and(|session| session.active_window != Some(source.window_id));
        if window_is_background
            && !self
                .tmux_background_bell_windows
                .insert((connection_id, source.window_id))
        {
            return Ok(0);
        }
        self.recent_tmux_bells.insert(key, now_ms);
        self.last_tmux_bell_source = Some(source.clone());
        match mode {
            TmuxBellMode::Off => Ok(0),
            TmuxBellMode::Spoken => {
                sr.speak(
                    &format!(
                        "bell in tmux connection {} {}, session {} {}, window {} {}, pane {} {}",
                        source.connection_id,
                        source.connection_label,
                        source.session_id.0,
                        source.session_name,
                        source.window_id.0,
                        source.window_name,
                        source.pane_id.0,
                        source.pane_title,
                    ),
                    false,
                )?;
                Ok(0)
            }
            TmuxBellMode::Audible => {
                if window_is_background
                    && let Some(announcement) = self.tmux_bell_concise_announcement(&source)
                {
                    sr.speak(&announcement, false)?;
                }
                if pane_is_visible {
                    Ok(1)
                } else {
                    self.emit_physical_bells(term_out, 1)?;
                    Ok(0)
                }
            }
        }
    }

    pub fn set_physical_profile(&mut self, profile: PhysicalTerminalProfile) {
        let colors = profile.virtual_terminal_colors();
        if colors != self.physical_profile.virtual_terminal_colors()
            && let Some(colors) = colors
        {
            self.view_stack.set_virtual_terminal_colors(colors);
        }
        let capabilities = RenderCapabilities {
            synchronized_output: self.output_scheduler.is_none() && profile.synchronized_output,
            hyperlinks: profile.hyperlinks,
            kitty_graphics: profile.kitty_graphics,
            inline_terminal_effects: self.output_scheduler.is_none(),
        };
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.set_synchronized_output_supported(profile.synchronized_output);
        }
        self.scene_renderer.set_capabilities(capabilities);
        self.physical_profile = profile;
    }

    pub fn enable_output_scheduler(&mut self, config: OutputSchedulerConfig) {
        self.scene_renderer.set_capabilities(RenderCapabilities {
            synchronized_output: false,
            hyperlinks: self.physical_profile.hyperlinks,
            kitty_graphics: self.physical_profile.kitty_graphics,
            inline_terminal_effects: false,
        });
        self.output_scheduler = Some(OutputScheduler::new(
            config,
            self.physical_profile.synchronized_output,
        ));
        self.pending_synchronization_timeout_stabilization = false;
        self.compositor_transition_bypass_owner = None;
        self.compositor_transition_retry = None;
        self.compositor_transition_retry_attempt = None;
        self.view_stack.enable_presentation_tracking();
        self.presented_accessibility_view = Some(self.view_stack.logical_active_view_id());
        let (label, tracks_terminal_title) = self.view_stack.active_accessibility_label(false);
        self.presented_accessibility_label = Some(label);
        self.presented_accessibility_label_tracks_terminal_title = tracks_terminal_title;
    }

    fn presented_accessibility_model_mut(&mut self) -> &mut View {
        if let Some(view_id) = self.presented_accessibility_view
            && self.view_stack.contains_view_id(view_id)
        {
            return self
                .view_stack
                .model_by_id_mut(view_id)
                .expect("a checked presented accessibility view must remain addressable");
        }
        self.view_stack.active_mut().model()
    }

    fn active_presented_update_status(&mut self) -> PresentedUpdateStatus {
        if self.output_scheduler.is_none() || self.view_stack.has_overlay() {
            return PresentedUpdateStatus::default();
        }
        let logical_view = self.view_stack.logical_active_view_id();
        if self.presented_accessibility_view != Some(logical_view) {
            return PresentedUpdateStatus::default();
        }
        let Some(view) = self.view_stack.model_by_id_mut(logical_view) else {
            return PresentedUpdateStatus::default();
        };
        if !view.accessibility_has_unfinalized_presentation() {
            return PresentedUpdateStatus::default();
        }
        let update = view.accessibility_update_summary();
        let parser_continuation = update.parser_continuation;
        let adaptive_quiet_trainable = adaptive_quiet_is_trainable(update);
        PresentedUpdateStatus {
            context: Some(AccessibilityContext {
                view_id: logical_view,
                screen: view.screen().screen,
            }),
            finalization_pending: true,
            application_transaction_open: view.screen().modes.synchronized_output,
            explicitly_stable: view.accessibility_presentation_explicitly_stable(),
            completes_linear_output_record: view.accessibility_completes_linear_output_record(),
            prompt_transaction_open: view.accessibility_prompt_transaction_open(),
            parser_continuation,
            adaptive_quiet_trainable,
        }
    }

    /// A live DEC 2026 mode blocks accessibility only while the compositor is
    /// still honoring that transaction. Once the shared watchdog abandons the
    /// epoch, review and speech must not be frozen by the stale parser mode.
    fn application_transaction_blocks_stabilization(&self, transaction_open: bool) -> bool {
        transaction_open
            && self
                .output_scheduler
                .as_ref()
                .is_none_or(|scheduler| !scheduler.application_synchronization_is_ignored())
    }

    fn note_pty_update(
        &mut self,
        context: AccessibilityContext,
        now_ms: u128,
        screen_context_changed: bool,
        adaptive_quiet_trainable: bool,
    ) {
        if screen_context_changed {
            self.cancel_stabilization_bursts();
        }
        let profile = self.stabilization_profiles.entry(context).or_default();
        profile.observe_output(now_ms, self.last_stdin_update, adaptive_quiet_trainable);
    }

    fn stabilization_burst(&self, context: AccessibilityContext) -> Option<StabilizationBurst> {
        self.stabilization_profiles
            .get(&context)
            .and_then(StabilizationProfile::burst)
    }

    fn rebase_stabilization_burst_deadline(&mut self, context: AccessibilityContext, now_ms: u128) {
        self.stabilization_profiles
            .entry(context)
            .or_default()
            .rebase_burst_deadline(now_ms);
    }

    fn cancel_stabilization_bursts(&mut self) {
        for profile in self.stabilization_profiles.values_mut() {
            profile.cancel_burst_context();
        }
    }

    fn finish_stabilization_burst(
        &mut self,
        context: AccessibilityContext,
        now_ms: u128,
        last_output_ms: u128,
        ordinary: bool,
        reset_context: bool,
    ) {
        let profile = self.stabilization_profiles.entry(context).or_default();
        if reset_context {
            profile.reset_context();
        } else {
            profile.finish_burst(now_ms, last_output_ms, ordinary);
        }
    }

    fn prune_retired_accessibility_views(&mut self) {
        let mut retained = self
            .output_scheduler
            .as_ref()
            .map_or_else(Vec::new, OutputScheduler::retained_accessibility_view_ids);
        if let Some(presented) = self.presented_accessibility_view {
            retained.push(presented);
        }
        retained.sort_unstable();
        retained.dedup();
        self.view_stack.retain_accessibility_views(&retained);
        let view_stack = &mut self.view_stack;
        self.stabilization_profiles
            .retain(|context, _| view_stack.contains_view_id(context.view_id));
        let maximum_reasonable_capacity =
            self.stabilization_profiles.len().saturating_mul(4).max(16);
        if self.stabilization_profiles.capacity() > maximum_reasonable_capacity {
            self.stabilization_profiles
                .shrink_to(maximum_reasonable_capacity);
        }
    }

    pub fn configure_physical_terminal(&mut self, focus_was_enabled: Option<bool>) {
        self.physical_lifecycle = PhysicalTerminalLifecycle::new(focus_was_enabled);
    }

    pub fn activate_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.activate();
        self.apply_lifecycle_transaction(term_out, transaction, true)
    }

    pub fn suspend_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.suspend();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn resume_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.resume();
        self.apply_lifecycle_transaction(term_out, transaction, true)
    }

    pub fn shutdown_physical_terminal(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let transaction = self.physical_lifecycle.shutdown();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn begin_physical_terminal_shutdown_fence(
        &mut self,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let transaction = self.physical_lifecycle.begin_shutdown_fence();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn finish_physical_terminal_shutdown_fence(
        &mut self,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let transaction = self.physical_lifecycle.finish_shutdown_fence();
        self.apply_lifecycle_transaction(term_out, transaction, false)
    }

    pub fn outstanding_startup_primary_device_attributes_replies(&self) -> usize {
        self.startup_probe_broker.as_ref().map_or(0, |broker| {
            broker.outstanding_primary_device_attributes_replies()
        })
    }

    fn apply_lifecycle_transaction(
        &mut self,
        term_out: &mut dyn Write,
        transaction: crate::presentation::LifecycleTransaction,
        reconstruct: bool,
    ) -> Result<()> {
        if matches!(transaction.damage, SceneDamage::Full) {
            self.scene_renderer.invalidate();
        }
        if !reconstruct
            && !transaction.bytes.is_empty()
            && let Some(scheduler) = &mut self.output_scheduler
        {
            scheduler.prepare_for_lifecycle_cleanup();
        }
        self.emit_physical_bytes(
            term_out,
            ScheduledOutputClass::Control,
            &transaction.bytes,
            "write physical terminal lifecycle transaction",
        )?;
        if reconstruct && !transaction.bytes.is_empty() {
            self.render_active_view(term_out)?;
        }
        Ok(())
    }

    pub fn drain_scheduled_output(
        &mut self,
        term_out: &mut dyn Write,
        force: bool,
    ) -> Result<DrainReport> {
        let Some(scheduler) = &mut self.output_scheduler else {
            return Ok(DrainReport::default());
        };
        let report = match scheduler.drain_ready(self.clock.now_ms(), force, term_out) {
            Ok(report) => report,
            Err(error) => {
                self.scene_renderer.invalidate();
                return Err(error).context("drain scheduled terminal output");
            }
        };
        if report.synchronization_timed_out {
            self.pending_synchronization_timeout_stabilization = true;
        }
        for completed in &report.completed_renders {
            self.scene_renderer.confirm(&completed.predicted);
            self.presented_scene = completed.predicted.clone();
            self.view_stack
                .apply_presented_bundle(&completed.accessibility);
            if let Some(active_view) = completed.accessibility.active_view {
                self.presented_accessibility_view = Some(active_view);
            }
            self.presented_accessibility_label
                .clone_from(&completed.accessibility.active_label);
            self.presented_accessibility_label_tracks_terminal_title =
                completed.accessibility.active_label_tracks_terminal_title;
            self.log_latency_stage("presentation-flushed", || {
                let revision = completed
                    .accessibility
                    .frames
                    .iter()
                    .find(|frame| Some(frame.view_id) == completed.accessibility.active_view)
                    .map_or(0, |frame| frame.revision.0);
                format!(
                    "active_view={:?} revision={revision}",
                    completed.accessibility.active_view
                )
            });
        }
        for completed in &report.completed_effects {
            self.presented_scene.apply_terminal_effect(&completed.event);
            if completed.owner == ROOT_SOURCE
                && self.presented_accessibility_label_tracks_terminal_title
                && let crate::terminal::TerminalEvent::TitleChanged(title) = &completed.event
            {
                self.presented_accessibility_label =
                    Some(format_terminal_accessibility_label(title));
            }
        }
        let application_synchronization_ignored = self
            .output_scheduler
            .as_ref()
            .is_some_and(OutputScheduler::application_synchronization_is_ignored);
        if !application_synchronization_ignored {
            self.pending_synchronization_timeout_stabilization = false;
        } else if self.pending_synchronization_timeout_stabilization
            && !report.completed_renders.is_empty()
            && !self.view_stack.has_overlay()
        {
            // The watchdog releases pixels and accessibility together. Begin
            // the ordinary quiet/hard fallback at that receipt rather than at
            // the much older hidden PTY output, which would otherwise speak a
            // partial frame immediately upon fail-open.
            let update = self.active_presented_update_status();
            if let Some(context) = update.context.filter(|_| update.finalization_pending) {
                self.rebase_stabilization_burst_deadline(context, self.clock.now_ms());
                self.pending_synchronization_timeout_stabilization = false;
                self.log_latency_stage("synchronization-timeout-presented", || {
                    format!("view_id={}", context.view_id.0)
                });
            }
        }
        if let Some(completed_generation) = report.application_synchronization_bypass_completed
            && self
                .compositor_transition_bypass_owner
                .is_some_and(|(generation, _)| generation == completed_generation)
        {
            let (_, transition) = self
                .compositor_transition_bypass_owner
                .take()
                .expect("a matched compositor bypass owner exists");
            self.view_stack.complete_compositor_transition(transition);
        }
        if self.compositor_transition_retry.is_some()
            && self.compositor_transition_retry != self.view_stack.compositor_transition()
        {
            self.compositor_transition_retry = None;
        }
        let compositor_transition_retry_ready = self.compositor_transition_retry.is_some()
            && self
                .output_scheduler
                .as_ref()
                .is_none_or(|scheduler| !scheduler.has_render_work());
        let timed_out_application_needs_render = report.synchronization_timed_out
            && !self.view_stack.has_overlay()
            && self
                .output_scheduler
                .as_ref()
                .is_none_or(|scheduler| !scheduler.has_render_work());
        if compositor_transition_retry_ready || timed_out_application_needs_render {
            // A compositor bypass may have replaced the held working render
            // with an overlay or committed underlay. Once the application's
            // synchronization epoch times out, publish a fresh live scene so
            // the timeout still has its documented fail-open behavior.
            // A capacity-rejected transition takes the same authoritative
            // path once the older retained bytes have drained. Consume the
            // retry before enqueueing so an individually oversized scene
            // cannot spin forever.
            self.compositor_transition_retry_attempt = self.compositor_transition_retry.take();
            self.scene_renderer.invalidate();
            let render_result = self.render_active_view(term_out);
            self.compositor_transition_retry_attempt = None;
            render_result?;
        }
        self.prune_retired_accessibility_views();
        Ok(report)
    }

    /// Reports an application atomic draw which deliberately has no physical
    /// compositor boundary yet.
    pub fn application_synchronization_holds_output(&self) -> bool {
        self.output_scheduler
            .as_ref()
            .is_some_and(OutputScheduler::application_synchronization_holds_output)
    }

    pub fn scheduled_output_timeout(&mut self) -> Option<time::Duration> {
        let output_deadline = self
            .output_scheduler
            .as_ref()
            .and_then(OutputScheduler::next_deadline_ms);
        let gateway_deadline = self
            .tmux_termination_deadline_ms
            .into_iter()
            .chain(
                self.nested_tmux_gateways
                    .values()
                    .filter_map(|gateway| gateway.termination_deadline_ms),
            )
            .min();
        let pane_resync_deadline = self
            .tmux_connections
            .iter()
            .flat_map(|connection| connection.pane_flow.values())
            .filter(|flow| !flow.resync_requested)
            .filter_map(|flow| flow.resync_after_ms)
            .min();
        let pending_input_deadline = self
            .pending_input_last_at
            .map(|last_at| last_at.saturating_add(ESC_TIMEOUT_MS));
        let presented_update = self.active_presented_update_status();
        let now_ms = self.clock.now_ms();
        let accessibility_blocked = self.application_transaction_blocks_stabilization(
            presented_update.application_transaction_open,
        );
        let accessibility_deadline = if presented_update.finalization_pending {
            presented_update
                .context
                .and_then(|context| self.stabilization_burst(context))
                .and_then(|burst| {
                    stabilization_decision(now_ms, burst, presented_update, accessibility_blocked)
                        .deadline_ms(now_ms)
                })
        } else {
            None
        };
        let deadline = output_deadline
            .into_iter()
            .chain(gateway_deadline)
            .chain(pane_resync_deadline)
            .chain(pending_input_deadline)
            .chain(accessibility_deadline)
            .chain(
                self.pending_force_abandon
                    .as_ref()
                    .map(|pending| pending.deadline_ms),
            )
            .chain(
                self.pending_graceful_teardown
                    .as_ref()
                    .and_then(|pending| pending.awaiting_deadline_ms),
            )
            .chain(
                self.startup_probe_broker
                    .as_ref()
                    .and_then(StartupProbeBroker::next_deadline_ms),
            )
            .chain(
                self.startup_probe_broker
                    .as_ref()
                    .and_then(StartupProbeBroker::color_wait_deadline_ms)
                    .filter(|deadline| *deadline > now_ms),
            )
            .chain(
                self.deferred_kitty_releases
                    .iter()
                    .map(|release| release.release_at_ms)
                    .min(),
            )
            .min()?;
        let remaining = deadline.saturating_sub(now_ms);
        Some(time::Duration::from_millis(
            remaining.try_into().unwrap_or(u64::MAX),
        ))
    }

    pub fn notify_scheduled_output_writable(&mut self) {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.notify_writable();
        }
    }

    pub fn physical_profile(&self) -> &PhysicalTerminalProfile {
        &self.physical_profile
    }

    pub fn presented_scene(&self) -> &PresentedScene {
        &self.presented_scene
    }

    pub fn take_popup_response(&mut self) -> Option<views::PopupResponse> {
        self.popup_responses.pop_front()
    }

    pub fn start_capability_probes(&mut self, term_out: &mut dyn Write) -> Result<()> {
        let mut broker = StartupProbeBroker::new(
            self.physical_profile.clone(),
            ProbePolicy::safe(),
            self.clock.now_ms(),
        );
        let queries = broker.startup_queries();
        self.emit_physical_bytes(
            term_out,
            ScheduledOutputClass::Control,
            &queries,
            "write physical terminal capability probes",
        )?;
        self.startup_probe_broker = Some(broker);
        Ok(())
    }

    pub(super) fn emit_physical_bytes(
        &mut self,
        term_out: &mut dyn Write,
        class: ScheduledOutputClass,
        bytes: &[u8],
        context: &'static str,
    ) -> Result<()> {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.enqueue_bytes(class, bytes.to_vec(), self.clock.now_ms());
            return Ok(());
        }
        term_out.write_all(bytes).with_context(|| context)?;
        term_out.flush().with_context(|| context)
    }

    pub fn flush_pending_clipboard_writes(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        for bytes in sr.take_terminal_clipboard_writes() {
            self.emit_physical_bytes(
                term_out,
                ScheduledOutputClass::Control,
                &bytes,
                "write OSC 52 system clipboard",
            )?;
        }
        Ok(())
    }

    pub(super) fn emit_physical_bells(
        &mut self,
        term_out: &mut dyn Write,
        count: usize,
    ) -> Result<()> {
        if let Some(scheduler) = &mut self.output_scheduler {
            scheduler.enqueue_bell(count, self.clock.now_ms());
            return Ok(());
        }
        let bells = vec![b'\x07'; count];
        term_out.write_all(&bells).context("write bell")?;
        term_out.flush().context("flush bell")
    }

    fn refresh_probed_profile(&mut self) {
        let Some(profile) = self
            .startup_probe_broker
            .as_ref()
            .map(|broker| broker.profile().clone())
        else {
            return;
        };
        self.set_physical_profile(profile);
    }

    fn log_bytes(&self, label: &str, bytes: &[u8]) {
        if self.log_enabled {
            if bytes.len() == 1
                && matches!(
                    label,
                    "parsed terminal event bytes"
                        | "dispatching decoded key to active view"
                        | "dispatching bytes to active view"
                )
            {
                return;
            }
            crate::diagnostics::bytes("app", label, bytes);
        }
    }

    fn log_event(&self, message: &str) {
        if self.log_enabled {
            crate::diagnostics::event("app", "event", message);
        }
    }

    fn log_latency_stage(&self, stage: &str, detail: impl FnOnce() -> String) {
        if self.log_enabled {
            crate::diagnostics::event("latency", stage, &detail());
        }
    }

    pub fn wants_tick(&mut self) -> bool {
        let presented_transition_ready =
            self.pending_view_announcement && self.accessibility_announcement_ready();
        let pending_read_ready = self.pending_active_view_read.is_some()
            && self.pending_active_view_read == self.presented_accessibility_view
            && self.logical_accessibility_view_is_presented();
        let color_wait_pending = self
            .startup_probe_broker
            .as_ref()
            .is_some_and(|broker| broker.color_wait_pending(self.clock.now_ms()));
        let pending_tmux_command_ready = if color_wait_pending {
            self.pending_tmux_commands
                .iter()
                .take_while(|command| {
                    !matches!(command.kind, PendingTmuxCommandKind::ColorReport(_))
                })
                .next()
                .is_some()
        } else {
            !self.pending_tmux_commands.is_empty()
        };
        presented_transition_ready
            || pending_read_ready
            || !self.pending_tmux_background_output.is_empty()
            || self.view_stack.active_mut().wants_tick()
            || pending_tmux_command_ready
            || !self.pending_direct_gateway_input.is_empty()
            || self
                .tmux_termination_deadline_ms
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
            || self.nested_tmux_gateways.values().any(|gateway| {
                gateway
                    .termination_deadline_ms
                    .is_some_and(|deadline| deadline <= self.clock.now_ms())
            })
            || self
                .pending_force_abandon
                .as_ref()
                .is_some_and(|pending| pending.deadline_ms <= self.clock.now_ms())
            || self
                .pending_graceful_teardown
                .as_ref()
                .and_then(|pending| pending.awaiting_deadline_ms)
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
            || self.tmux_connections.iter().any(|connection| {
                connection.pane_flow.values().any(|flow| {
                    !flow.resync_requested
                        && flow
                            .resync_after_ms
                            .is_some_and(|deadline| deadline <= self.clock.now_ms())
                })
            })
            || self
                .output_scheduler
                .as_ref()
                .and_then(OutputScheduler::next_deadline_ms)
                .is_some_and(|deadline| deadline <= self.clock.now_ms())
            || self
                .deferred_kitty_releases
                .iter()
                .any(|release| release.release_at_ms <= self.clock.now_ms())
    }

    pub fn has_overlay(&self) -> bool {
        self.view_stack.has_overlay()
    }

    #[must_use]
    pub fn tmux_connection_count(&self) -> usize {
        self.tmux_connections.len()
    }

    #[must_use]
    pub fn active_tmux_connection(&self) -> Option<u64> {
        self.active_tmux_connection
    }

    pub fn show_tmux_gateway(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == connection_id)
        {
            return Ok(false);
        }
        if let Some(GatewayOrigin::Pane {
            parent_connection_id,
            ..
        }) = self.tmux_hierarchy.origin(connection_id)
        {
            if !self
                .view_stack
                .activate_tmux_connection(parent_connection_id)
            {
                return Ok(false);
            }
            self.active_tmux_connection = Some(parent_connection_id);
            self.cancel_stabilization_bursts();
            if let Some(connection) = self
                .tmux_connections
                .iter_mut()
                .find(|connection| connection.id == connection_id)
            {
                connection.prefix_state = None;
            }
            self.pending_tmux_confirmation = None;
            self.pending_gateway_confirmation = None;
            if let Some(parent) = self.view_stack.tmux_connection_mut(parent_connection_id) {
                parent.show_connection();
            }
            self.sync_tmux_panes(parent_connection_id)?;
            self.render_active_view(term_out)?;
            self.announce_view_change(sr)?;
            return Ok(true);
        }
        if !self.view_stack.activate_tmux_connection(connection_id) {
            return Ok(false);
        }
        self.active_tmux_connection = Some(connection_id);
        self.cancel_stabilization_bursts();
        if let Some(connection) = self
            .tmux_connections
            .iter_mut()
            .find(|connection| connection.id == connection_id)
        {
            connection.prefix_state = None;
        }
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        let Some(connection) = self.view_stack.tmux_connection_mut(connection_id) else {
            return Ok(false);
        };
        connection.show_portal();
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(true)
    }

    pub fn activate_tmux_connection(
        &mut self,
        connection_id: u64,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<bool> {
        if !self
            .tmux_connections
            .iter()
            .any(|connection| connection.id == connection_id)
            || !self.view_stack.activate_tmux_connection(connection_id)
        {
            return Ok(false);
        }
        self.pending_tmux_confirmation = None;
        self.pending_gateway_confirmation = None;
        self.active_tmux_connection = Some(connection_id);
        self.cancel_stabilization_bursts();
        self.queue_tmux_connection_activation(connection_id);
        if let Some(connection) = self.view_stack.tmux_connection_mut(connection_id) {
            connection.show_connection();
        }
        self.sync_tmux_panes(connection_id)?;
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(true)
    }

    #[must_use]
    pub fn debug_tmux_topology(&self, connection_id: u64) -> Option<String> {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
            .map(|connection| connection.topology.debug_dump())
    }

    #[must_use]
    pub fn debug_tmux_gateway_origin(&self, connection_id: u64) -> Option<GatewayOrigin> {
        self.tmux_hierarchy.origin(connection_id)
    }

    pub fn debug_tmux_pane_portal_target(
        &mut self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<u64> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_portal_target(crate::tmux_model::PaneId(pane_id))
    }

    pub fn debug_tmux_pane_contents(&mut self, connection_id: u64, pane_id: u64) -> Option<String> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_contents(crate::tmux_model::PaneId(pane_id))
    }

    pub fn debug_tmux_pane_pending_update_batch_count(
        &mut self,
        connection_id: u64,
        pane_id: u64,
    ) -> Option<usize> {
        self.view_stack
            .tmux_connection_mut(connection_id)?
            .pane_pending_update_batch_count(crate::tmux_model::PaneId(pane_id))
    }

    #[must_use]
    pub fn debug_nested_tmux_gateway_count(&self) -> usize {
        self.nested_tmux_gateways.len()
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16, term_out: &mut dyn Write) -> Result<()> {
        self.on_resize_with_geometry(TerminalGeometry::from_cells(rows, cols), term_out)
    }

    pub fn on_resize_with_geometry(
        &mut self,
        geometry: TerminalGeometry,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.view_stack.on_resize_with_geometry(geometry);
        if let Some(connection_id) = self.active_tmux_connection
            && self.view_stack.tmux_connection_mut(connection_id).is_some()
        {
            self.queue_tmux_resize(connection_id, geometry);
        }
        self.render_active_view(term_out)?;
        Ok(())
    }

    pub fn show_message(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.view_stack.push(Box::new(views::MessageView::new(
            rows, cols, title, message,
        )));
        self.render_active_view(term_out)?;
        self.announce_view_change(sr)?;
        Ok(())
    }

    pub fn show_popup_announcement(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::PopupView::announcement(
                rows, cols, title, message,
            ))),
            term_out,
        )
    }

    pub fn show_popup_error(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        self.show_popup_announcement(sr, title, message, term_out)
    }

    pub fn show_popup_confirmation(
        &mut self,
        sr: &mut ScreenReader,
        title: &str,
        message: &str,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        self.handle_view_action(
            sr,
            views::ViewAction::Push(Box::new(views::PopupView::confirmation(
                rows, cols, title, message,
            ))),
            term_out,
        )
    }
}

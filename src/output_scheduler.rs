//! Serialized, bounded physical-terminal output scheduling.
//!
//! The scheduler owns byte ordering and write progress. Visual work that has
//! not started may be replaced by a newer authoritative render, while control
//! transactions and bells are retained. Once a transaction has started it is
//! never interleaved with another transaction.

use crate::{
    presentation::{PresentedAccessibilityBundle, PresentedScene, RenderBatch, SurfaceId, ViewId},
    terminal::{ProgressState, TerminalEvent, TerminalGeometry},
};
use std::{collections::VecDeque, io, io::Write};

const SYNCHRONIZED_OUTPUT_START: &[u8] = b"\x1b[?2026h";
const SYNCHRONIZED_OUTPUT_END: &[u8] = b"\x1b[?2026l";
const CONTROL_BACKLOG_LIMIT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputSchedulerConfig {
    pub latency_budget_ms: u128,
    /// How long an application synchronized-output transaction may remain
    /// idle before its newest available render is released.
    pub synchronization_timeout_ms: u128,
    /// Absolute bound for a synchronized-output transaction which keeps
    /// producing data without ever closing.
    pub synchronization_hard_timeout_ms: u128,
    pub write_budget_bytes: usize,
    pub maximum_pending_bytes: usize,
}

impl Default for OutputSchedulerConfig {
    fn default() -> Self {
        Self {
            latency_budget_ms: 4,
            synchronization_timeout_ms: 100,
            synchronization_hard_timeout_ms: 2_000,
            write_budget_bytes: 64 * 1024,
            maximum_pending_bytes: 2 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduledOutputClass {
    /// Terminal lifecycle, capability, and other nonvisual transactions.
    Control,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueOutcome {
    Queued,
    ReplacedObsoleteRender,
    DroppedForCapacity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedRender {
    pub predicted: PresentedScene,
    pub geometry: TerminalGeometry,
    /// Accessibility state for the exact render generation which has now
    /// completed a successful physical-terminal flush.
    pub accessibility: PresentedAccessibilityBundle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DrainReport {
    pub bytes_written: usize,
    pub completed_renders: Vec<CompletedRender>,
    pub completed_effects: Vec<ScheduledTerminalEffect>,
    pub blocked: bool,
    pub write_budget_exhausted: bool,
    pub synchronization_timed_out: bool,
    /// A compositor-owned render crossed an application's synchronized-output
    /// hold and has now been physically flushed.
    pub application_synchronization_bypass_completed: bool,
}

#[derive(Clone, Debug)]
struct PendingBytes {
    class: ScheduledOutputClass,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct PendingRender {
    batch: RenderBatch,
    generation: u64,
    accessibility: PresentedAccessibilityBundle,
}

#[derive(Clone, Debug)]
struct TrackedRender {
    predicted: PresentedScene,
    generation: u64,
    accessibility: PresentedAccessibilityBundle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScheduledTerminalEffect {
    pub owner: SurfaceId,
    pub event: TerminalEvent,
}

#[derive(Clone, Debug)]
struct ActiveTransaction {
    kind: ActiveTransactionKind,
    bytes: Vec<u8>,
    offset: usize,
    completed_render: Option<TrackedRender>,
    completed_effects: Vec<ScheduledTerminalEffect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveTransactionKind {
    Control,
    Render,
    Effect,
    Bell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationSynchronization {
    Active {
        started_ms: u128,
        last_activity_ms: u128,
    },
    /// A missing close must not create a repeating hold/release cycle. Once an
    /// abandoned transaction is released, later chunks flow normally until
    /// the application eventually closes that same transaction.
    IgnoringUntilClose,
}

impl ActiveTransaction {
    fn remaining(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }
}

pub struct OutputScheduler {
    config: OutputSchedulerConfig,
    synchronized_output_supported: bool,
    pending_bytes: VecDeque<PendingBytes>,
    pending_render: Option<PendingRender>,
    pending_effects: VecDeque<ScheduledTerminalEffect>,
    pending_bells: usize,
    pending_since_ms: Option<u128>,
    active: VecDeque<ActiveTransaction>,
    awaiting_flush_renders: Vec<TrackedRender>,
    awaiting_flush_effects: Vec<ScheduledTerminalEffect>,
    flush_required: bool,
    waiting_for_writable: bool,
    application_synchronization: Option<ApplicationSynchronization>,
    bypass_next_render: bool,
    application_synchronization_bypass_generation: Option<u64>,
    next_render_generation: u64,
    synchronization_timeout_release_generation: Option<u64>,
    needs_reconciliation: bool,
}

impl OutputScheduler {
    pub fn new(config: OutputSchedulerConfig, synchronized_output_supported: bool) -> Self {
        Self {
            config,
            synchronized_output_supported,
            pending_bytes: VecDeque::new(),
            pending_render: None,
            pending_effects: VecDeque::new(),
            pending_bells: 0,
            pending_since_ms: None,
            active: VecDeque::new(),
            awaiting_flush_renders: Vec::new(),
            awaiting_flush_effects: Vec::new(),
            flush_required: false,
            waiting_for_writable: false,
            application_synchronization: None,
            bypass_next_render: false,
            application_synchronization_bypass_generation: None,
            next_render_generation: 1,
            synchronization_timeout_release_generation: None,
            needs_reconciliation: false,
        }
    }

    pub fn enqueue_bytes(&mut self, class: ScheduledOutputClass, bytes: Vec<u8>, now_ms: u128) {
        if bytes.is_empty() {
            return;
        }
        let control_bytes = self.control_bytes();
        if bytes.len() > CONTROL_BACKLOG_LIMIT_BYTES
            || control_bytes.saturating_add(bytes.len()) > CONTROL_BACKLOG_LIMIT_BYTES
        {
            return;
        }
        if self.pending_bytes().saturating_add(bytes.len()) > self.config.maximum_pending_bytes {
            // Lifecycle/control bytes outrank visual work. All discarded work
            // is unstarted and can be regenerated from the authoritative
            // scene; a partially written transaction and a render selected as
            // a synchronization release boundary are never removed.
            let release_generation = self.synchronization_timeout_release_generation;
            let bypass_generation = self.application_synchronization_bypass_generation;
            if !self.pending_render.as_ref().is_some_and(|render| {
                Some(render.generation) == release_generation
                    || Some(render.generation) == bypass_generation
            }) {
                self.pending_render = None;
            }
            self.pending_effects.clear();
            self.pending_bells = 0;
            self.active.retain(|transaction| {
                transaction.offset > 0
                    || transaction.kind == ActiveTransactionKind::Control
                    || transaction.completed_render.as_ref().is_some_and(|render| {
                        Some(render.generation) == release_generation
                            || Some(render.generation) == bypass_generation
                    })
            });
            if self.pending_bytes().saturating_add(bytes.len()) > self.config.maximum_pending_bytes
            {
                return;
            }
        }
        self.note_pending(now_ms);
        if let Some(last) = self.pending_bytes.back_mut()
            && last.class == class
            && last.bytes.len().saturating_add(bytes.len()) <= self.config.maximum_pending_bytes
        {
            last.bytes.extend_from_slice(&bytes);
        } else {
            self.pending_bytes.push_back(PendingBytes { class, bytes });
        }
    }

    /// Discards work that has not begun so a suspend/shutdown cleanup becomes
    /// the final queued transaction. A transaction with bytes already written
    /// is retained and completed before cleanup to avoid leaving a partial VT
    /// sequence on the physical terminal.
    pub fn prepare_for_lifecycle_cleanup(&mut self) {
        self.pending_bytes.clear();
        self.pending_render = None;
        self.pending_effects.clear();
        self.pending_bells = 0;
        self.active.retain(|transaction| transaction.offset > 0);
        // A render whose bytes are waiting only for flush still has to reach
        // that fence before cleanup bytes are written, but its presentation
        // receipt is obsolete: the lifecycle transaction will supersede the
        // scene in the same drain call before the application can observe the
        // report.
        self.awaiting_flush_renders.clear();
        self.application_synchronization = None;
        self.bypass_next_render = false;
        self.application_synchronization_bypass_generation = None;
        self.synchronization_timeout_release_generation = None;
        if self.active.is_empty()
            && self.awaiting_flush_renders.is_empty()
            && self.awaiting_flush_effects.is_empty()
        {
            self.pending_since_ms = None;
        }
    }

    pub fn enqueue_render(&mut self, batch: RenderBatch, now_ms: u128) -> EnqueueOutcome {
        self.enqueue_render_with_accessibility(
            batch,
            PresentedAccessibilityBundle::default(),
            now_ms,
        )
    }

    /// Queues a render and binds its accessibility state to the same write and
    /// flush lifecycle. Replacing an unstarted render replaces its bundle too;
    /// a started render retains its own bundle until its flush completes.
    pub fn enqueue_render_with_accessibility(
        &mut self,
        batch: RenderBatch,
        accessibility: PresentedAccessibilityBundle,
        now_ms: u128,
    ) -> EnqueueOutcome {
        let bypass_requested = std::mem::take(&mut self.bypass_next_render);
        let Some(render_bytes) = render_batch_byte_len(&batch, self.synchronized_output_supported)
        else {
            return EnqueueOutcome::DroppedForCapacity;
        };
        let replaceable_pending_bytes = self.pending_render.as_ref().map_or(0, |render| {
            render_batch_byte_len(&render.batch, self.synchronized_output_supported)
                .unwrap_or(usize::MAX)
        });
        let replaceable_active_index = self.active.iter().position(|transaction| {
            transaction.kind == ActiveTransactionKind::Render && transaction.offset == 0
        });
        let replaceable_active_bytes = replaceable_active_index
            .and_then(|index| self.active.get(index))
            .map_or(0, |transaction| transaction.bytes.len());
        let retained_bytes = self
            .pending_bytes()
            .saturating_sub(replaceable_pending_bytes)
            .saturating_sub(replaceable_active_bytes);
        if render_bytes > self.config.maximum_pending_bytes
            || retained_bytes.saturating_add(render_bytes) > self.config.maximum_pending_bytes
        {
            return EnqueueOutcome::DroppedForCapacity;
        }

        let mut outcome = EnqueueOutcome::Queued;
        let mut replaces_timeout_release = false;
        let mut replaces_synchronization_bypass = false;
        if let Some(replaced) = self.pending_render.take() {
            replaces_timeout_release |=
                Some(replaced.generation) == self.synchronization_timeout_release_generation;
            replaces_synchronization_bypass |=
                Some(replaced.generation) == self.application_synchronization_bypass_generation;
            outcome = EnqueueOutcome::ReplacedObsoleteRender;
        }
        if let Some(render_index) = replaceable_active_index {
            let replaced = self
                .active
                .remove(render_index)
                .expect("render index exists");
            replaces_timeout_release |= replaced.completed_render.as_ref().is_some_and(|render| {
                Some(render.generation) == self.synchronization_timeout_release_generation
            });
            replaces_synchronization_bypass |=
                replaced.completed_render.as_ref().is_some_and(|render| {
                    Some(render.generation) == self.application_synchronization_bypass_generation
                });
            let mut index = 0;
            while index < self.active.len() {
                if self.active[index].kind == ActiveTransactionKind::Bell
                    && self.active[index].offset == 0
                {
                    let bell = self.active.remove(index).expect("bell index exists");
                    self.pending_bells = self.pending_bells.saturating_add(bell.bytes.len());
                } else {
                    index += 1;
                }
            }
            self.pending_since_ms = Some(now_ms.saturating_sub(self.config.latency_budget_ms));
            outcome = EnqueueOutcome::ReplacedObsoleteRender;
        }
        let generation = self.next_render_generation;
        self.next_render_generation = self.next_render_generation.wrapping_add(1).max(1);
        if replaces_timeout_release {
            self.synchronization_timeout_release_generation = Some(generation);
        }
        if bypass_requested || replaces_synchronization_bypass {
            self.application_synchronization_bypass_generation = Some(generation);
        }
        self.note_pending(now_ms);
        self.pending_render = Some(PendingRender {
            batch,
            generation,
            accessibility,
        });
        outcome
    }

    pub fn enqueue_terminal_effect(
        &mut self,
        owner: SurfaceId,
        event: TerminalEvent,
        now_ms: u128,
    ) {
        let event = bound_terminal_effect(event, self.config.maximum_pending_bytes);
        let event_kind = terminal_event_kind(&event);
        if matches!(
            event_kind,
            TerminalEventKind::Title | TerminalEventKind::WorkingDirectory
        ) {
            // These model effects describe current state, so only their newest
            // unstarted value is authoritative. This also lets a compositor
            // render replace a held working-frame value with the committed one.
            self.pending_effects.retain(|pending| {
                pending.owner != owner || terminal_event_kind(&pending.event) != event_kind
            });
            self.active.retain(|transaction| {
                transaction.offset != 0
                    || transaction.kind != ActiveTransactionKind::Effect
                    || !transaction.completed_effects.iter().any(|pending| {
                        pending.owner == owner && terminal_event_kind(&pending.event) == event_kind
                    })
            });
        }
        let retained = terminal_event_retained_bytes(&event);
        let pending = self.retained_effect_bytes();
        if pending.saturating_add(retained) > self.config.maximum_pending_bytes
            && let Some(index) = self.pending_effects.iter().position(|pending| {
                pending.owner == owner && terminal_event_kind(&pending.event) == event_kind
            })
        {
            self.pending_effects.remove(index);
        }
        let pending = self.pending_bytes();
        if pending.saturating_add(retained) > self.config.maximum_pending_bytes {
            return;
        }
        self.note_pending(now_ms);
        self.pending_effects
            .push_back(ScheduledTerminalEffect { owner, event });
    }

    pub fn set_synchronized_output_supported(&mut self, supported: bool) {
        self.synchronized_output_supported = supported;
    }

    pub fn has_render_work(&self) -> bool {
        self.pending_render.is_some()
            || !self.awaiting_flush_renders.is_empty()
            || self
                .active
                .iter()
                .any(|transaction| transaction.kind == ActiveTransactionKind::Render)
    }

    pub fn enqueue_bell(&mut self, count: usize, now_ms: u128) {
        if count == 0 {
            return;
        }
        let retained = count.min(
            self.config
                .maximum_pending_bytes
                .saturating_sub(self.pending_bytes()),
        );
        if retained != 0 {
            self.note_pending(now_ms);
            self.pending_bells = self.pending_bells.saturating_add(retained);
        }
    }

    pub fn set_application_synchronized(&mut self, synchronized: bool, now_ms: u128) {
        self.set_application_synchronization(synchronized, false, now_ms);
    }

    pub fn set_application_synchronization(
        &mut self,
        synchronized: bool,
        opened: bool,
        now_ms: u128,
    ) {
        self.observe_application_synchronization(synchronized, opened, true, now_ms);
    }

    pub fn observe_application_synchronization(
        &mut self,
        synchronized: bool,
        opened: bool,
        activity: bool,
        now_ms: u128,
    ) {
        if synchronized {
            match &mut self.application_synchronization {
                Some(ApplicationSynchronization::IgnoringUntilClose) if opened => {
                    self.application_synchronization = Some(ApplicationSynchronization::Active {
                        started_ms: now_ms,
                        last_activity_ms: now_ms,
                    });
                    self.synchronization_timeout_release_generation = None;
                }
                Some(ApplicationSynchronization::Active {
                    last_activity_ms, ..
                }) if activity => {
                    *last_activity_ms = now_ms;
                }
                Some(ApplicationSynchronization::Active { .. }) => {}
                Some(ApplicationSynchronization::IgnoringUntilClose) => {}
                None => {
                    self.application_synchronization = Some(ApplicationSynchronization::Active {
                        started_ms: now_ms,
                        last_activity_ms: now_ms,
                    });
                }
            }
        } else {
            self.application_synchronization = None;
            self.bypass_next_render = false;
            self.synchronization_timeout_release_generation = None;
        }
    }

    /// Arms the next render enqueue to pass the application's synchronization
    /// hold. The bypass belongs to that accepted render, follows an unstarted
    /// replacement, and ends only after the render has been flushed.
    pub fn set_application_synchronization_bypassed(&mut self, bypassed: bool) {
        self.bypass_next_render = bypassed && self.application_synchronization.is_some();
    }

    pub fn next_deadline_ms(&self) -> Option<u128> {
        if self.waiting_for_writable {
            return None;
        }
        let synchronization_deadline = match self.application_synchronization {
            Some(ApplicationSynchronization::Active {
                started_ms,
                last_activity_ms,
            }) => Some(
                last_activity_ms
                    .saturating_add(self.config.synchronization_timeout_ms)
                    .min(started_ms.saturating_add(self.config.synchronization_hard_timeout_ms)),
            ),
            Some(ApplicationSynchronization::IgnoringUntilClose) | None => None,
        };
        if self.active.is_empty()
            && self.pending_bytes.is_empty()
            && self.pending_render.is_none()
            && self.pending_effects.is_empty()
            && self.pending_bells == 0
            && !self.flush_required
        {
            return synchronization_deadline;
        }
        let application_hold_blocks_unstarted_active =
            matches!(
                self.application_synchronization,
                Some(ApplicationSynchronization::Active { .. })
            ) && self.application_synchronization_bypass_generation.is_none()
                && self
                    .active
                    .front()
                    .is_some_and(|transaction| transaction.offset == 0);
        if (!self.active.is_empty() || self.flush_required)
            && !application_hold_blocks_unstarted_active
        {
            return Some(0);
        }
        if self.application_synchronization_bypass_generation.is_none()
            && let Some(deadline) = synchronization_deadline
        {
            return Some(deadline);
        }
        self.pending_since_ms
            .map(|started| started.saturating_add(self.config.latency_budget_ms))
            .into_iter()
            .chain(synchronization_deadline)
            .min()
    }

    pub fn pending_bytes(&self) -> usize {
        let active = self
            .active
            .iter()
            .map(|transaction| transaction.bytes.len().saturating_sub(transaction.offset))
            .sum::<usize>();
        let queued = self
            .pending_bytes
            .iter()
            .map(|transaction| transaction.bytes.len())
            .sum::<usize>();
        let render = self.pending_render.as_ref().map_or(0, |render| {
            render_batch_byte_len(&render.batch, self.synchronized_output_supported)
                .unwrap_or(usize::MAX)
        });
        let effects = self
            .pending_effects
            .iter()
            .map(|effect| terminal_event_retained_bytes(&effect.event))
            .sum::<usize>();
        active
            .saturating_add(queued)
            .saturating_add(render)
            .saturating_add(effects)
            .saturating_add(self.pending_bells)
    }

    pub fn pending_effect_count(&self) -> usize {
        self.pending_effects.len()
            + self.awaiting_flush_effects.len()
            + self
                .active
                .iter()
                .map(|transaction| transaction.completed_effects.len())
                .sum::<usize>()
    }

    /// View identities still owned by an uncompleted physical render.
    /// Callers use this to retain logically removed view models only while a
    /// pending, started, or flush-blocked receipt can still make one current.
    pub(crate) fn retained_accessibility_view_ids(&self) -> Vec<ViewId> {
        let mut ids = Vec::new();
        let mut collect = |bundle: &PresentedAccessibilityBundle| {
            if let Some(active_view) = bundle.active_view {
                ids.push(active_view);
            }
            ids.extend(bundle.frames.iter().map(|frame| frame.view_id));
        };
        if let Some(render) = &self.pending_render {
            collect(&render.accessibility);
        }
        for render in self
            .active
            .iter()
            .filter_map(|transaction| transaction.completed_render.as_ref())
            .chain(self.awaiting_flush_renders.iter())
        {
            collect(&render.accessibility);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn retained_effect_bytes(&self) -> usize {
        self.pending_effects
            .iter()
            .chain(&self.awaiting_flush_effects)
            .chain(
                self.active
                    .iter()
                    .flat_map(|transaction| &transaction.completed_effects),
            )
            .map(|effect| terminal_event_retained_bytes(&effect.event))
            .sum()
    }

    fn control_bytes(&self) -> usize {
        let active = self
            .active
            .iter()
            .filter(|transaction| transaction.kind == ActiveTransactionKind::Control)
            .map(|transaction| transaction.bytes.len().saturating_sub(transaction.offset))
            .sum::<usize>();
        self.pending_bytes
            .iter()
            .map(|pending| pending.bytes.len())
            .sum::<usize>()
            .saturating_add(active)
    }

    pub const fn needs_reconciliation(&self) -> bool {
        self.needs_reconciliation
    }

    pub fn recover(&mut self) {
        self.needs_reconciliation = false;
        self.waiting_for_writable = false;
    }

    pub fn notify_writable(&mut self) {
        self.waiting_for_writable = false;
    }

    pub fn drain_ready(
        &mut self,
        now_ms: u128,
        force: bool,
        writer: &mut dyn Write,
    ) -> io::Result<DrainReport> {
        let mut report = DrainReport::default();
        if self.needs_reconciliation {
            return Ok(report);
        }
        if self.waiting_for_writable && !force {
            return Ok(report);
        }
        if force {
            self.waiting_for_writable = false;
        }

        if self.flush_required && !self.flush_writer(writer, &mut report)? {
            return Ok(report);
        }

        let mut finish_started_transaction_only = false;
        if let Some(ApplicationSynchronization::Active {
            started_ms,
            last_activity_ms,
        }) = self.application_synchronization
        {
            let idle_timed_out =
                now_ms >= last_activity_ms.saturating_add(self.config.synchronization_timeout_ms);
            let hard_timed_out =
                now_ms >= started_ms.saturating_add(self.config.synchronization_hard_timeout_ms);
            let timed_out = idle_timed_out || hard_timed_out;
            if !force && !timed_out && self.application_synchronization_bypass_generation.is_none()
            {
                if self
                    .active
                    .front()
                    .is_some_and(|transaction| transaction.offset > 0)
                {
                    finish_started_transaction_only = true;
                } else {
                    return Ok(report);
                }
            }
            if timed_out {
                self.application_synchronization =
                    Some(ApplicationSynchronization::IgnoringUntilClose);
                self.synchronization_timeout_release_generation = self
                    .pending_render
                    .as_ref()
                    .map(|render| render.generation)
                    .or_else(|| {
                        self.active.iter().rev().find_map(|transaction| {
                            transaction
                                .completed_render
                                .as_ref()
                                .map(|render| render.generation)
                        })
                    })
                    .or_else(|| {
                        self.awaiting_flush_renders
                            .last()
                            .map(|render| render.generation)
                    });
                if self.synchronization_timeout_release_generation.is_none() {
                    report.synchronization_timed_out = true;
                }
            }
        }
        if !force
            && self.active.is_empty()
            && self.pending_since_ms.is_some_and(|started| {
                now_ms < started.saturating_add(self.config.latency_budget_ms)
            })
        {
            return Ok(report);
        }

        self.activate_pending();
        let budget = self.config.write_budget_bytes.max(1);
        let mut reached_synchronization_bypass = false;
        while report.bytes_written < budget {
            let Some(transaction) = self.active.front_mut() else {
                break;
            };
            if finish_started_transaction_only && transaction.offset == 0 {
                break;
            }
            if transaction.remaining().is_empty() {
                let completed = self.active.pop_front().expect("active front exists");
                reached_synchronization_bypass =
                    completed.completed_render.as_ref().is_some_and(|render| {
                        Some(render.generation)
                            == self.application_synchronization_bypass_generation
                    });
                if let Some(predicted) = completed.completed_render {
                    self.awaiting_flush_renders.push(predicted);
                }
                self.awaiting_flush_effects
                    .extend(completed.completed_effects);
                if reached_synchronization_bypass {
                    break;
                }
                continue;
            }
            let remaining_budget = budget.saturating_sub(report.bytes_written);
            let bytes =
                &transaction.remaining()[..transaction.remaining().len().min(remaining_budget)];
            match writer.write(bytes) {
                Ok(0) => return self.fail(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(written) => {
                    transaction.offset = transaction.offset.saturating_add(written);
                    report.bytes_written = report.bytes_written.saturating_add(written);
                    self.flush_required = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.waiting_for_writable = true;
                    report.blocked = true;
                    return Ok(report);
                }
                Err(error) => return self.fail(error),
            }
        }
        // Record a render whose last byte landed exactly on the budget.
        while !reached_synchronization_bypass
            && self
                .active
                .front()
                .is_some_and(|transaction| transaction.remaining().is_empty())
        {
            let completed = self.active.pop_front().expect("active front exists");
            reached_synchronization_bypass =
                completed.completed_render.as_ref().is_some_and(|render| {
                    Some(render.generation) == self.application_synchronization_bypass_generation
                });
            if let Some(predicted) = completed.completed_render {
                self.awaiting_flush_renders.push(predicted);
            }
            self.awaiting_flush_effects
                .extend(completed.completed_effects);
        }
        if self.flush_required && !self.flush_writer(writer, &mut report)? {
            return Ok(report);
        }
        if !self.flush_required {
            self.complete_awaiting_renders(&mut report);
        }
        // A call may finish the previously active transaction without having
        // activated work which was queued behind it. Report that remaining
        // boundary work so EOF/suspend/shutdown drain loops call us again
        // instead of discarding the newest complete scene during cleanup.
        report.write_budget_exhausted = !self.active.is_empty()
            || !self.pending_bytes.is_empty()
            || self.pending_render.is_some()
            || !self.pending_effects.is_empty()
            || self.pending_bells != 0;
        Ok(report)
    }

    fn note_pending(&mut self, now_ms: u128) {
        self.pending_since_ms.get_or_insert(now_ms);
    }

    fn activate_pending(&mut self) {
        if !self.active.is_empty() {
            return;
        }
        for pending in self.pending_bytes.drain(..) {
            self.active.push_back(ActiveTransaction {
                kind: match pending.class {
                    ScheduledOutputClass::Control => ActiveTransactionKind::Control,
                },
                bytes: pending.bytes,
                offset: 0,
                completed_render: None,
                completed_effects: Vec::new(),
            });
        }
        for effect in self.pending_effects.drain(..) {
            self.active.push_back(ActiveTransaction {
                kind: ActiveTransactionKind::Effect,
                bytes: encode_terminal_effect(&effect.event),
                offset: 0,
                completed_render: None,
                completed_effects: vec![effect],
            });
        }
        if let Some(render) = self.pending_render.take() {
            let mut bytes = Vec::new();
            if self.synchronized_output_supported {
                bytes.extend_from_slice(SYNCHRONIZED_OUTPUT_START);
            }
            for transaction in render.batch.transactions {
                append_without_synchronization_markers(&mut bytes, &transaction.bytes);
            }
            if self.synchronized_output_supported {
                bytes.extend_from_slice(SYNCHRONIZED_OUTPUT_END);
            }
            self.active.push_back(ActiveTransaction {
                kind: ActiveTransactionKind::Render,
                bytes,
                offset: 0,
                completed_render: Some(TrackedRender {
                    predicted: render.batch.predicted,
                    generation: render.generation,
                    accessibility: render.accessibility,
                }),
                completed_effects: Vec::new(),
            });
        }
        if self.pending_bells > 0 {
            self.active.push_back(ActiveTransaction {
                kind: ActiveTransactionKind::Bell,
                bytes: vec![b'\x07'; self.pending_bells],
                offset: 0,
                completed_render: None,
                completed_effects: Vec::new(),
            });
            self.pending_bells = 0;
        }
        self.pending_since_ms = None;
    }

    fn fail<T>(&mut self, error: io::Error) -> io::Result<T> {
        self.active.clear();
        self.awaiting_flush_renders.clear();
        self.awaiting_flush_effects.clear();
        self.flush_required = false;
        self.waiting_for_writable = false;
        self.pending_bytes.clear();
        self.pending_render = None;
        self.pending_effects.clear();
        self.pending_bells = 0;
        self.pending_since_ms = None;
        self.application_synchronization = None;
        self.bypass_next_render = false;
        self.application_synchronization_bypass_generation = None;
        self.synchronization_timeout_release_generation = None;
        self.needs_reconciliation = true;
        Err(error)
    }

    fn flush_writer(
        &mut self,
        writer: &mut dyn Write,
        report: &mut DrainReport,
    ) -> io::Result<bool> {
        loop {
            match writer.flush() {
                Ok(()) => {
                    self.flush_required = false;
                    self.complete_awaiting_renders(report);
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.waiting_for_writable = true;
                    report.blocked = true;
                    return Ok(false);
                }
                Err(error) => return self.fail(error),
            }
        }
    }

    fn complete_awaiting_renders(&mut self, report: &mut DrainReport) {
        for render in self.awaiting_flush_renders.drain(..) {
            let releases_synchronization =
                Some(render.generation) == self.synchronization_timeout_release_generation;
            let completes_synchronization_bypass =
                Some(render.generation) == self.application_synchronization_bypass_generation;
            report.completed_renders.push(CompletedRender {
                geometry: render.predicted.geometry(),
                predicted: render.predicted,
                accessibility: render.accessibility,
            });
            if releases_synchronization {
                report.synchronization_timed_out = true;
                self.synchronization_timeout_release_generation = None;
            }
            if completes_synchronization_bypass {
                self.application_synchronization_bypass_generation = None;
                report.application_synchronization_bypass_completed = true;
            }
        }
        report
            .completed_effects
            .append(&mut self.awaiting_flush_effects);
    }
}

fn encode_terminal_effect(event: &TerminalEvent) -> Vec<u8> {
    let mut bytes = Vec::new();
    match event {
        TerminalEvent::TitleChanged(title) => write_osc(&mut bytes, 2, title),
        TerminalEvent::WorkingDirectoryChanged(directory) => write_osc(&mut bytes, 7, directory),
        TerminalEvent::ProgressReport { state, progress } => {
            let state = match state {
                ProgressState::Remove => 0,
                ProgressState::Set => 1,
                ProgressState::Error => 2,
                ProgressState::Indeterminate => 3,
                ProgressState::Pause => 4,
            };
            bytes.extend_from_slice(format!("\x1b]9;4;{state}").as_bytes());
            if let Some(progress) = progress {
                bytes.extend_from_slice(format!(";{}", progress.min(&100)).as_bytes());
            }
            bytes.extend_from_slice(b"\x1b\\");
        }
        TerminalEvent::Bell => bytes.push(b'\x07'),
        TerminalEvent::ClipboardWrite { .. }
        | TerminalEvent::DesktopNotification { .. }
        | TerminalEvent::Query(_)
        | TerminalEvent::PtyReply(_)
        | TerminalEvent::UnknownSequence { .. } => {}
    }
    bytes
}

fn write_osc(bytes: &mut Vec<u8>, code: u8, value: &str) {
    bytes.extend_from_slice(b"\x1b]");
    bytes.extend_from_slice(code.to_string().as_bytes());
    bytes.push(b';');
    bytes.extend(value.bytes().filter(|byte| *byte >= b' ' && *byte != 0x7f));
    bytes.extend_from_slice(b"\x1b\\");
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminalEventKind {
    Bell,
    Title,
    WorkingDirectory,
    Clipboard,
    Notification,
    Progress,
    Query,
    Reply,
    Unknown,
}

fn terminal_event_kind(event: &TerminalEvent) -> TerminalEventKind {
    match event {
        TerminalEvent::Bell => TerminalEventKind::Bell,
        TerminalEvent::TitleChanged(_) => TerminalEventKind::Title,
        TerminalEvent::WorkingDirectoryChanged(_) => TerminalEventKind::WorkingDirectory,
        TerminalEvent::ClipboardWrite { .. } => TerminalEventKind::Clipboard,
        TerminalEvent::DesktopNotification { .. } => TerminalEventKind::Notification,
        TerminalEvent::ProgressReport { .. } => TerminalEventKind::Progress,
        TerminalEvent::Query(_) => TerminalEventKind::Query,
        TerminalEvent::PtyReply(_) => TerminalEventKind::Reply,
        TerminalEvent::UnknownSequence { .. } => TerminalEventKind::Unknown,
    }
}

fn render_batch_byte_len(
    batch: &RenderBatch,
    synchronized_output_supported: bool,
) -> Option<usize> {
    batch
        .transactions
        .iter()
        .try_fold(0usize, |total, transaction| {
            total.checked_add(transaction.bytes.len())
        })
        .and_then(|total| {
            total.checked_add(if synchronized_output_supported {
                SYNCHRONIZED_OUTPUT_START.len() + SYNCHRONIZED_OUTPUT_END.len()
            } else {
                0
            })
        })
}

fn terminal_event_retained_bytes(event: &TerminalEvent) -> usize {
    let payload = match event {
        TerminalEvent::TitleChanged(value) | TerminalEvent::WorkingDirectoryChanged(value) => {
            value.len()
        }
        TerminalEvent::ClipboardWrite { contents, .. } => contents
            .iter()
            .map(|content| content.mime.len().saturating_add(content.data.len()))
            .sum(),
        TerminalEvent::DesktopNotification { title, body } => {
            title.len().saturating_add(body.len())
        }
        TerminalEvent::PtyReply(bytes) | TerminalEvent::UnknownSequence { content: bytes, .. } => {
            bytes.len()
        }
        TerminalEvent::Bell | TerminalEvent::ProgressReport { .. } | TerminalEvent::Query(_) => 0,
    };
    payload.saturating_add(16)
}

fn bound_terminal_effect(mut event: TerminalEvent, maximum: usize) -> TerminalEvent {
    let payload_limit = maximum.saturating_sub(16);
    match &mut event {
        TerminalEvent::TitleChanged(value) | TerminalEvent::WorkingDirectoryChanged(value) => {
            truncate_string(value, payload_limit);
        }
        TerminalEvent::ClipboardWrite { contents, .. } => {
            let mut remaining = payload_limit;
            for content in contents.iter_mut() {
                truncate_string(&mut content.mime, remaining);
                remaining = remaining.saturating_sub(content.mime.len());
                content.data.truncate(remaining);
                remaining = remaining.saturating_sub(content.data.len());
            }
            contents.retain(|content| !content.mime.is_empty() || !content.data.is_empty());
        }
        TerminalEvent::DesktopNotification { title, body } => {
            truncate_string(title, payload_limit);
            truncate_string(body, payload_limit.saturating_sub(title.len()));
        }
        TerminalEvent::PtyReply(bytes) | TerminalEvent::UnknownSequence { content: bytes, .. } => {
            bytes.truncate(payload_limit);
        }
        TerminalEvent::Bell | TerminalEvent::ProgressReport { .. } | TerminalEvent::Query(_) => {}
    }
    event
}

fn truncate_string(value: &mut String, maximum: usize) {
    if value.len() <= maximum {
        return;
    }
    let boundary = (0..=maximum)
        .rev()
        .find(|&index| value.is_char_boundary(index))
        .unwrap_or(0);
    value.truncate(boundary);
}

fn append_without_synchronization_markers(target: &mut Vec<u8>, source: &[u8]) {
    let mut offset = 0;
    while offset < source.len() {
        let remaining = &source[offset..];
        if remaining.starts_with(SYNCHRONIZED_OUTPUT_START) {
            offset += SYNCHRONIZED_OUTPUT_START.len();
        } else if remaining.starts_with(SYNCHRONIZED_OUTPUT_END) {
            offset += SYNCHRONIZED_OUTPUT_END.len();
        } else {
            target.push(source[offset]);
            offset += 1;
        }
    }
}

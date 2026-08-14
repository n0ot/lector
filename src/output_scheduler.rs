//! Serialized, bounded physical-terminal output scheduling.
//!
//! The scheduler owns byte ordering and write progress. Visual work that has
//! not started may be replaced by a newer authoritative render, while control
//! transactions and bells are retained. Once a transaction has started it is
//! never interleaved with another transaction.

use crate::{
    presentation::{PresentedScene, RenderBatch, SurfaceId},
    terminal::{ProgressState, TerminalEvent, TerminalGeometry},
};
use std::{collections::VecDeque, io, io::Write};

const SYNCHRONIZED_OUTPUT_START: &[u8] = b"\x1b[?2026h";
const SYNCHRONIZED_OUTPUT_END: &[u8] = b"\x1b[?2026l";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutputSchedulerConfig {
    pub latency_budget_ms: u128,
    pub synchronization_timeout_ms: u128,
    pub write_budget_bytes: usize,
    pub maximum_pending_bytes: usize,
}

impl Default for OutputSchedulerConfig {
    fn default() -> Self {
        Self {
            latency_budget_ms: 4,
            synchronization_timeout_ms: 100,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedRender {
    pub predicted: PresentedScene,
    pub geometry: TerminalGeometry,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DrainReport {
    pub bytes_written: usize,
    pub completed_renders: Vec<CompletedRender>,
    pub completed_effects: Vec<ScheduledTerminalEffect>,
    pub blocked: bool,
    pub write_budget_exhausted: bool,
    pub synchronization_timed_out: bool,
}

#[derive(Clone, Debug)]
struct PendingBytes {
    class: ScheduledOutputClass,
    bytes: Vec<u8>,
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
    completed_render: Option<PresentedScene>,
    completed_effects: Vec<ScheduledTerminalEffect>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveTransactionKind {
    Control,
    Render,
    Effect,
    Bell,
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
    pending_render: Option<RenderBatch>,
    pending_effects: VecDeque<ScheduledTerminalEffect>,
    pending_bells: usize,
    pending_since_ms: Option<u128>,
    active: VecDeque<ActiveTransaction>,
    awaiting_flush_renders: Vec<PresentedScene>,
    awaiting_flush_effects: Vec<ScheduledTerminalEffect>,
    flush_required: bool,
    waiting_for_writable: bool,
    application_sync_started_ms: Option<u128>,
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
            application_sync_started_ms: None,
            needs_reconciliation: false,
        }
    }

    pub fn enqueue_bytes(&mut self, class: ScheduledOutputClass, bytes: Vec<u8>, now_ms: u128) {
        if bytes.is_empty() {
            return;
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
        self.application_sync_started_ms = None;
        if self.active.is_empty()
            && self.awaiting_flush_renders.is_empty()
            && self.awaiting_flush_effects.is_empty()
        {
            self.pending_since_ms = None;
        }
    }

    pub fn enqueue_render(&mut self, batch: RenderBatch, now_ms: u128) -> EnqueueOutcome {
        self.note_pending(now_ms);
        if self.pending_render.replace(batch).is_some() {
            return EnqueueOutcome::ReplacedObsoleteRender;
        }
        if let Some(render_index) = self.active.iter().position(|transaction| {
            transaction.kind == ActiveTransactionKind::Render && transaction.offset == 0
        }) {
            self.active.remove(render_index);
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
            return EnqueueOutcome::ReplacedObsoleteRender;
        }
        EnqueueOutcome::Queued
    }

    pub fn enqueue_terminal_effect(
        &mut self,
        owner: SurfaceId,
        event: TerminalEvent,
        now_ms: u128,
    ) {
        let event = bound_terminal_effect(event, self.config.maximum_pending_bytes);
        let retained = terminal_event_retained_bytes(&event);
        let pending = self.retained_effect_bytes();
        if pending.saturating_add(retained) > self.config.maximum_pending_bytes
            && let Some(index) = self.pending_effects.iter().position(|pending| {
                pending.owner == owner
                    && terminal_event_kind(&pending.event) == terminal_event_kind(&event)
            })
        {
            self.pending_effects.remove(index);
        }
        let pending = self.retained_effect_bytes();
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
        self.note_pending(now_ms);
        self.pending_bells = self.pending_bells.saturating_add(count);
    }

    pub fn set_application_synchronized(&mut self, synchronized: bool, now_ms: u128) {
        if synchronized {
            self.application_sync_started_ms.get_or_insert(now_ms);
        } else {
            self.application_sync_started_ms = None;
        }
    }

    pub fn next_deadline_ms(&self) -> Option<u128> {
        if self.waiting_for_writable {
            return None;
        }
        if self.active.is_empty()
            && self.pending_bytes.is_empty()
            && self.pending_render.is_none()
            && self.pending_effects.is_empty()
            && self.pending_bells == 0
            && !self.flush_required
        {
            return None;
        }
        if !self.active.is_empty() || self.flush_required {
            return Some(0);
        }
        if let Some(started) = self.application_sync_started_ms {
            return Some(started.saturating_add(self.config.synchronization_timeout_ms));
        }
        self.pending_since_ms
            .map(|started| started.saturating_add(self.config.latency_budget_ms))
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
        let render = self.pending_render.as_ref().map_or(0, |batch| {
            batch
                .transactions
                .iter()
                .map(|transaction| transaction.bytes.len())
                .sum()
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

        if let Some(started) = self.application_sync_started_ms {
            let timed_out =
                now_ms >= started.saturating_add(self.config.synchronization_timeout_ms);
            if !force && !timed_out {
                return Ok(report);
            }
            if timed_out {
                report.synchronization_timed_out = true;
                self.application_sync_started_ms = None;
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
        while report.bytes_written < budget {
            let Some(transaction) = self.active.front_mut() else {
                break;
            };
            if transaction.remaining().is_empty() {
                let completed = self.active.pop_front().expect("active front exists");
                if let Some(predicted) = completed.completed_render {
                    self.awaiting_flush_renders.push(predicted);
                }
                self.awaiting_flush_effects
                    .extend(completed.completed_effects);
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
        while self
            .active
            .front()
            .is_some_and(|transaction| transaction.remaining().is_empty())
        {
            let completed = self.active.pop_front().expect("active front exists");
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
        report.write_budget_exhausted = !self.active.is_empty();
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
        if let Some(batch) = self.pending_render.take() {
            let mut bytes = Vec::new();
            if self.synchronized_output_supported {
                bytes.extend_from_slice(SYNCHRONIZED_OUTPUT_START);
            }
            for transaction in batch.transactions {
                append_without_synchronization_markers(&mut bytes, &transaction.bytes);
            }
            if self.synchronized_output_supported {
                bytes.extend_from_slice(SYNCHRONIZED_OUTPUT_END);
            }
            self.active.push_back(ActiveTransaction {
                kind: ActiveTransactionKind::Render,
                bytes,
                offset: 0,
                completed_render: Some(batch.predicted),
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
        self.application_sync_started_ms = None;
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
        report
            .completed_renders
            .extend(
                self.awaiting_flush_renders
                    .drain(..)
                    .map(|predicted| CompletedRender {
                        geometry: predicted.geometry(),
                        predicted,
                    }),
            );
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

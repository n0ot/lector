use super::{
    application_accessibility::{
        ApplicationAccessibilityCommand, ApplicationAccessibilityPolicy,
        ApplicationAccessibilitySpeech, parse as parse_application_accessibility,
    },
    ext::{CellExt, ScreenExt},
    presentation::{
        AccessibilityEpoch, PaneMediaStore, PresentationError, PresentedHistoryBasis,
        PresentedHistoryDelta, PresentedViewFrame, SurfaceId, ViewId, ViewRevision,
    },
    terminal::{
        GhosttyEngine, GhosttyReviewMark, HistoryPosition, SemanticKind as Osc133Kind,
        SemanticMark as Osc133Mark, TerminalEngine, TerminalEvent, TerminalGeometry,
        TerminalSnapshot, UpdateSummary, Viewport,
    },
};
use std::{
    cmp::min,
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// A bounded history avoids unbounded memory growth while retaining enough
/// output for extended review and semantic-prompt navigation.
pub const SCROLLBACK_LINES: usize = 10_000;

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

// Exact parser evidence is retained only until its physical receipt is
// consumed. Backpressure must not turn that retention into an unbounded output
// log: beyond either limit accessibility safely falls back to snapshot diffing.
const ACCESSIBILITY_JOURNAL_MAX_ENTRIES: usize = 1_024;
const ACCESSIBILITY_JOURNAL_MAX_BYTES: usize = 1024 * 1024;
const PRESENTED_HISTORY_MAX_DELTA_DEPTH: usize = 256;
const PRESENTED_HISTORY_MAX_RETAINED_ROWS: usize = SCROLLBACK_LINES * 2;
const APPLICATION_SPEECH_MAX_ENTRIES: usize = 32;
const APPLICATION_SPEECH_MAX_BYTES: usize = 32 * 1024;

struct PendingApplicationSpeech {
    epoch_generation: u64,
    revision: ViewRevision,
    speech: ApplicationAccessibilitySpeech,
}

struct AccessibilityJournalEntry {
    epoch: AccessibilityEpoch,
    revision: ViewRevision,
    update: UpdateSummary,
    requires_snapshot_diff: bool,
    retained_bytes: usize,
}

#[derive(Clone, Copy)]
struct CompletedLinearRecordCache {
    epoch: AccessibilityEpoch,
    revision: ViewRevision,
    finalized_revision: ViewRevision,
    result: bool,
}

#[derive(Clone, Copy)]
struct ReviewSelection {
    scrollback: usize,
    cursor: (u16, u16),
}

#[derive(Clone, Copy)]
struct FallbackSemanticInput {
    prompt_count: usize,
    prompt_mark: Osc133Mark,
    position: HistoryPosition,
    frozen: bool,
}

enum AccessibilityReadState {
    Live,
    /// Accessibility keeps reading the last committed model while the parser
    /// and renderer continue to mutate the live Ghostty engine behind it.
    ///
    /// Without presentation tracking this is used only for an open
    /// synchronized-output transaction. With tracking enabled it also spans
    /// the interval between parsing any output and flushing its exact render.
    Frozen {
        screen: Box<TerminalSnapshot>,
        review_scrollback: usize,
        review_mark: Option<HistoryPosition>,
        review_mark_changed: bool,
        history_changed: bool,
    },
}

fn semantic_mark_summary(
    marks: &[Osc133Mark],
    alternate_screen: bool,
) -> (usize, Option<&Osc133Mark>) {
    marks
        .iter()
        .filter(|mark| mark.alternate_screen == alternate_screen)
        .fold((0, None), |(count, _), mark| {
            (count.saturating_add(1), Some(mark))
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HistoryState {
    revision: u64,
    basis: PresentedHistoryBasis,
}

impl HistoryState {
    fn from_snapshot_with_revision(snapshot: &TerminalSnapshot, revision: u64) -> Self {
        Self {
            revision,
            basis: PresentedHistoryBasis::from_snapshot(snapshot),
        }
    }

    fn from_delta(delta: &PresentedHistoryDelta) -> Self {
        Self {
            revision: delta.revision,
            basis: delta.basis,
        }
    }
}

/// Validates a receipt chain using only compact interval metadata. Doing this
/// before taking the committed deque makes malformed or obsolete receipts a
/// cheap, transactional rejection instead of requiring an O(history) backup.
fn validate_history_delta_chain(
    delta: &PresentedHistoryDelta,
    presented: HistoryState,
) -> Option<HistoryState> {
    validate_history_delta_chain_from(delta, presented.revision, presented)
}

fn validate_history_delta_chain_from(
    delta: &PresentedHistoryDelta,
    current_revision: u64,
    current: HistoryState,
) -> Option<HistoryState> {
    if delta.revision <= current_revision {
        return Some(current);
    }
    if delta.revision <= delta.base_revision {
        return None;
    }

    let target = HistoryState::from_delta(delta);
    let target_end = target.basis.end()?;
    if delta.replace_from < target.basis.origin
        || delta.replace_from > target_end
        || delta.rows.len() != target_end - delta.replace_from
    {
        return None;
    }

    if delta.full_replacement {
        return (delta.replace_from == target.basis.origin && delta.previous.is_none())
            .then_some(target);
    }

    let base = if delta.base_revision == current_revision {
        current
    } else {
        let previous = delta.previous.as_deref()?;
        validate_history_delta_chain_from(previous, current_revision, current)?
    };
    let base_end = base.basis.end()?;
    (base.revision == delta.base_revision
        && base.basis.screen == target.basis.screen
        && base.basis.geometry == target.basis.geometry
        && base.basis.origin <= target.basis.origin
        && target.basis.origin <= base_end
        && delta.replace_from == base_end
        && target_end >= base_end)
        .then_some(target)
}

/// Applies a chain already accepted by [`validate_history_delta_chain`]. Rows
/// are Arc-backed, so the only work proportional to history change is dropping
/// evicted deque entries and cloning the newly retained row handles.
fn apply_history_delta_chain(
    scrollback: &mut VecDeque<crate::terminal::Row>,
    delta: &PresentedHistoryDelta,
    presented: HistoryState,
) -> HistoryState {
    if delta.revision <= presented.revision {
        return presented;
    }

    let mut base = presented;
    if !delta.full_replacement && delta.base_revision != base.revision {
        base = apply_history_delta_chain(
            scrollback,
            delta
                .previous
                .as_deref()
                .expect("validated incremental delta has its missing base"),
            base,
        );
    }

    let target = HistoryState::from_delta(delta);
    apply_history_transition(
        scrollback,
        base,
        target,
        delta.replace_from,
        delta.full_replacement,
        delta.rows.iter().cloned(),
    );
    target
}

fn apply_history_transition(
    scrollback: &mut VecDeque<crate::terminal::Row>,
    base: HistoryState,
    target: HistoryState,
    replace_from: usize,
    full_replacement: bool,
    rows: impl IntoIterator<Item = crate::terminal::Row>,
) {
    if full_replacement {
        scrollback.clear();
    } else {
        debug_assert_eq!(base.basis.screen, target.basis.screen);
        debug_assert_eq!(base.basis.geometry, target.basis.geometry);
        let evicted = target.basis.origin - base.basis.origin;
        for _ in 0..evicted {
            let removed = scrollback.pop_front();
            debug_assert!(removed.is_some());
        }
        scrollback.truncate(replace_from - target.basis.origin);
    }
    scrollback.extend(rows);
    debug_assert_eq!(scrollback.len(), target.basis.extent);
}

/// Cuts a bounded receipt chain without asking Ghostty to decode the complete
/// history again. The committed deque plus the existing exact chain already
/// owns every immutable overlap row; only the newest suffix came from the
/// adapter. Started receipts keep their old Arcs, while the new root becomes
/// independently applicable from any presented generation.
fn compact_history_delta_root(
    committed: &VecDeque<crate::terminal::Row>,
    presented: HistoryState,
    previous: &PresentedHistoryDelta,
    base: HistoryState,
    target: HistoryState,
    replace_from: usize,
    rows: Vec<crate::terminal::Row>,
) -> Option<Arc<[crate::terminal::Row]>> {
    if committed.len() != presented.basis.extent {
        return None;
    }
    let validated = validate_history_delta_chain(previous, presented)?;
    if validated != base {
        return None;
    }
    let mut materialized = committed.clone();
    let applied = apply_history_delta_chain(&mut materialized, previous, presented);
    if applied != base {
        return None;
    }
    apply_history_transition(&mut materialized, base, target, replace_from, false, rows);
    Some(Arc::from(Vec::from(materialized)))
}

pub struct View {
    view_id: ViewId,
    presentation_tracking: bool,
    live_revision: ViewRevision,
    presented_revision: ViewRevision,
    finalized_presented_revision: ViewRevision,
    live_accessibility_epoch: AccessibilityEpoch,
    accessibility_epoch_floor_generation: u64,
    presented_accessibility_epoch: AccessibilityEpoch,
    presented_accessibility_evidence_revision: ViewRevision,
    presented_accessibility_evidence_exact: bool,
    presented_accessibility_requires_snapshot_diff: bool,
    live_application_accessibility_policy: ApplicationAccessibilityPolicy,
    presented_application_accessibility_policy: ApplicationAccessibilityPolicy,
    pending_application_speech: VecDeque<PendingApplicationSpeech>,
    pending_application_speech_bytes: usize,
    accessibility_journal: VecDeque<AccessibilityJournalEntry>,
    accessibility_journal_bytes: usize,
    accessibility_journal_gap_start: Option<ViewRevision>,
    accessibility_journal_discarded_through: ViewRevision,
    completed_linear_record_cache: Option<CompletedLinearRecordCache>,
    completed_linear_record_report: String,
    completed_linear_record_presented: String,
    live_revision_synchronized_output_closed: bool,
    presented_revision_synchronized_output_closed: bool,
    live_revision_cursor_restored: bool,
    presented_revision_cursor_restored: bool,
    live_history_revision: u64,
    presented_history_revision: u64,
    presented_history_basis: PresentedHistoryBasis,
    shared_live_history: Option<Arc<PresentedHistoryDelta>>,
    application_transaction_open: bool,
    unpresented_synchronized_output: bool,
    engine: GhosttyEngine,
    committed_snapshot: TerminalSnapshot,
    accessibility_read_state: AccessibilityReadState,
    media: PaneMediaStore,
    /// Cumulative parser metadata for standalone Views, where parsing is also
    /// the accessibility publication boundary. Presentation-tracked Views use
    /// the bounded revision journal and `presented_update` exclusively; keeping
    /// a second cumulative copy here would grow without bound under physical
    /// terminal backpressure.
    standalone_update: UpdateSummary,
    /// Parser metadata which is safe to use with the currently presented
    /// accessibility snapshot. Snapshot-diff fallbacks retain only fixed-size
    /// structural provenance; transient printed text remains excluded.
    presented_update: UpdateSummary,
    prev_screen: TerminalSnapshot,
    prev_screen_time: u128,
    review_cursor_position: (u16, u16),
    review_context_initialized: bool,
    saved_primary_review_selection: Option<ReviewSelection>,
    review_cursor_follow_pending: bool,
    review_cursor_screen_transition_pending: bool,
    accessibility_screen_transition_pending: bool,
    review_scrollback: usize,
    retained_history_len: usize,
    review_mark: Option<GhosttyReviewMark>,
    review_cursor_indent_level: u16,
    application_cursor_indent_level: u16,
    application_semantic_indent_level: u16,
    fallback_semantic_input: Option<FallbackSemanticInput>,
    cached_full: String,
    cached_prev_full: String,
    cached_full_valid: bool,
    cached_prev_full_valid: bool,
    cached_full_row_hashes: Vec<u64>,
    cached_prev_full_row_hashes: Vec<u64>,
    cached_document: String,
    cached_prev_document: String,
    cached_document_valid: bool,
    cached_prev_document_valid: bool,
    cached_document_row_hashes: Vec<u64>,
    cached_prev_document_row_hashes: Vec<u64>,
}

impl View {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::new_with_scrollback(rows, cols, SCROLLBACK_LINES)
    }

    fn new_with_scrollback(rows: u16, cols: u16, scrollback_lines: usize) -> Self {
        let mut engine =
            GhosttyEngine::new_with_scrollback(rows.max(1), cols.max(1), scrollback_lines)
                .unwrap_or_else(|error| {
                    panic!("could not create Ghostty terminal engine: {error}")
                });
        let committed_snapshot = engine.snapshot_with_history();
        let cursor_position = committed_snapshot.cursor_position();
        let prev_screen = committed_snapshot.clone();
        let presented_history_basis = PresentedHistoryBasis::from_snapshot(&committed_snapshot);
        View {
            view_id: ViewId(NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed)),
            presentation_tracking: false,
            live_revision: ViewRevision(0),
            presented_revision: ViewRevision(0),
            finalized_presented_revision: ViewRevision(0),
            live_accessibility_epoch: AccessibilityEpoch {
                generation: 1,
                start_revision: ViewRevision(0),
            },
            accessibility_epoch_floor_generation: 1,
            presented_accessibility_epoch: AccessibilityEpoch {
                generation: 1,
                start_revision: ViewRevision(0),
            },
            presented_accessibility_evidence_revision: ViewRevision(0),
            presented_accessibility_evidence_exact: true,
            presented_accessibility_requires_snapshot_diff: false,
            live_application_accessibility_policy: ApplicationAccessibilityPolicy::default(),
            presented_application_accessibility_policy: ApplicationAccessibilityPolicy::default(),
            pending_application_speech: VecDeque::new(),
            pending_application_speech_bytes: 0,
            accessibility_journal: VecDeque::new(),
            accessibility_journal_bytes: 0,
            accessibility_journal_gap_start: None,
            accessibility_journal_discarded_through: ViewRevision(0),
            completed_linear_record_cache: None,
            completed_linear_record_report: String::new(),
            completed_linear_record_presented: String::new(),
            live_revision_synchronized_output_closed: false,
            presented_revision_synchronized_output_closed: false,
            live_revision_cursor_restored: false,
            presented_revision_cursor_restored: false,
            live_history_revision: 0,
            presented_history_revision: 0,
            presented_history_basis,
            shared_live_history: None,
            application_transaction_open: false,
            unpresented_synchronized_output: false,
            engine,
            committed_snapshot,
            accessibility_read_state: AccessibilityReadState::Live,
            media: PaneMediaStore::new(Default::default()),
            standalone_update: UpdateSummary::default(),
            presented_update: UpdateSummary::default(),
            prev_screen,
            prev_screen_time: 0,
            review_cursor_position: cursor_position,
            review_context_initialized: false,
            saved_primary_review_selection: None,
            review_cursor_follow_pending: false,
            review_cursor_screen_transition_pending: false,
            accessibility_screen_transition_pending: false,
            review_scrollback: 0,
            retained_history_len: 0,
            review_mark: None,
            review_cursor_indent_level: 0,
            application_cursor_indent_level: 0,
            application_semantic_indent_level: 0,
            fallback_semantic_input: None,
            cached_full: String::new(),
            cached_prev_full: String::new(),
            cached_full_valid: false,
            cached_prev_full_valid: false,
            cached_full_row_hashes: Vec::new(),
            cached_prev_full_row_hashes: Vec::new(),
            cached_document: String::new(),
            cached_prev_document: String::new(),
            cached_document_valid: false,
            cached_prev_document_valid: false,
            cached_document_row_hashes: Vec::new(),
            cached_prev_document_row_hashes: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_scrollback_for_test(
        rows: u16,
        cols: u16,
        scrollback_lines: usize,
    ) -> Self {
        Self::new_with_scrollback(rows, cols, scrollback_lines)
    }

    /// Processes new changes, updating the internal screen representation
    pub fn process_changes(&mut self, buf: &[u8]) {
        let _ = self.process_changes_inner(buf, false, true);
    }

    /// Processes one parser batch and returns that batch's update summary.
    ///
    /// When `retain_for_accessibility` is true, print provenance moves into the
    /// accessibility owner instead of being cloned. Standalone views merge
    /// that subset into their pending summary; presentation-tracked views
    /// append it to the bounded receipt journal. The returned renderer/effect
    /// delta retains its damage rows but not that accessibility-only text.
    /// Callers which need only the immediate delta leave retention false, so
    /// shadow panes retain no cumulative vectors.
    pub(crate) fn process_changes_with_batch(
        &mut self,
        buf: &[u8],
        retain_for_accessibility: bool,
    ) -> UpdateSummary {
        self.process_changes_inner(buf, true, retain_for_accessibility)
            .expect("a requested terminal batch summary is always captured")
    }

    fn process_changes_inner(
        &mut self,
        buf: &[u8],
        capture_batch: bool,
        retain_for_accessibility: bool,
    ) -> Option<UpdateSummary> {
        if self.presentation_tracking {
            self.freeze_current_accessibility();
        }
        let old_review_scrollback = self.review_scrollback;
        let needs_live_snapshot_copy = !self.presentation_tracking || old_review_scrollback != 0;
        let old_history_origin = self.engine.snapshot().history_origin;
        let old_scrollback_extent = self.engine.snapshot().scrollback_extent;
        let old_live_snapshot = needs_live_snapshot_copy.then(|| self.engine.snapshot().clone());
        let was_synchronized = self.engine.snapshot().modes.synchronized_output;
        let accessible_scrollback_before = if self.presentation_tracking {
            0
        } else {
            self.scrollback()
        };
        let review_mark_before = if self.presentation_tracking {
            None
        } else {
            self.review_mark_position()
        };

        // Output is always interpreted against the live drawing screen. The
        // selected review viewport is restored afterward.
        self.engine.select_viewport(Viewport::Live);
        let mut update = TerminalEngine::advance(&mut self.engine, buf);
        let synchronized_output_open_snapshot =
            self.engine.take_synchronized_output_open_snapshot();
        let synchronized = update.synchronized_output;
        let synchronized_output_closed = update.synchronized_output_closed;
        let cursor_visibility_restored = update.cursor_visibility_restored;
        self.application_transaction_open = synchronized;
        let synchronized_transaction_activity =
            was_synchronized || synchronized || update.synchronized_output_opened;
        let live_history_origin = self.engine.snapshot().history_origin;
        let live_scrollback_extent = self.engine.snapshot().scrollback_extent;
        let live_snapshot = needs_live_snapshot_copy.then(|| self.engine.snapshot().clone());
        let screen_transition = update.screen_before != update.screen_after;
        let application_context_reset = screen_transition || update.terminal_reset;
        let batch_history_changed = update.history_changed
            || live_scrollback_extent != old_scrollback_extent
            || live_history_origin != old_history_origin
            || screen_transition;
        if self.presentation_tracking && batch_history_changed {
            self.live_history_revision = self
                .live_history_revision
                .checked_add(1)
                .expect("view history presentation revision exhausted");
        }
        if screen_transition {
            self.review_cursor_screen_transition_pending = true;
        }
        if self.presentation_tracking && application_context_reset {
            // A primary/alternate handoff or terminal reset is a new
            // accessibility context. Keep older journal entries alive for
            // already-captured receipts, but tag this update and every later
            // one with a fresh epoch so evidence cannot cross the boundary.
            self.begin_accessibility_epoch();
        }
        if application_context_reset {
            self.reset_application_accessibility();
        }
        let application_revision = if self.presentation_tracking {
            ViewRevision(self.live_revision.0.saturating_add(1))
        } else {
            self.live_revision
        };
        self.consume_application_accessibility(
            &mut update,
            retain_for_accessibility,
            application_revision,
        );
        // This boundary flag drives the scheduler for the just-observed PTY
        // batch; unlike damage and printed runs it must not stay sticky until
        // speech finalization.
        let mut accessibility_evidence = None;
        let batch_update = if capture_batch {
            if retain_for_accessibility {
                if self.presentation_tracking {
                    accessibility_evidence =
                        Some(take_normalized_accessibility_evidence(&mut update, true));
                    // The renderer owns this exact batch while the bounded
                    // journal owns the accessibility subset. Do not also retain
                    // a cumulative legacy summary while a receipt is pending.
                    Some(update)
                } else {
                    // Standalone callers have no later physical receipt. Keep
                    // their established cumulative summary contract, while the
                    // returned clone remains the exact renderer/effect batch.
                    let batch_update = update.clone();
                    update.operations.clear();
                    self.standalone_update.synchronized_output_opened = false;
                    self.standalone_update.merge(update);
                    Some(batch_update)
                }
            } else {
                // There is no accessibility consumer for this pane. Move the
                // renderer/effect facts to their immediate consumer, but drop
                // print provenance which neither consumer uses. Leave no
                // cumulative vectors behind.
                update.printed_runs.clear();
                update.linear_output_effect = crate::terminal::LinearOutputEffect::Preserve;
                self.standalone_update = UpdateSummary::default();
                Some(update)
            }
        } else {
            if self.presentation_tracking {
                accessibility_evidence =
                    Some(take_normalized_accessibility_evidence(&mut update, false));
            } else {
                self.standalone_update.synchronized_output_opened = false;
                self.standalone_update.merge(update);
            }
            None
        };
        if self.presentation_tracking {
            self.live_revision_synchronized_output_closed = synchronized_output_closed;
            self.live_revision_cursor_restored = cursor_visibility_restored;
            self.advance_live_revision();
            self.unpresented_synchronized_output |= synchronized_transaction_activity;
            if let Some(evidence) = accessibility_evidence {
                self.append_accessibility_evidence(evidence, synchronized_transaction_activity);
            } else {
                // A non-accessible shadow update deliberately has no retained
                // parser facts. Mark the hole so a later receipt can use the
                // exact snapshot but cannot mistake surrounding facts for a
                // complete summary.
                self.note_accessibility_evidence_gap(self.live_revision);
            }
        }
        // A repaint is not itself a cursor move. Full-screen applications
        // commonly redraw a spinner or prompt in place; treating every such
        // update as movement pulls a manually positioned review cursor back
        // to the application cursor. Compare the final live cursor with the
        // last finalized cursor so fragmented real moves still follow.
        if !synchronized {
            self.review_cursor_follow_pending = self.engine.snapshot().cursor_position()
                != self.prev_screen.cursor_position()
                || self.review_cursor_screen_transition_pending;
        }
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = if old_review_scrollback == 0 {
            0
        } else {
            translate_scrollback_offset(
                old_review_scrollback,
                old_live_snapshot
                    .as_ref()
                    .expect("review translation captured the old viewport"),
                live_snapshot
                    .as_ref()
                    .expect("review translation captured the live viewport"),
            )
        };
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));

        if self.presentation_tracking {
            // The scheduler owns the publication boundary. Parsing updates
            // only the live model; accessibility stays on the last frame
            // whose complete byte transaction reached the physical terminal.
            if let AccessibilityReadState::Frozen {
                history_changed, ..
            } = &mut self.accessibility_read_state
            {
                *history_changed |= batch_history_changed;
            }
            self.review_cursor_follow_pending = false;
        } else if synchronized {
            if let Some(snapshot) = synchronized_output_open_snapshot {
                let snapshot = self.hydrate_open_snapshot(snapshot);
                let checkpoint_follow_pending = snapshot.cursor_position()
                    != self.prev_screen.cursor_position()
                    || snapshot.screen != self.prev_screen.screen;
                let (review_scrollback, review_cursor_position) = translate_review_selection(
                    accessible_scrollback_before,
                    self.review_cursor_position,
                    &self.committed_snapshot,
                    &snapshot,
                );
                let review_mark = review_mark_before.and_then(|position| {
                    translate_history_position(position, &self.committed_snapshot, &snapshot)
                });
                let replaced_frozen_mark = matches!(
                    self.accessibility_read_state,
                    AccessibilityReadState::Frozen {
                        review_mark_changed: true,
                        ..
                    }
                );
                if replaced_frozen_mark {
                    self.review_mark = None;
                }
                self.committed_snapshot = snapshot;
                self.review_cursor_position = review_cursor_position;
                let screen = snapshot_at_scrollback(&self.committed_snapshot, review_scrollback);
                self.accessibility_read_state = AccessibilityReadState::Frozen {
                    screen: Box::new(screen),
                    review_scrollback,
                    review_mark: if replaced_frozen_mark {
                        None
                    } else {
                        review_mark
                    },
                    review_mark_changed: false,
                    history_changed: batch_history_changed,
                };
                // Only committed movement before the opening marker may pull
                // the review cursor. Movement after it belongs to the hidden
                // working frame and is ignored until the real close.
                self.review_cursor_follow_pending = checkpoint_follow_pending;
            } else if !was_synchronized
                && matches!(self.accessibility_read_state, AccessibilityReadState::Live)
            {
                // Every accepted false-to-true transition should carry the
                // adapter checkpoint. Keeping the previous committed model is
                // the conservative fallback if a future backend violates that
                // contract.
                let review_scrollback =
                    old_review_scrollback.min(self.committed_snapshot.scrollback_extent);
                let screen = snapshot_at_scrollback(&self.committed_snapshot, review_scrollback);
                let checkpoint_follow_pending = self.committed_snapshot.cursor_position()
                    != self.prev_screen.cursor_position()
                    || self.committed_snapshot.screen != self.prev_screen.screen;
                self.accessibility_read_state = AccessibilityReadState::Frozen {
                    screen: Box::new(screen),
                    review_scrollback,
                    review_mark: review_mark_before,
                    review_mark_changed: false,
                    history_changed: batch_history_changed,
                };
                self.review_cursor_follow_pending = checkpoint_follow_pending;
            } else if let AccessibilityReadState::Frozen {
                history_changed, ..
            } = &mut self.accessibility_read_state
            {
                *history_changed |= batch_history_changed;
            }
        } else if !matches!(self.accessibility_read_state, AccessibilityReadState::Live) {
            self.publish_synchronized_output(
                live_snapshot.expect("standalone view captured its live snapshot"),
                batch_history_changed,
            );
        } else {
            let live_snapshot = live_snapshot.expect("standalone view captured its live snapshot");
            let (review_scrollback, review_cursor_position) = translate_review_selection(
                old_review_scrollback,
                self.review_cursor_position,
                &self.committed_snapshot,
                &live_snapshot,
            );
            self.review_scrollback = review_scrollback;
            self.review_cursor_position = review_cursor_position;
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
            self.update_committed_snapshot(live_snapshot, batch_history_changed);
        }
        self.invalidate_visible_cache();
        // If the screen's size changed, the cursor may now be out of bounds.
        let review_cursor_position = self.review_cursor_position;
        let (rows, cols) = self.size();
        let max_row = rows.saturating_sub(1);
        let max_col = cols.saturating_sub(1);
        self.review_cursor_position = (
            min(review_cursor_position.0, max_row),
            min(review_cursor_position.1, max_col),
        );

        // If the review cursor moved,
        // it's because the screen was resized.
        // Clear the mark, because it's probably not where you'd expect it.
        if review_cursor_position != self.review_cursor_position {
            self.clear_review_mark();
        }
        if !self.presentation_tracking {
            self.refresh_fallback_semantic_input();
        }
        batch_update
    }

    /// Advances the previous screen to match the current one,
    /// and sets its update time to now
    pub fn finalize_changes(&mut self, now_ms: u128) {
        let frozen = matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        );
        if (frozen && !self.presentation_tracking)
            || (self.presentation_tracking && !self.accessibility_has_unfinalized_presentation())
        {
            return;
        }
        if frozen {
            // The parser may already be drawing a newer frame. Advance the
            // speech baseline only to the snapshot whose render receipt has
            // completed, never to that newer live engine state.
            self.prev_screen = self.committed_snapshot.clone();
        } else {
            let visible_offset = self.review_scrollback;
            self.engine.select_viewport(Viewport::Live);
            self.prev_screen = self.engine.snapshot_with_history();
            self.engine
                .select_viewport(Viewport::Scrollback(visible_offset));
        }
        self.prev_screen_time = now_ms;
        self.completed_linear_record_cache = None;
        self.standalone_update = UpdateSummary::default();
        self.presented_update = UpdateSummary::default();
        if self.presentation_tracking {
            self.finalized_presented_revision = self.presented_revision;
            self.presented_revision_synchronized_output_closed = false;
            self.presented_revision_cursor_restored = false;
            self.presented_accessibility_evidence_revision = self.presented_revision;
            self.presented_accessibility_evidence_exact = true;
            self.presented_accessibility_requires_snapshot_diff = false;
            self.discard_accessibility_journal_through(self.presented_revision);
        }
        self.review_cursor_follow_pending = !frozen && self.review_cursor_screen_transition_pending;
        if self.cached_full_valid {
            self.cached_prev_full.clone_from(&self.cached_full);
            self.cached_prev_full_valid = true;
            self.cached_prev_full_row_hashes
                .clone_from(&self.cached_full_row_hashes);
        } else {
            self.cached_prev_full_valid = false;
            self.cached_prev_full_row_hashes.clear();
        }
        if self.cached_document_valid {
            self.cached_prev_document.clone_from(&self.cached_document);
            self.cached_prev_document_valid = true;
            self.cached_prev_document_row_hashes
                .clone_from(&self.cached_document_row_hashes);
        } else {
            self.cached_prev_document_valid = false;
            self.cached_prev_document_row_hashes.clear();
        }
    }

    /// Gets the current screen backing this view
    pub fn screen(&self) -> &TerminalSnapshot {
        match &self.accessibility_read_state {
            AccessibilityReadState::Frozen { screen, .. } => screen,
            AccessibilityReadState::Live => self.engine.snapshot(),
        }
    }

    /// Gets the mutable parser's current screen, bypassing the committed
    /// accessibility snapshot. Protocol handling and physical presentation
    /// use this; screen-reading commands intentionally use [`Self::screen`].
    pub(crate) fn live_screen(&self) -> &TerminalSnapshot {
        self.engine.snapshot()
    }

    /// Geometry belongs to the mutable terminal endpoint, even while
    /// accessibility intentionally retains an older committed frame.
    pub(crate) fn live_size(&self) -> (u16, u16) {
        self.live_screen().size()
    }

    /// Makes accessibility publication follow successful physical-terminal
    /// flushes instead of parser progress.
    ///
    /// The application enables this once for views which participate in its
    /// render scheduler. Standalone views intentionally retain their immediate
    /// parser-to-accessibility behavior.
    pub(crate) fn enable_presentation_tracking(&mut self) {
        if self.presentation_tracking {
            return;
        }

        self.presentation_tracking = true;
        // Any metadata accumulated before the scheduler took ownership is
        // represented by the committed snapshot established below. It must not
        // survive as an unbounded parallel log.
        self.standalone_update = UpdateSummary::default();
        self.reset_accessibility_journal_for_handoff();
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            // A synchronized transaction was already open. Its working model
            // is one revision ahead of the accessibility snapshot which was
            // visible when tracking took ownership.
            self.advance_live_revision();
            self.live_history_revision = 1;
            self.shared_live_history = None;
            self.unpresented_synchronized_output = true;
            self.append_accessibility_evidence(UpdateSummary::default(), true);
        } else {
            self.committed_snapshot = self.live_snapshot_with_history();
        }
        self.presented_history_basis =
            PresentedHistoryBasis::from_snapshot(&self.committed_snapshot);
        self.presented_revision = ViewRevision(0);
    }

    pub(crate) fn view_id(&self) -> ViewId {
        self.view_id
    }

    /// Revision which already existed when a screen-relative input intent was
    /// dispatched. A later physical receipt must advance past this boundary
    /// before it can confirm that intent; otherwise an older queued frame
    /// could be mistaken for the application's response.
    pub(crate) fn input_intent_revision_boundary(&self) -> Option<ViewRevision> {
        self.presentation_tracking.then_some(self.live_revision)
    }

    /// Revision currently visible on the physical terminal. Standalone Views
    /// do not use presentation receipts and deliberately have no revision
    /// gate.
    pub(crate) fn accessibility_revision(&self) -> Option<ViewRevision> {
        self.presentation_tracking
            .then_some(self.presented_revision)
    }

    /// Captures the exact authoritative model associated with the render
    /// currently being enqueued. The scheduler carries this frame alongside
    /// the render bytes, so replacement and backpressure cannot accidentally
    /// publish a newer model when an older transaction completes.
    pub(crate) fn capture_live_presentation_frame(
        &mut self,
        surface_id: SurfaceId,
    ) -> PresentedViewFrame {
        debug_assert!(self.presentation_tracking);
        let snapshot = self.with_live_screen(|view| view.live_screen().clone());
        let history_basis = PresentedHistoryBasis::from_snapshot(&snapshot);
        let history = self.presentation_history_if_changed();
        PresentedViewFrame {
            view_id: self.view_id,
            revision: self.live_revision,
            surface_id,
            snapshot,
            history_revision: self.live_history_revision,
            history_basis,
            history,
            accessibility_epoch: self.live_accessibility_epoch,
            application_auto_read_suppressed: self
                .live_application_accessibility_policy
                .suppress_auto_read,
            application_cursor_tracking_suppressed: self
                .live_application_accessibility_policy
                .suppress_cursor_tracking,
            synchronized_output_closed: self.live_revision_synchronized_output_closed,
            cursor_visibility_restored: self.live_revision_cursor_restored,
        }
    }

    /// Captures the committed viewport used by a compositor transition while
    /// the mutable application model is held. This keeps accessibility
    /// geometry and cropping aligned with the pixels in that transition
    /// without publishing the application's newer working revision.
    pub(crate) fn capture_committed_presentation_frame(
        &mut self,
        surface_id: SurfaceId,
    ) -> PresentedViewFrame {
        debug_assert!(self.presentation_tracking);
        let history_basis = self.presented_history_basis;
        PresentedViewFrame {
            view_id: self.view_id,
            revision: self.presented_revision,
            surface_id,
            snapshot: self.committed_presentation_snapshot(),
            history_revision: self.presented_history_revision,
            history_basis,
            history: None,
            accessibility_epoch: self.presented_accessibility_epoch,
            application_auto_read_suppressed: self
                .presented_application_accessibility_policy
                .suppress_auto_read,
            application_cursor_tracking_suppressed: self
                .presented_application_accessibility_policy
                .suppress_cursor_tracking,
            synchronized_output_closed: false,
            cursor_visibility_restored: false,
        }
    }

    /// Publishes a model only after the render carrying it has completely
    /// flushed. Returns `false` for a frame routed to the wrong view, a frame
    /// from the future, or an obsolete duplicate.
    #[cfg(test)]
    pub(crate) fn apply_presented_frame(&mut self, frame: PresentedViewFrame) -> bool {
        if !self.can_apply_presented_frame(&frame) {
            return false;
        }
        self.install_presented_frame(frame)
    }

    /// Applies a borrowed receipt only when it belongs to this view and can
    /// advance its presented state. Routing code uses this boundary so rejected
    /// candidates never clone a full terminal snapshot.
    pub(crate) fn apply_presented_frame_ref(&mut self, frame: &PresentedViewFrame) -> bool {
        if !self.can_apply_presented_frame(frame) {
            return false;
        }
        self.install_presented_frame(frame.clone())
    }

    fn can_apply_presented_frame(&self, frame: &PresentedViewFrame) -> bool {
        if !self.presentation_tracking
            || frame.view_id != self.view_id
            || frame.revision > self.live_revision
            || frame.revision < self.presented_revision
        {
            return false;
        }
        frame.history_revision >= self.presented_history_revision
            && (frame.history_revision == self.presented_history_revision
                || frame
                    .history
                    .as_ref()
                    .is_some_and(|history| history.revision == frame.history_revision))
    }

    fn install_presented_frame(&mut self, frame: PresentedViewFrame) -> bool {
        let caught_up = frame.revision == self.live_revision;
        let accessibility_epoch = frame.accessibility_epoch;
        let frame_revision = frame.revision;
        let mut snapshot = frame.snapshot;
        if snapshot.screen != frame.history_basis.screen
            || snapshot.history_origin != frame.history_basis.origin
            || snapshot.scrollback_extent != frame.history_basis.extent
            // A committed compositor transition may fit the old viewport to
            // live geometry without reflowing its committed history. Every
            // advancing application frame must use one geometry for both.
            || (frame.revision != self.presented_revision
                && snapshot.geometry != frame.history_basis.geometry)
        {
            return false;
        }
        if frame.history_revision == self.presented_history_revision {
            if frame.history_basis != self.presented_history_basis {
                return false;
            }
            snapshot.scrollback = std::mem::take(&mut self.committed_snapshot.scrollback);
        } else {
            let history = frame
                .history
                .expect("a validated newer history revision carries its rows");
            let presented = HistoryState::from_snapshot_with_revision(
                &self.committed_snapshot,
                self.presented_history_revision,
            );
            let Some(target) = validate_history_delta_chain(&history, presented) else {
                return false;
            };
            if target.revision != frame.history_revision || target.basis != frame.history_basis {
                return false;
            }
            snapshot.scrollback = std::mem::take(&mut self.committed_snapshot.scrollback);
            let applied = apply_history_delta_chain(&mut snapshot.scrollback, &history, presented);
            debug_assert_eq!(applied, target);
            self.presented_history_revision = frame.history_revision;
            self.presented_history_basis = target.basis;
        }
        self.presented_revision = frame.revision;
        self.completed_linear_record_cache = None;
        let accessibility_epoch_is_current =
            accessibility_epoch.generation >= self.accessibility_epoch_floor_generation;
        self.presented_revision_synchronized_output_closed =
            accessibility_epoch_is_current && frame.synchronized_output_closed;
        self.presented_revision_cursor_restored =
            accessibility_epoch_is_current && frame.cursor_visibility_restored;
        self.presented_application_accessibility_policy = if accessibility_epoch_is_current {
            ApplicationAccessibilityPolicy {
                suppress_auto_read: frame.application_auto_read_suppressed,
                suppress_cursor_tracking: frame.application_cursor_tracking_suppressed,
            }
        } else {
            ApplicationAccessibilityPolicy::default()
        };
        self.install_presented_snapshot(snapshot, caught_up);
        self.refresh_fallback_semantic_input();
        self.publish_accessibility_evidence(accessibility_epoch, frame_revision);
        if self.unpresented_synchronized_output {
            // Exact journal entries from an atomic transaction already require
            // snapshot diffing. Keep the parser-ahead marker set until the
            // receipt catches up so no later partial generation is mistaken for
            // an ordinary print stream.
            if caught_up {
                self.unpresented_synchronized_output = false;
            }
        }
        if self
            .shared_live_history
            .as_ref()
            .is_some_and(|history| history.revision <= self.presented_history_revision)
        {
            self.shared_live_history = None;
        }
        true
    }

    /// Whether the application currently has DEC private mode 2026 open.
    /// This is parser state, deliberately independent from accessibility's
    /// wait for a render receipt.
    pub(crate) fn application_transaction_open(&self) -> bool {
        self.application_transaction_open
    }

    /// Whether parsed state exists which has not yet been physically flushed
    /// and therefore must not be exposed through accessibility.
    pub(crate) fn accessibility_awaiting_presentation(&self) -> bool {
        self.presentation_tracking && self.presented_revision != self.live_revision
    }

    /// Whether the physical terminal has completed a newer frame which has
    /// not yet crossed the speech-diff finalization boundary.
    pub(crate) fn accessibility_has_unfinalized_presentation(&self) -> bool {
        self.presentation_tracking && self.finalized_presented_revision != self.presented_revision
    }

    /// Whether the exact unfinalized frame which reached the physical terminal
    /// ended at an application-declared atomic commit boundary.
    pub(crate) fn accessibility_presentation_synchronized_output_closed(&self) -> bool {
        self.accessibility_has_unfinalized_presentation()
            && self.presented_revision_synchronized_output_closed
    }

    /// Returns the live viewport from the last committed application frame.
    ///
    /// This is distinct from [`Self::screen`]: a user may navigate the frozen
    /// accessibility snapshot into scrollback while a compositor transition
    /// still needs the committed live viewport which was visible before the
    /// application opened its synchronized-output transaction.
    pub(crate) fn committed_presentation_snapshot(&mut self) -> TerminalSnapshot {
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            let mut snapshot = snapshot_at_scrollback(&self.committed_snapshot, 0);
            fit_snapshot_to_geometry(&mut snapshot, self.live_screen().geometry);
            snapshot
        } else {
            self.with_live_screen(|view| view.live_screen().clone())
        }
    }

    pub(crate) fn holds_synchronized_output(&self) -> bool {
        self.application_transaction_open()
    }

    pub fn snapshot_with_history(&mut self) -> TerminalSnapshot {
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            self.committed_snapshot.clone()
        } else {
            self.engine.snapshot_with_history()
        }
    }

    fn publish_synchronized_output(
        &mut self,
        live_snapshot: TerminalSnapshot,
        batch_history_changed: bool,
    ) {
        let history_changed = self.finish_frozen_accessibility_state();
        self.accessibility_read_state = AccessibilityReadState::Live;
        self.update_committed_snapshot(live_snapshot, history_changed || batch_history_changed);
        // Printed runs describe every write the application made while its
        // transaction was open, including text overwritten before commit.
        // Auto-read must compare the two committed snapshots instead of
        // speaking that transient stream.
        self.standalone_update.printed_runs.clear();
        self.invalidate_visible_cache();
    }

    pub(crate) fn application_accessibility_policy(&self) -> ApplicationAccessibilityPolicy {
        if self.presentation_tracking {
            self.presented_application_accessibility_policy
        } else {
            self.live_application_accessibility_policy
        }
    }

    /// Takes semantic speech whose owning view and presentation revision are
    /// currently active. Callers deliberately discard this result when global
    /// automatic reading is disabled or the terminal is unfocused; messages
    /// are never replayed after focus returns.
    pub(crate) fn take_presented_application_speech(
        &mut self,
    ) -> Vec<ApplicationAccessibilitySpeech> {
        let through_revision = if self.presentation_tracking {
            self.presented_revision
        } else {
            self.live_revision
        };
        let epoch_generation = if self.presentation_tracking {
            self.presented_accessibility_epoch.generation
        } else {
            self.live_accessibility_epoch.generation
        };
        let mut speech = Vec::new();
        while self
            .pending_application_speech
            .front()
            .is_some_and(|pending| {
                pending.epoch_generation < epoch_generation
                    || pending.epoch_generation == epoch_generation
                        && pending.revision <= through_revision
            })
        {
            let pending = self
                .pending_application_speech
                .pop_front()
                .expect("front was present");
            self.pending_application_speech_bytes = self
                .pending_application_speech_bytes
                .saturating_sub(pending.speech.text.len());
            if pending.epoch_generation == epoch_generation {
                speech.push(pending.speech);
            }
        }
        speech
    }

    fn consume_application_accessibility(
        &mut self,
        update: &mut UpdateSummary,
        retain_speech: bool,
        revision: ViewRevision,
    ) {
        let mut unclaimed = Vec::with_capacity(update.effects.events.len());
        for event in std::mem::take(&mut update.effects.events) {
            let command = match &event {
                TerminalEvent::UnknownSequence { content, truncated } => {
                    parse_application_accessibility(content, *truncated)
                }
                _ => None,
            };
            let Some(command) = command else {
                unclaimed.push(event);
                continue;
            };
            match command {
                ApplicationAccessibilityCommand::Set(policy) => {
                    self.live_application_accessibility_policy = policy;
                }
                ApplicationAccessibilityCommand::Speak(speech) if retain_speech => {
                    self.push_application_speech(revision, speech);
                }
                ApplicationAccessibilityCommand::Speak(_) => {}
                ApplicationAccessibilityCommand::End => {
                    self.live_application_accessibility_policy =
                        ApplicationAccessibilityPolicy::default();
                }
            }
        }
        update.effects.events = unclaimed;
    }

    fn push_application_speech(
        &mut self,
        revision: ViewRevision,
        speech: ApplicationAccessibilitySpeech,
    ) {
        self.pending_application_speech_bytes = self
            .pending_application_speech_bytes
            .saturating_add(speech.text.len());
        self.pending_application_speech
            .push_back(PendingApplicationSpeech {
                epoch_generation: self.live_accessibility_epoch.generation,
                revision,
                speech,
            });
        while self.pending_application_speech.len() > APPLICATION_SPEECH_MAX_ENTRIES
            || self.pending_application_speech_bytes > APPLICATION_SPEECH_MAX_BYTES
        {
            let Some(discarded) = self.pending_application_speech.pop_front() else {
                break;
            };
            self.pending_application_speech_bytes = self
                .pending_application_speech_bytes
                .saturating_sub(discarded.speech.text.len());
        }
    }

    fn reset_application_accessibility(&mut self) {
        self.live_application_accessibility_policy = ApplicationAccessibilityPolicy::default();
        self.pending_application_speech.clear();
        self.pending_application_speech_bytes = 0;
    }

    pub(crate) fn application_semantic_indentation_changed(&mut self, level: u16) -> bool {
        let changed = level != self.application_semantic_indent_level;
        self.application_semantic_indent_level = level;
        changed
    }

    fn advance_live_revision(&mut self) {
        self.live_revision = ViewRevision(
            self.live_revision
                .0
                .checked_add(1)
                .expect("view presentation revision exhausted"),
        );
    }

    fn begin_accessibility_epoch(&mut self) {
        self.live_accessibility_epoch = AccessibilityEpoch {
            generation: self
                .live_accessibility_epoch
                .generation
                .checked_add(1)
                .expect("view accessibility epoch exhausted"),
            start_revision: self.live_revision,
        };
    }

    fn reset_accessibility_journal_for_handoff(&mut self) {
        if self.live_revision > self.finalized_presented_revision {
            self.note_accessibility_evidence_gap(ViewRevision(
                self.finalized_presented_revision.0.saturating_add(1),
            ));
            self.accessibility_journal_discarded_through = self.live_revision;
        }
        self.completed_linear_record_cache = None;
        self.accessibility_journal.clear();
        self.accessibility_journal_bytes = 0;
        self.begin_accessibility_epoch();
        self.accessibility_epoch_floor_generation = self.live_accessibility_epoch.generation;
        self.presented_accessibility_epoch = self.live_accessibility_epoch;
        self.presented_accessibility_evidence_revision = self.presented_revision;
        self.presented_accessibility_evidence_exact = true;
        self.presented_accessibility_requires_snapshot_diff = false;
    }

    fn append_accessibility_evidence(
        &mut self,
        update: UpdateSummary,
        requires_snapshot_diff: bool,
    ) {
        let retained_bytes = accessibility_evidence_retained_bytes(&update);
        if retained_bytes > ACCESSIBILITY_JOURNAL_MAX_BYTES {
            // The snapshot remains exact, but no later parser report can be
            // considered complete until a receipt through this missing record
            // becomes the finalized diff baseline.
            self.note_accessibility_evidence_gap(self.live_revision);
            return;
        }

        self.accessibility_journal_bytes = self
            .accessibility_journal_bytes
            .saturating_add(retained_bytes);
        self.accessibility_journal
            .push_back(AccessibilityJournalEntry {
                epoch: self.live_accessibility_epoch,
                revision: self.live_revision,
                update,
                requires_snapshot_diff,
                retained_bytes,
            });
        while self.accessibility_journal.len() > ACCESSIBILITY_JOURNAL_MAX_ENTRIES
            || self.accessibility_journal_bytes > ACCESSIBILITY_JOURNAL_MAX_BYTES
        {
            let Some(discarded) = self.accessibility_journal.pop_front() else {
                break;
            };
            self.accessibility_journal_bytes = self
                .accessibility_journal_bytes
                .saturating_sub(discarded.retained_bytes);
            self.note_accessibility_evidence_gap(discarded.revision);
        }
    }

    fn note_accessibility_evidence_gap(&mut self, revision: ViewRevision) {
        self.accessibility_journal_gap_start = Some(
            self.accessibility_journal_gap_start
                .map_or(revision, |start| start.min(revision)),
        );
        self.accessibility_journal_discarded_through =
            self.accessibility_journal_discarded_through.max(revision);
    }

    fn publish_accessibility_evidence(
        &mut self,
        epoch: AccessibilityEpoch,
        revision: ViewRevision,
    ) {
        if epoch.generation < self.accessibility_epoch_floor_generation {
            // An ownership handoff invalidated this parser context after its
            // frame was captured. The physical snapshot may still flush, but
            // its old facts must never move the accessibility epoch backwards.
            self.presented_update = UpdateSummary::default();
            return;
        }
        if self.presented_accessibility_epoch != epoch {
            self.presented_accessibility_epoch = epoch;
            self.presented_accessibility_evidence_revision = epoch.start_revision;
            self.presented_accessibility_evidence_exact = true;
            self.presented_accessibility_requires_snapshot_diff = false;
            self.presented_update = UpdateSummary::default();
        }

        let required_after = self
            .presented_accessibility_evidence_revision
            .max(epoch.start_revision);
        if revision < required_after {
            // A committed compositor transition may deliberately carry an
            // older snapshot after an ownership reset. It has no parser facts
            // in the new context and must not revive the previous summary.
            self.presented_update = UpdateSummary::default();
            self.presented_accessibility_evidence_revision = revision;
            return;
        }
        let discarded_in_required_range =
            self.accessibility_journal_gap_start.is_some_and(|start| {
                start <= revision && self.accessibility_journal_discarded_through > required_after
            });
        self.presented_accessibility_evidence_exact &= !discarded_in_required_range;
        if !self.presented_accessibility_evidence_exact {
            self.presented_update = snapshot_diff_provenance(&self.presented_update);
        }

        while self
            .accessibility_journal
            .front()
            .is_some_and(|entry| entry.revision <= revision)
        {
            let entry = self
                .accessibility_journal
                .pop_front()
                .expect("journal front was checked");
            self.accessibility_journal_bytes = self
                .accessibility_journal_bytes
                .saturating_sub(entry.retained_bytes);
            if entry.epoch != epoch || entry.revision <= required_after {
                continue;
            }
            self.presented_accessibility_requires_snapshot_diff |= entry.requires_snapshot_diff;
            if self.presented_accessibility_evidence_exact
                && !self.presented_accessibility_requires_snapshot_diff
            {
                self.presented_update.merge(entry.update);
            } else {
                if entry.requires_snapshot_diff {
                    self.presented_update = snapshot_diff_provenance(&self.presented_update);
                }
                self.presented_update
                    .merge(snapshot_diff_provenance(&entry.update));
            }
        }
        self.presented_accessibility_evidence_revision = revision;
    }

    fn discard_accessibility_journal_through(&mut self, revision: ViewRevision) {
        while self
            .accessibility_journal
            .front()
            .is_some_and(|entry| entry.revision <= revision)
        {
            let entry = self
                .accessibility_journal
                .pop_front()
                .expect("journal front was checked");
            self.accessibility_journal_bytes = self
                .accessibility_journal_bytes
                .saturating_sub(entry.retained_bytes);
        }
        if let Some(gap_start) = self.accessibility_journal_gap_start
            && revision >= gap_start
        {
            if revision >= self.accessibility_journal_discarded_through {
                self.accessibility_journal_gap_start = None;
            } else {
                self.accessibility_journal_gap_start =
                    Some(ViewRevision(revision.0.saturating_add(1)));
            }
        }
    }

    fn live_snapshot_with_history(&mut self) -> TerminalSnapshot {
        let visible_offset = self.review_scrollback;
        self.engine.select_viewport(Viewport::Live);
        let snapshot = self.engine.snapshot_with_history();
        self.engine
            .select_viewport(Viewport::Scrollback(visible_offset));
        snapshot
    }

    fn presentation_history_if_changed(&mut self) -> Option<Arc<PresentedHistoryDelta>> {
        if self.live_history_revision == self.presented_history_revision {
            return None;
        }
        if let Some(history) = &self.shared_live_history
            && history.revision == self.live_history_revision
        {
            return Some(Arc::clone(history));
        }

        let live = self.engine.snapshot();
        let target = HistoryState {
            revision: self.live_history_revision,
            basis: PresentedHistoryBasis::from_snapshot(live),
        };
        let base = self
            .shared_live_history
            .as_deref()
            .map(HistoryState::from_delta)
            .unwrap_or(HistoryState {
                revision: self.presented_history_revision,
                basis: self.presented_history_basis,
            });
        let base_end = base.basis.end();
        let target_end = target.basis.end();
        let append_compatible = target.basis.extent != 0
            && base.basis.screen == target.basis.screen
            && base.basis.geometry == target.basis.geometry
            && base.basis.origin <= target.basis.origin
            && base_end.is_some_and(|end| target.basis.origin <= end)
            && base_end
                .zip(target_end)
                .is_some_and(|(base_end, target_end)| target_end >= base_end);
        let incremental_rows = base_end
            .zip(target_end)
            .filter(|_| append_compatible)
            .map_or(target.basis.extent, |(base_end, target_end)| {
                target_end - base_end
            });
        let compact = append_compatible
            && self.shared_live_history.as_ref().is_some_and(|history| {
                history.depth >= PRESENTED_HISTORY_MAX_DELTA_DEPTH
                    || history.retained_rows.saturating_add(incremental_rows)
                        > PRESENTED_HISTORY_MAX_RETAINED_ROWS
            });
        let incremental_replace_from = if append_compatible {
            base_end.expect("append-compatible history has a finite end")
        } else {
            target.basis.origin
        };
        let logical_start = incremental_replace_from
            .checked_sub(target.basis.origin)
            .expect("history replacement begins within the target window");
        let row_suffix = self
            .engine
            .normalized_history_rows_from(logical_start)
            .unwrap_or_else(|error| panic!("Ghostty history delta failed: {error}"));
        debug_assert_eq!(
            row_suffix.len(),
            target
                .basis
                .end()
                .and_then(|end| end.checked_sub(incremental_replace_from))
                .expect("history delta interval must be representable"),
            "Ghostty history suffix must match its advertised logical interval"
        );
        let (full_replacement, replace_from, rows) =
            if compact {
                let presented = HistoryState {
                    revision: self.presented_history_revision,
                    basis: self.presented_history_basis,
                };
                let compacted =
                    compact_history_delta_root(
                        &self.committed_snapshot.scrollback,
                        presented,
                        self.shared_live_history
                            .as_deref()
                            .expect("compaction requires an existing history chain"),
                        base,
                        target,
                        incremental_replace_from,
                        row_suffix,
                    )
                    .unwrap_or_else(|| {
                        Arc::from(self.engine.normalized_history_rows_from(0).unwrap_or_else(
                            |error| panic!("Ghostty history compaction fallback failed: {error}"),
                        ))
                    });
                (true, target.basis.origin, compacted)
            } else {
                (
                    !append_compatible,
                    incremental_replace_from,
                    Arc::from(row_suffix),
                )
            };
        let previous = (!full_replacement)
            .then(|| self.shared_live_history.as_ref().map(Arc::clone))
            .flatten();
        let depth = previous.as_ref().map_or(1, |history| history.depth + 1);
        let retained_rows = previous
            .as_ref()
            .map_or(rows.len(), |history| history.retained_rows + rows.len());
        let history = Arc::new(PresentedHistoryDelta {
            revision: target.revision,
            base_revision: if full_replacement {
                self.presented_history_revision
            } else {
                base.revision
            },
            basis: target.basis,
            replace_from,
            rows,
            full_replacement,
            previous,
            depth,
            retained_rows,
        });
        self.shared_live_history = Some(Arc::clone(&history));
        Some(history)
    }

    fn freeze_current_accessibility(&mut self) {
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            return;
        }

        let screen = self.engine.snapshot().clone();
        let review_scrollback = self.review_scrollback;
        let review_mark = self.review_mark_position();
        // `committed_snapshot` was installed by the last render receipt (or
        // when tracking was enabled) and already owns the same full history.
        // Re-cloning up to the scrollback limit on every PTY batch would make
        // presentation tracking needlessly proportional to history size.
        self.accessibility_read_state = AccessibilityReadState::Frozen {
            screen: Box::new(screen),
            review_scrollback,
            review_mark,
            review_mark_changed: false,
            history_changed: false,
        };
    }

    fn install_presented_snapshot(&mut self, snapshot: TerminalSnapshot, caught_up: bool) {
        let old_presented_cursor = self.committed_snapshot.cursor_position();
        let old_presented_screen = self.committed_snapshot.screen;
        let state = std::mem::replace(
            &mut self.accessibility_read_state,
            AccessibilityReadState::Live,
        );
        let (review_scrollback, review_mark, review_mark_changed, history_changed) = match state {
            AccessibilityReadState::Frozen {
                review_scrollback,
                review_mark,
                review_mark_changed,
                history_changed,
                ..
            } => (
                review_scrollback,
                review_mark,
                review_mark_changed,
                history_changed,
            ),
            AccessibilityReadState::Live => (
                self.review_scrollback,
                self.review_mark_position(),
                false,
                false,
            ),
        };
        let (review_scrollback, review_cursor_position) = translate_review_selection(
            review_scrollback,
            self.review_cursor_position,
            &self.committed_snapshot,
            &snapshot,
        );
        let review_mark = review_mark.and_then(|position| {
            translate_history_position(position, &self.committed_snapshot, &snapshot)
        });
        let history_changed = history_changed
            || snapshot.scrollback_extent != self.committed_snapshot.scrollback_extent
            || snapshot.history_origin != self.committed_snapshot.history_origin;
        self.committed_snapshot = snapshot;
        self.review_cursor_position = review_cursor_position;

        if review_mark_changed {
            self.review_mark = None;
        }
        if caught_up {
            self.review_scrollback = review_scrollback.min(self.engine.scrollback_extent());
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
            self.accessibility_read_state = AccessibilityReadState::Live;
            self.review_cursor_follow_pending = self.committed_snapshot.cursor_position()
                != old_presented_cursor
                || self.committed_snapshot.screen != old_presented_screen
                || self.review_cursor_screen_transition_pending;
        } else {
            let screen = snapshot_at_scrollback(&self.committed_snapshot, review_scrollback);
            self.accessibility_read_state = AccessibilityReadState::Frozen {
                screen: Box::new(screen),
                review_scrollback,
                review_mark: if review_mark_changed {
                    None
                } else {
                    review_mark
                },
                review_mark_changed,
                history_changed,
            };
            // This cursor belongs to the frame which is now physically
            // visible even though the parser has moved on. Follow consecutive
            // presented frames, never cursor hints from the newer live model.
            self.review_cursor_follow_pending = self.committed_snapshot.cursor_position()
                != old_presented_cursor
                || self.committed_snapshot.screen != old_presented_screen;
        }

        let old_review_cursor = self.review_cursor_position;
        let (rows, cols) = self.size();
        self.review_cursor_position = (
            min(old_review_cursor.0, rows.saturating_sub(1)),
            min(old_review_cursor.1, cols.saturating_sub(1)),
        );
        if old_review_cursor != self.review_cursor_position {
            self.clear_review_mark();
        }
        self.invalidate_visible_cache();
    }

    fn finish_frozen_accessibility_state(&mut self) -> bool {
        let state = std::mem::replace(
            &mut self.accessibility_read_state,
            AccessibilityReadState::Live,
        );
        let AccessibilityReadState::Frozen {
            review_scrollback,
            review_mark,
            review_mark_changed,
            history_changed,
            ..
        } = state
        else {
            self.accessibility_read_state = state;
            return false;
        };

        let live_snapshot = self.engine.snapshot().clone();
        let (review_scrollback, review_cursor_position) = translate_review_selection(
            review_scrollback,
            self.review_cursor_position,
            &self.committed_snapshot,
            &live_snapshot,
        );
        self.review_scrollback = review_scrollback;
        self.review_cursor_position = review_cursor_position;
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));

        if review_mark_changed {
            // A mark created against the frozen transaction snapshot cannot
            // be retroactively attached to that cell in the already-mutated
            // Ghostty grid. It remains usable throughout the transaction and
            // is deliberately cleared when the new frame commits.
            let _ = review_mark;
            self.review_mark = None;
        }
        history_changed
    }

    fn hydrate_open_snapshot(&mut self, mut snapshot: TerminalSnapshot) -> TerminalSnapshot {
        if snapshot.scrollback.len() == snapshot.scrollback_extent {
            return snapshot;
        }

        debug_assert_eq!(
            self.committed_snapshot.scrollback.len(),
            snapshot.scrollback_extent,
            "the adapter must include history when it changes before a synchronized-output opener"
        );
        debug_assert_eq!(
            self.committed_snapshot.history_origin, snapshot.history_origin,
            "the adapter must include history when its lineage advances before a synchronized-output opener"
        );
        let expected_extent = snapshot.scrollback_extent;
        if self.committed_snapshot.scrollback.len() == snapshot.scrollback_extent
            && self.committed_snapshot.history_origin == snapshot.history_origin
            && self.committed_snapshot.screen == snapshot.screen
        {
            snapshot.scrollback = std::mem::take(&mut self.committed_snapshot.scrollback);
        }
        // Preserve safe addressing if a future backend violates the
        // debug-checked boundary contract.
        snapshot.scrollback_extent = snapshot.scrollback.len();
        if snapshot.scrollback_extent != expected_extent {
            snapshot.history_origin = snapshot
                .history_origin
                .saturating_add(expected_extent.saturating_sub(snapshot.scrollback_extent));
        }
        snapshot
    }

    fn update_committed_snapshot(
        &mut self,
        mut live_snapshot: TerminalSnapshot,
        history_changed: bool,
    ) {
        if !history_changed
            && self.committed_snapshot.scrollback.len() == live_snapshot.scrollback_extent
            && self.committed_snapshot.history_origin == live_snapshot.history_origin
            && self.committed_snapshot.screen == live_snapshot.screen
        {
            live_snapshot.scrollback = std::mem::take(&mut self.committed_snapshot.scrollback);
        }
        // When history changed, retain only the cheap visible state here. The
        // adapter remembers that history is dirty across PTY batches and
        // supplies one coherent full-history copy at the next real opener.
        self.committed_snapshot = live_snapshot;
    }

    fn set_accessible_scrollback(&mut self, scrollback: usize) {
        if let AccessibilityReadState::Frozen {
            review_scrollback, ..
        } = &mut self.accessibility_read_state
        {
            *review_scrollback = scrollback.min(self.committed_snapshot.scrollback_extent);
            self.refresh_frozen_screen();
        } else {
            self.review_scrollback = scrollback.min(self.retained_history_len);
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
        }
        self.invalidate_visible_cache();
    }

    fn refresh_frozen_screen(&mut self) {
        let scrollback = match self.accessibility_read_state {
            AccessibilityReadState::Frozen {
                review_scrollback, ..
            } => review_scrollback,
            AccessibilityReadState::Live => return,
        };
        let refreshed = snapshot_at_scrollback(&self.committed_snapshot, scrollback);
        if let AccessibilityReadState::Frozen { screen, .. } = &mut self.accessibility_read_state {
            **screen = refreshed;
        }
    }

    pub(crate) fn presentation_media(&mut self) -> Result<&PaneMediaStore, PresentationError> {
        let placements = self.engine.kitty_image_placements()?;
        self.media.synchronize(&placements)?;
        Ok(&self.media)
    }

    /// Runs work against the live drawing screen, then returns to the review
    /// viewport. Screen diffing and application-cursor tracking must not read
    /// whichever historical page the review cursor happens to be on.
    pub(crate) fn with_live_screen<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let restore_frozen_review = matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen {
                review_scrollback,
                ..
            } if review_scrollback != 0
        );
        if restore_frozen_review {
            let presented_live = snapshot_at_scrollback(&self.committed_snapshot, 0);
            if let AccessibilityReadState::Frozen { screen, .. } =
                &mut self.accessibility_read_state
            {
                **screen = presented_live;
            }
            self.invalidate_visible_cache();
        }
        self.engine.select_viewport(Viewport::Live);
        let result = f(self);
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = self.review_scrollback.min(history_len);
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        if restore_frozen_review {
            self.refresh_frozen_screen();
            self.invalidate_visible_cache();
        }
        result
    }

    pub fn scrollback(&self) -> usize {
        match self.accessibility_read_state {
            AccessibilityReadState::Frozen {
                review_scrollback, ..
            } => review_scrollback,
            AccessibilityReadState::Live => self.review_scrollback,
        }
    }

    pub fn scrollback_len(&self) -> usize {
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            self.committed_snapshot.scrollback_extent
        } else {
            self.retained_history_len
        }
    }

    pub fn review_cursor_position(&self) -> (u16, u16) {
        self.review_cursor_position
    }

    pub(crate) fn review_cursor_follow_pending(&self) -> bool {
        self.review_cursor_follow_pending
    }

    pub(crate) fn set_review_cursor_position(&mut self, position: (u16, u16)) {
        self.review_cursor_position = position;
    }

    pub(crate) fn follow_application_cursor(&mut self) {
        let frozen = matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        );
        if let AccessibilityReadState::Frozen {
            review_scrollback, ..
        } = &mut self.accessibility_read_state
        {
            *review_scrollback = 0;
            self.review_cursor_position = self.committed_snapshot.cursor_position();
            self.refresh_frozen_screen();
        } else {
            self.review_scrollback = 0;
            self.engine.select_viewport(Viewport::Live);
            self.review_cursor_position = self.engine.snapshot().cursor_position();
        }
        self.invalidate_visible_cache();
        let cursor_line_has_text = self
            .screen()
            .rows
            .get(usize::from(self.review_cursor_position.0))
            .is_some_and(|row| {
                row.cells.iter().any(|cell| {
                    cell.contents()
                        .chars()
                        .any(|character| !character.is_whitespace())
                })
            });
        if !frozen && cursor_line_has_text {
            self.review_cursor_screen_transition_pending = false;
        }
        self.review_cursor_follow_pending = !frozen && self.review_cursor_screen_transition_pending;
    }

    /// Selects the review position belonging to the context which is becoming
    /// active. A new context starts at its application cursor. Returning to an
    /// existing overlay, pane, or primary screen preserves that context's
    /// independent review position instead of overwriting it during the view
    /// announcement.
    pub(crate) fn prepare_review_cursor_for_activation(&mut self) -> ((u16, u16), (u16, u16)) {
        let old = self.review_cursor_position;
        let previous_screen = self.prev_screen().screen;
        let current_screen = self.screen().screen;

        if previous_screen != current_screen {
            match (previous_screen, current_screen) {
                (
                    crate::terminal::ScreenIdentity::Primary,
                    crate::terminal::ScreenIdentity::Alternate,
                ) => {
                    self.saved_primary_review_selection = Some(ReviewSelection {
                        scrollback: self.scrollback(),
                        cursor: self.review_cursor_position,
                    });
                    self.follow_application_cursor();
                }
                (
                    crate::terminal::ScreenIdentity::Alternate,
                    crate::terminal::ScreenIdentity::Primary,
                ) => {
                    if let Some(selection) = self.saved_primary_review_selection.take() {
                        self.set_accessible_scrollback(selection.scrollback);
                        let (rows, cols) = self.size();
                        self.review_cursor_position = (
                            selection.cursor.0.min(rows.saturating_sub(1)),
                            selection.cursor.1.min(cols.saturating_sub(1)),
                        );
                    } else {
                        self.follow_application_cursor();
                    }
                }
                _ => self.follow_application_cursor(),
            }
            self.clear_review_mark();
            self.review_context_initialized = true;
            self.cancel_pending_screen_transition_follow();
        } else if !self.review_context_initialized {
            self.follow_application_cursor();
            self.review_context_initialized = true;
            self.cancel_pending_screen_transition_follow();
        } else {
            // Output received while another view was active may have queued a
            // follow. Reactivating this already-initialized context restores
            // its saved review position; later visible output can queue a new
            // follow in the usual way.
            self.review_cursor_follow_pending = false;
        }

        (old, self.review_cursor_position)
    }

    pub(crate) fn mark_review_context_active(&mut self) {
        self.review_context_initialized = true;
    }

    pub(crate) fn cancel_pending_screen_transition_follow(&mut self) {
        self.review_cursor_screen_transition_pending = false;
        self.review_cursor_follow_pending = false;
    }

    pub(crate) fn accessibility_screen_transition_pending(&self) -> bool {
        self.accessibility_screen_transition_pending
    }

    pub(crate) fn defer_accessibility_screen_transition(&mut self) {
        self.accessibility_screen_transition_pending = true;
    }

    pub(crate) fn complete_accessibility_screen_transition(&mut self) {
        self.accessibility_screen_transition_pending = false;
    }

    pub(crate) fn set_review_cursor_row(&mut self, row: u16) {
        self.review_cursor_position.0 = row;
    }

    pub(crate) fn set_review_cursor_col(&mut self, col: u16) {
        self.review_cursor_position.1 = col;
    }

    pub fn review_history_position(&self) -> HistoryPosition {
        HistoryPosition {
            row: self
                .scrollback_len()
                .saturating_sub(self.scrollback())
                .saturating_add(usize::from(self.review_cursor_position.0)),
            col: self.review_cursor_position.1,
        }
    }

    fn current_history_position(&self) -> HistoryPosition {
        self.review_history_position()
    }

    #[cfg(test)]
    pub(crate) fn set_review_history_position(&mut self, position: HistoryPosition) {
        let history_len = self.scrollback_len();
        let last_row = usize::from(self.size().0.saturating_sub(1));
        let max_history_row = history_len.saturating_add(last_row);
        let target_row = position.row.min(max_history_row);
        let current_start = history_len.saturating_sub(self.scrollback());
        let current_end = current_start.saturating_add(last_row);

        let visible_start = if target_row < current_start {
            target_row
        } else if target_row > current_end {
            target_row.saturating_sub(last_row)
        } else {
            current_start
        };
        let scrollback = history_len.saturating_sub(visible_start);
        if let AccessibilityReadState::Frozen {
            review_scrollback, ..
        } = &mut self.accessibility_read_state
        {
            *review_scrollback = scrollback;
            self.refresh_frozen_screen();
        } else {
            self.review_scrollback = scrollback;
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
        }
        self.invalidate_visible_cache();
        self.review_cursor_position = (
            target_row.saturating_sub(visible_start) as u16,
            position.col.min(self.size().1.saturating_sub(1)),
        );
    }

    pub(crate) fn set_review_mark(&mut self) {
        let position = self.review_history_position();
        if let AccessibilityReadState::Frozen {
            review_mark,
            review_mark_changed,
            ..
        } = &mut self.accessibility_read_state
        {
            *review_mark = Some(position);
            *review_mark_changed = true;
            return;
        }
        self.review_mark = Some(
            self.engine
                .track_review_mark(position)
                .unwrap_or_else(|error| panic!("could not track Ghostty review mark: {error}")),
        );
    }

    pub(crate) fn review_mark_position(&self) -> Option<HistoryPosition> {
        if let AccessibilityReadState::Frozen { review_mark, .. } = &self.accessibility_read_state {
            return *review_mark;
        }
        self.review_mark.as_ref().and_then(|mark| {
            self.engine
                .review_mark_position(mark)
                .unwrap_or_else(|error| panic!("could not resolve Ghostty review mark: {error}"))
        })
    }

    pub(crate) fn update_summary(&self) -> &UpdateSummary {
        &self.standalone_update
    }

    /// Update metadata paired with [`Self::screen`]. This differs from
    /// [`Self::update_summary`] while the parser is ahead of the physical
    /// terminal: renderer/protocol consumers need the live summary, while
    /// accessibility must never consume hints from an unpresented frame.
    pub(crate) fn accessibility_update_summary(&self) -> &UpdateSummary {
        if self.presentation_tracking {
            &self.presented_update
        } else {
            &self.standalone_update
        }
    }

    /// Whether the currently accessible, physically presented update is a
    /// complete append-only output record. The parallel print observer only
    /// supplies provenance; Ghostty's resulting snapshot validates that the
    /// reported text actually exists in the presented terminal history/grid.
    pub(crate) fn accessibility_completes_linear_output_record(&mut self) -> bool {
        let cache_key = self.presentation_tracking.then_some((
            self.presented_accessibility_epoch,
            self.presented_revision,
            self.finalized_presented_revision,
        ));
        if let (Some((epoch, revision, finalized_revision)), Some(cached)) =
            (cache_key, self.completed_linear_record_cache)
            && cached.epoch == epoch
            && cached.revision == revision
            && cached.finalized_revision == finalized_revision
        {
            return cached.result;
        }

        let result = self.validate_completed_linear_output_record();
        if let Some((epoch, revision, finalized_revision)) = cache_key {
            self.completed_linear_record_cache = Some(CompletedLinearRecordCache {
                epoch,
                revision,
                finalized_revision,
                result,
            });
        }
        result
    }

    fn validate_completed_linear_output_record(&mut self) -> bool {
        if self.presentation_tracking && !self.accessibility_has_unfinalized_presentation() {
            return false;
        }
        if self.screen().screen != crate::terminal::ScreenIdentity::Primary
            || self.prev_screen.screen != crate::terminal::ScreenIdentity::Primary
        {
            return false;
        }

        let mut reported = std::mem::take(&mut self.completed_linear_record_report);
        let update = self.accessibility_update_summary();
        if !update.completes_linear_output_record()
            || update.linear_output_text_into(&mut reported).is_none()
        {
            self.completed_linear_record_report = reported;
            return false;
        }
        if reported.trim_end_matches('\n').is_empty() {
            self.completed_linear_record_report = reported;
            return true;
        }

        let mut presented = std::mem::take(&mut self.completed_linear_record_presented);
        presented.clear();
        let result = {
            let reported = reported.trim_end_matches('\n');
            let snapshot = self.screen();
            let columns = usize::from(snapshot.size().1.max(1));
            // A completed record is necessarily at the live tail. Include the
            // whole visible grid plus only enough recent history to cover the
            // reported bytes, explicit line boundaries, and a small cursor/wrap
            // margin. Scanning all retained history for every streamed line would
            // turn long-running output into quadratic work.
            let history_rows = reported
                .len()
                .div_ceil(columns)
                .saturating_add(reported.matches('\n').count())
                .saturating_add(4);
            let history_start = snapshot.scrollback.len().saturating_sub(history_rows);
            // The qualifying stream ends in a real LF, so its completed record
            // is strictly before the post-update cursor. At the bottom margin
            // that row either moved upward or entered scrollback. Including the
            // cursor row could let unrelated stale text there validate a bad
            // repaint.
            let visible_rows_before_cursor =
                usize::from(snapshot.cursor.row).min(snapshot.rows.len());
            for row in snapshot
                .scrollback
                .iter()
                .skip(history_start)
                .chain(snapshot.rows.iter().take(visible_rows_before_cursor))
            {
                row.append_contents_to(&mut presented);
                if !row.wrapped {
                    presented.push('\n');
                }
            }
            let presented = presented.trim_end_matches('\n');
            // A completed record must be the exact live tail, not merely text
            // found somewhere nearby. Tail anchoring rejects stale suffixes and
            // later stale matches such as writing `ab` over `abab`.
            if !presented.ends_with(reported) {
                false
            } else {
                let start = presented.len().saturating_sub(reported.len());
                let line_start = presented[..start].rfind('\n').map_or(0, |index| index + 1);
                presented[line_start..].find(reported) == Some(start.saturating_sub(line_start))
            }
        };
        self.completed_linear_record_report = reported;
        self.completed_linear_record_presented = presented;
        result
    }

    /// A view used as a shadow model may observe output without owning the
    /// application's PTY. Its terminal replies are useful to a real terminal
    /// owner but must not remain pending in an observational surface.
    pub(crate) fn discard_shadow_pty_replies(&mut self) {
        self.standalone_update.pty_replies.clear();
    }

    /// Pane-scoped terminal side effects observed since the last finalized
    /// screen update. Borrowed callback data has already been copied into
    /// owned, normalized values before reaching this model.
    pub fn terminal_events(&self) -> &[crate::terminal::TerminalEvent] {
        &self.standalone_update.effects.events
    }

    pub(crate) fn clear_update_summary(&mut self) {
        self.standalone_update = UpdateSummary::default();
        self.presented_update = UpdateSummary::default();
        if self.presentation_tracking {
            self.reset_accessibility_journal_for_handoff();
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_renderer_damage_hints(&mut self) {
        self.standalone_update.changed_rows.clear();
        self.standalone_update.damage = crate::terminal::TerminalDamage::None;
    }

    pub(crate) fn set_previous_screen_time(&mut self, time: u128) {
        self.prev_screen_time = time;
    }

    /// Gets the previous screen backing this view
    pub fn prev_screen(&self) -> &TerminalSnapshot {
        &self.prev_screen
    }
    /// Gets the size of this view
    pub fn size(&self) -> (u16, u16) {
        self.screen().size()
    }

    /// Update future application colour-query replies without changing any
    /// visible cells or accessibility/presentation revisions.
    pub(crate) fn set_virtual_terminal_colors(
        &mut self,
        colors: crate::terminal_protocol::VirtualTerminalColors,
    ) {
        self.engine.set_virtual_terminal_colors(colors);
    }

    /// Resizes this view
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.set_size_with_geometry(TerminalGeometry::from_cells(rows, cols));
    }

    /// Resizes this view while preserving per-cell pixel geometry for
    /// size-query and image consumers.
    pub fn set_size_with_geometry(&mut self, geometry: TerminalGeometry) {
        let geometry = TerminalGeometry::new(
            geometry.rows,
            geometry.cols,
            geometry.cell_width_px,
            geometry.cell_height_px,
        );
        if self.presentation_tracking {
            self.freeze_current_accessibility();
        }
        TerminalEngine::resize_with_geometry(&mut self.engine, geometry);
        if self.presentation_tracking {
            self.advance_live_revision();
            self.live_history_revision = self
                .live_history_revision
                .checked_add(1)
                .expect("view history presentation revision exhausted");
            self.shared_live_history = None;
        }
        let history_len = self.engine.scrollback_extent();
        self.retained_history_len = history_len;
        self.review_scrollback = self.review_scrollback.min(history_len);
        self.engine
            .select_viewport(Viewport::Scrollback(self.review_scrollback));
        if let AccessibilityReadState::Frozen {
            history_changed, ..
        } = &mut self.accessibility_read_state
        {
            // Resize/reflow mutates only the working model. The committed
            // transaction snapshot retains its original rows until close.
            *history_changed = true;
        } else {
            self.engine.select_viewport(Viewport::Live);
            let live_snapshot = self.engine.snapshot().clone();
            self.update_committed_snapshot(live_snapshot, true);
            self.engine
                .select_viewport(Viewport::Scrollback(self.review_scrollback));
        }
        self.invalidate_visible_cache();
        // When publication is deferred, the old physical geometry remains
        // navigable until the resize render flushes. The completed frame will
        // clamp the review cursor against its newly presented geometry.
        if self.presentation_tracking {
            return;
        }

        // If the screen's size changed, the cursor may now be out of bounds.
        let review_cursor_position = self.review_cursor_position;
        let max_row = geometry.rows.saturating_sub(1);
        let max_col = geometry.cols.saturating_sub(1);
        self.review_cursor_position = (
            min(self.review_cursor_position.0, max_row),
            min(self.review_cursor_position.1, max_col),
        );
        if review_cursor_position != self.review_cursor_position {
            self.clear_review_mark();
        }
    }

    fn clear_review_mark(&mut self) {
        self.review_mark = None;
        if let AccessibilityReadState::Frozen {
            review_mark,
            review_mark_changed,
            ..
        } = &mut self.accessibility_read_state
        {
            *review_mark = None;
            *review_mark_changed = true;
        }
    }

    fn invalidate_visible_cache(&mut self) {
        self.cached_full_valid = false;
        self.cached_full_row_hashes.clear();
        self.cached_document_valid = false;
        self.cached_document_row_hashes.clear();
    }

    /// Gets the indentation level of the line under the review cursor,
    /// and whether it's changed since the last time this method was called.
    pub fn review_cursor_indentation_level(&mut self) -> (u16, bool) {
        let indent_level = self
            .screen()
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                self.review_cursor_position.0,
                0,
                self.review_cursor_position.0,
                self.size().1 - 1,
            )
            .map_or(self.review_cursor_indent_level, |(_, col)| col);

        let changed = indent_level != self.review_cursor_indent_level;
        self.review_cursor_indent_level = indent_level;
        (indent_level, changed)
    }

    /// Gets the indentation level of the line under the application cursor,
    /// and whether it's changed since the last time this method was called.
    pub fn application_cursor_indentation_level(&mut self) -> (u16, bool) {
        let indent_level = self
            .screen()
            .find_cell(
                |c| !c.contents().is_empty() && !c.contents().chars().all(char::is_whitespace),
                self.screen().cursor_position().0,
                0,
                self.screen().cursor_position().0,
                self.size().1 - 1,
            )
            .map_or(self.application_cursor_indent_level, |(_, col)| col);

        let changed = indent_level != self.application_cursor_indent_level;
        self.application_cursor_indent_level = indent_level;
        (indent_level, changed)
    }

    /// Moves the review cursor up a line within the currently visible viewport.
    /// Review commands never select a different scrollback viewport; the frozen
    /// Review overlay owns document-level history navigation.
    /// If skip_blank_lines is true,
    /// the review cursor will move up to the previous non blank line,
    /// or remain in place if this is the first non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_up(&mut self, skip_blank_lines: bool) -> bool {
        if self.review_cursor_position.0 == 0 {
            return false;
        }
        if !skip_blank_lines {
            self.review_cursor_position.0 -= 1;
            return true;
        }

        let row = self.review_cursor_position.0;
        let last_col = self.size().1 - 1;
        self.review_cursor_position.0 = self
            .screen()
            .rfind_cell(CellExt::is_in_word, 0, 0, row - 1, last_col)
            .map_or(row, |(row, _)| row);

        self.review_cursor_position.0 != row
    }

    /// Moves the review cursor down a line within the currently visible viewport.
    /// Review commands never select a different scrollback viewport; the frozen
    /// Review overlay owns document-level history navigation.
    /// If skip_blank_lines is true,
    /// the review cursor will move down to the next non blank line,
    /// or remain in place if this is the last non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_down(&mut self, skip_blank_lines: bool) -> bool {
        let last_row = self.size().0 - 1;
        if self.review_cursor_position.0 == last_row {
            return false;
        }
        if !skip_blank_lines {
            self.review_cursor_position.0 += 1;
            return true;
        }

        let row = self.review_cursor_position.0;
        let last_col = self.size().1 - 1;
        self.review_cursor_position.0 = self
            .screen()
            .find_cell(CellExt::is_in_word, row + 1, 0, last_row, last_col)
            .map_or(row, |(row, _)| row);

        self.review_cursor_position.0 != row
    }

    pub fn osc133_marks(&self) -> &[Osc133Mark] {
        &self.screen().semantic_marks
    }

    fn active_semantic_input_start(&self) -> Option<HistoryPosition> {
        let alternate_screen = self.screen().alternate_screen();
        let latest_mark = self
            .osc133_marks()
            .iter()
            .rev()
            .find(|mark| mark.alternate_screen == alternate_screen)?;
        matches!(latest_mark.kind, Osc133Kind::InputStart).then_some(latest_mark.position)
    }

    fn refresh_fallback_semantic_input(&mut self) {
        let alternate_screen = self.screen().alternate_screen();
        let (prompt_count, latest_mark) =
            semantic_mark_summary(&self.screen().semantic_marks, alternate_screen);
        let Some(prompt_mark) = latest_mark.copied() else {
            self.fallback_semantic_input = None;
            return;
        };
        if !matches!(prompt_mark.kind, Osc133Kind::PromptStart) {
            self.fallback_semantic_input = None;
            return;
        }

        let (row, col) = self.screen().cursor_position();
        let position = HistoryPosition {
            row: self.scrollback_len() + usize::from(row),
            col,
        };
        match &mut self.fallback_semantic_input {
            Some(fallback)
                if fallback.prompt_count == prompt_count && fallback.prompt_mark == prompt_mark =>
            {
                if !fallback.frozen {
                    fallback.position = position;
                }
            }
            fallback => {
                *fallback = Some(FallbackSemanticInput {
                    prompt_count,
                    prompt_mark,
                    position,
                    frozen: false,
                });
            }
        }
    }

    /// Freezes an inferred input boundary before application-caused output can
    /// move the cursor. Some shell integrations emit OSC 133 A but omit B on
    /// later prompts; until the first forwarded input, each presented prompt
    /// fragment can safely advance the inferred end of that prompt.
    pub(crate) fn note_forwarded_application_input(&mut self) {
        if let Some(fallback) = &mut self.fallback_semantic_input {
            fallback.frozen = true;
        }
    }

    /// Whether a screen-relative cursor position is at or before the active
    /// OSC 133 input boundary. Readline leaves the prompt immediately before
    /// this boundary, so Backspace must not treat a prompt cell as editable
    /// input when the command line is empty (including queued Backspaces
    /// whose earlier echoes have not reached presentation yet).
    pub(crate) fn position_at_or_before_active_semantic_input(
        &self,
        (row, col): (u16, u16),
    ) -> bool {
        let input_start = self.active_semantic_input_start().or_else(|| {
            self.fallback_semantic_input
                .map(|fallback| fallback.position)
        });
        let Some(input_start) = input_start else {
            return false;
        };
        HistoryPosition {
            row: self.scrollback_len() + usize::from(row),
            col,
        } <= input_start
    }

    /// Returns the most recently submitted command line delimited by OSC 133
    /// B/C, excluding the prompt. This describes submitted input, not a
    /// transient Readline history selection that has not been executed.
    pub fn last_submitted_input(&mut self) -> Option<String> {
        let marks = self.osc133_marks();
        let alternate_screen = self.screen().alternate_screen();
        let command_index = marks.iter().rposition(|mark| {
            mark.alternate_screen == alternate_screen
                && matches!(mark.kind, Osc133Kind::CommandStart)
        })?;
        let command_start = marks[command_index].position;
        let prompt_index = marks[..command_index].iter().rposition(|mark| {
            mark.alternate_screen == alternate_screen
                && matches!(mark.kind, Osc133Kind::PromptStart)
        });
        let input_start = marks[prompt_index.map_or(0, |index| index + 1)..command_index]
            .iter()
            .rfind(|mark| {
                mark.alternate_screen == alternate_screen
                    && matches!(mark.kind, Osc133Kind::InputStart)
            })?
            .position;
        let mut input = self.contents_between_history(input_start, command_start)?;
        while input.ends_with(['\r', '\n']) {
            input.pop();
        }
        Some(input)
    }

    /// Returns the currently displayed editable input when the latest OSC 133
    /// phase is B (input). Bash does not emit another B for each Readline
    /// history selection, but the original marker remains a reliable prompt
    /// boundary while Up/Down redraw the text after it.
    pub fn active_semantic_input(&mut self) -> Option<String> {
        let input_start = self.active_semantic_input_start()?;
        let (cursor_row, cursor_col) = self.screen().cursor_position();
        let last_col = self.size().1.saturating_sub(1);
        let input_end_col = self
            .screen()
            .rfind_cell(
                |cell| !cell.contents().is_empty(),
                cursor_row,
                cursor_col,
                cursor_row,
                last_col,
            )
            .map_or(cursor_col, |(_, col)| col.saturating_add(1));
        let cursor = HistoryPosition {
            row: self.scrollback_len() + usize::from(cursor_row),
            col: input_end_col,
        };
        let mut input = self.contents_between_history(input_start, cursor)?;
        while matches!(input.chars().last(), Some('\r' | '\n')) {
            input.pop();
        }
        Some(input)
    }

    fn contents_between_history(
        &mut self,
        start: HistoryPosition,
        end: HistoryPosition,
    ) -> Option<String> {
        if start > end {
            return None;
        }
        if matches!(
            self.accessibility_read_state,
            AccessibilityReadState::Frozen { .. }
        ) {
            return contents_between_snapshot_history(&self.committed_snapshot, start, end);
        }
        let saved_offset = self.review_scrollback;
        let cols = self.size().1;
        let mut contents = String::new();

        for absolute_row in start.row..=end.row {
            let (offset, visible_row) = if absolute_row < self.retained_history_len {
                (self.retained_history_len - absolute_row, 0)
            } else {
                let row = absolute_row - self.retained_history_len;
                if row >= usize::from(self.size().0) {
                    self.engine
                        .select_viewport(Viewport::Scrollback(saved_offset));
                    return None;
                }
                (0, row as u16)
            };
            self.engine.select_viewport(Viewport::Scrollback(offset));
            let start_col = if absolute_row == start.row {
                start.col
            } else {
                0
            };
            let end_col = if absolute_row == end.row {
                end.col
            } else {
                cols
            };
            for col in start_col..end_col.min(cols) {
                contents.push_str(
                    self.screen()
                        .cell(visible_row, col)
                        .map_or("", crate::terminal::Cell::contents),
                );
            }
            if absolute_row != end.row && !self.screen().row_wrapped(visible_row) {
                contents.push('\n');
            }
        }

        self.engine
            .select_viewport(Viewport::Scrollback(saved_offset));
        Some(contents)
    }

    pub(crate) fn copy_review_selection(&mut self, mark: HistoryPosition) -> Option<String> {
        let cursor = self.current_history_position();
        if mark > cursor {
            return None;
        }
        self.contents_between_history(
            mark,
            HistoryPosition {
                row: cursor.row,
                col: cursor.col.saturating_add(1).min(self.size().1),
            },
        )
    }

    /// Moves the cursor to the start of the previous word,
    /// or the beginning of the line if the cursor is in or before the first word.
    /// This method will return true only if the cursor moved to a different word.
    pub fn review_cursor_prev_word(&mut self) -> bool {
        let (row, col) = self.review_cursor_position;
        // First, find the beginning of this word.
        let col = self.screen().find_word_start(row, col);
        if col == 0 {
            // The current word was the first.
            // Just move to the beginning of the line.
            self.review_cursor_position.1 = 0;
            return false;
        }

        // Now, find the start of the previous word and move to it.
        let col = self.screen().find_word_start(row, col - 1);
        self.review_cursor_position.1 = col;
        true
    }

    /// Moves the cursor to the start of the next word,
    /// or the end of the line if the cursor is in or past the last word.
    /// This method will return true only if the cursor moved to a different word.
    pub fn review_cursor_next_word(&mut self) -> bool {
        let last = self.size().1 - 1;
        let (row, col) = self.review_cursor_position;
        // First, find the end of this word.
        let col = self.screen().find_word_end(row, col);
        if col >= last {
            // The current word was the last.
            return false;
        }

        self.review_cursor_position.1 = col + 1;
        true
    }

    /// Moves the review cursor left a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_left(&mut self) -> bool {
        if self.review_cursor_position.1 == 0 {
            return false;
        }
        if let Some((row, col)) = self.screen().rfind_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            0,
            self.review_cursor_position.0,
            self.review_cursor_position.1 - 1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }

    /// Moves the review cursor right a column.
    /// If the next cell continues a wide character, it will be skipped.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_right(&mut self) -> bool {
        if self.review_cursor_position.1 >= self.size().1 - 1 {
            return false;
        }

        if let Some((row, col)) = self.screen().find_cell(
            |c| !c.is_wide_continuation(),
            self.review_cursor_position.0,
            self.review_cursor_position.1 + 1,
            self.review_cursor_position.0,
            self.size().1 - 1,
        ) {
            self.review_cursor_position = (row, col);
            true
        } else {
            false
        }
    }

    /// Returns the entire line at the specified row.
    pub fn line(&self, row: u16) -> String {
        self.screen().contents_between(row, 0, row, self.size().1)
    }

    /// Returns the word at the specified coordinates.
    pub fn word(&self, row: u16, col: u16) -> String {
        let start = self.screen().find_word_start(row, col);
        let end = self.screen().find_word_end(row, col);
        self.screen().contents_between(row, start, row, end + 1)
    }

    /// Returns the character at the specified coordinates.
    pub fn character(&self, row: u16, col: u16) -> String {
        self.screen().contents_between(row, col, row, col + 1)
    }

    /// Returns the contents of the full screen, including blank lines.
    pub fn contents_full(&self) -> String {
        self.screen().contents_full()
    }

    /// Writes the contents of the full screen, including blank lines, into `out`.
    pub fn contents_full_into(&self, out: &mut String) {
        self.screen().contents_full_into(out);
    }

    pub fn full_contents_cached(&mut self) -> (&str, &str, &[u64], &[u64]) {
        self.prepare_full_contents_cache();
        self.full_contents_from_cache()
    }

    pub(crate) fn prepare_full_contents_cache(&mut self) {
        self.ensure_cached_full();
        self.ensure_cached_prev_full();
    }

    pub(crate) fn full_contents_from_cache(&self) -> (&str, &str, &[u64], &[u64]) {
        debug_assert!(self.cached_full_valid);
        debug_assert!(self.cached_prev_full_valid);
        (
            &self.cached_prev_full,
            &self.cached_full,
            &self.cached_prev_full_row_hashes,
            &self.cached_full_row_hashes,
        )
    }

    /// Returns the previous and current complete review documents: retained
    /// history followed by the visible grid. The accompanying hashes use the
    /// same physical-row coordinates as the strings.
    pub(crate) fn prepare_document_contents_cache(&mut self) {
        self.ensure_cached_document();
        self.ensure_cached_prev_document();
    }

    pub(crate) fn document_contents_cached(&self) -> (&str, &str, &[u64], &[u64]) {
        debug_assert!(self.cached_document_valid);
        debug_assert!(self.cached_prev_document_valid);
        // Align the two retained documents by absolute history row. When the
        // history cap evicts a prefix, comparing the raw strings could mistake
        // a repeated new tail for unchanged text. Removing that known-deleted
        // prefix preserves row identity even when every row has equal text.
        let evicted_rows = self
            .screen()
            .history_origin
            .saturating_sub(self.prev_screen.history_origin)
            .min(self.cached_prev_document_row_hashes.len());
        let previous_byte_offset = row_byte_offset(&self.cached_prev_document, evicted_rows);
        (
            &self.cached_prev_document[previous_byte_offset..],
            &self.cached_document,
            &self.cached_prev_document_row_hashes[evicted_rows..],
            &self.cached_document_row_hashes,
        )
    }

    #[cfg(test)]
    pub(crate) fn document_contents_cache_is_prepared_for_test(&self) -> bool {
        self.cached_document_valid || self.cached_prev_document_valid
    }

    /// Whether the active visible grid belongs to a newly introduced terminal
    /// context. These transitions invalidate the meaning of every visible row,
    /// so accessibility must read the new grid in full rather than diff it.
    pub(crate) fn accessibility_requires_screen_reintroduction(&self) -> bool {
        let previous = &self.prev_screen;
        let current = self.screen();
        self.accessibility_update_summary().terminal_reset
            || previous.screen != current.screen
            || previous.geometry != current.geometry
    }

    /// Whether retained history has a continuous row identity across the
    /// current accessibility boundary. A history-only gap prevents a complete
    /// document diff, but does not invalidate fixed visible-grid coordinates;
    /// callers can still fall back to an explicit visible-grid diff.
    pub(crate) fn accessibility_document_is_continuous(&self) -> bool {
        if self.accessibility_requires_screen_reintroduction() {
            return false;
        }
        let previous = &self.prev_screen;
        let current = self.screen();

        let Some(previous_end) = previous
            .history_origin
            .checked_add(previous.scrollback_extent)
        else {
            return false;
        };
        let Some(current_end) = current
            .history_origin
            .checked_add(current.scrollback_extent)
        else {
            return false;
        };
        previous.history_origin <= current.history_origin
            && current.history_origin <= previous_end
            && current_end >= previous_end
    }

    pub(crate) fn accessibility_document_changed(&self) -> bool {
        let previous = &self.prev_screen;
        let current = self.screen();
        previous.history_origin != current.history_origin
            || previous.scrollback_extent != current.scrollback_extent
            || self.accessibility_update_summary().history_changed
    }

    fn ensure_cached_full(&mut self) {
        if self.cached_full_valid {
            return;
        }
        let mut cached_full = std::mem::take(&mut self.cached_full);
        self.screen().contents_full_into(&mut cached_full);
        compute_row_hashes(&cached_full, &mut self.cached_full_row_hashes);
        self.cached_full = cached_full;
        self.cached_full_valid = true;
    }

    fn ensure_cached_prev_full(&mut self) {
        if self.cached_prev_full_valid {
            return;
        }
        let mut cached_prev_full = std::mem::take(&mut self.cached_prev_full);
        self.prev_screen.contents_full_into(&mut cached_prev_full);
        compute_row_hashes(&cached_prev_full, &mut self.cached_prev_full_row_hashes);
        self.cached_prev_full = cached_prev_full;
        self.cached_prev_full_valid = true;
    }

    fn ensure_cached_document(&mut self) {
        if self.cached_document_valid {
            return;
        }
        let mut cached_document = std::mem::take(&mut self.cached_document);
        self.snapshot_with_history()
            .document_contents_into(&mut cached_document);
        compute_row_hashes(&cached_document, &mut self.cached_document_row_hashes);
        self.cached_document = cached_document;
        self.cached_document_valid = true;
    }

    fn ensure_cached_prev_document(&mut self) {
        if self.cached_prev_document_valid {
            return;
        }
        let mut cached_prev_document = std::mem::take(&mut self.cached_prev_document);
        self.prev_screen
            .document_contents_into(&mut cached_prev_document);
        compute_row_hashes(
            &cached_prev_document,
            &mut self.cached_prev_document_row_hashes,
        );
        self.cached_prev_document = cached_prev_document;
        self.cached_prev_document_valid = true;
    }
}

fn row_byte_offset(contents: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    contents
        .match_indices('\n')
        .nth(row - 1)
        .map_or(contents.len(), |(offset, _)| offset + 1)
}

/// Transfers print provenance to accessibility and copies changed-row ranges
/// only when the immediate renderer still owns the batch. Renderer operations,
/// terminal replies, and side effects never enter the presentation journal.
fn take_normalized_accessibility_evidence(
    update: &mut UpdateSummary,
    preserve_renderer_damage: bool,
) -> UpdateSummary {
    let changed_rows = if preserve_renderer_damage {
        update.changed_rows.clone()
    } else {
        std::mem::take(&mut update.changed_rows)
    };
    // `completes_linear_output_record` uses the starting-boundary evidence to
    // reject a carriage-return overwrite of content predating this span.
    UpdateSummary {
        printed_runs: std::mem::take(&mut update.printed_runs),
        linear_output_effect: std::mem::take(&mut update.linear_output_effect),
        output_report_structural: update.output_report_structural,
        parser_continuation: update.parser_continuation,
        cursor_operations: update.cursor_operations,
        cursor_operations_after_last_line_feed: update.cursor_operations_after_last_line_feed,
        line_feed_boundaries: update.line_feed_boundaries,
        scroll_operations: update.scroll_operations,
        changed_rows,
        cursor_before: update.cursor_before,
        screen_before: update.screen_before,
        screen_after: update.screen_after,
        semantic_input_boundary: update.semantic_input_boundary,
        batch_count: update.batch_count,
        ..UpdateSummary::default()
    }
}

/// Keeps only fixed-size facts which remain true when accessibility must use
/// authoritative before/after snapshots instead of the transient print
/// stream. In particular, structural painting and LF boundaries describe how
/// the application produced the committed frame without retaining its text.
fn snapshot_diff_provenance(update: &UpdateSummary) -> UpdateSummary {
    UpdateSummary {
        linear_output_effect: crate::terminal::LinearOutputEffect::Clear,
        output_report_structural: update.output_report_structural,
        cursor_operations: update.cursor_operations,
        cursor_operations_after_last_line_feed: update.cursor_operations_after_last_line_feed,
        line_feed_boundaries: update.line_feed_boundaries,
        scroll_operations: update.scroll_operations,
        cursor_before: update.cursor_before,
        cursor_after: update.cursor_after,
        screen_before: update.screen_before,
        screen_after: update.screen_after,
        batch_count: update.batch_count,
        ..UpdateSummary::default()
    }
}

fn accessibility_evidence_retained_bytes(update: &UpdateSummary) -> usize {
    let linear_output_bytes = match &update.linear_output_effect {
        crate::terminal::LinearOutputEffect::Append(report)
        | crate::terminal::LinearOutputEffect::Replace(report) => report
            .printed_runs
            .len()
            .saturating_mul(std::mem::size_of::<crate::terminal::PrintedRun>())
            .saturating_add(report.printed_runs.iter().map(|run| run.text.len()).sum()),
        crate::terminal::LinearOutputEffect::Preserve
        | crate::terminal::LinearOutputEffect::Clear => 0,
    };
    std::mem::size_of::<AccessibilityJournalEntry>()
        .saturating_add(
            update
                .printed_runs
                .len()
                .saturating_mul(std::mem::size_of::<crate::terminal::PrintedRun>()),
        )
        .saturating_add(
            update
                .printed_runs
                .iter()
                .map(|run| run.text.len())
                .sum::<usize>(),
        )
        .saturating_add(linear_output_bytes)
        .saturating_add(
            update
                .changed_rows
                .len()
                .saturating_mul(std::mem::size_of::<std::ops::RangeInclusive<u16>>()),
        )
}

fn history_window_end(snapshot: &TerminalSnapshot) -> usize {
    snapshot
        .history_origin
        .saturating_add(snapshot.scrollback_extent)
}

fn translate_scrollback_offset(
    scrollback: usize,
    old: &TerminalSnapshot,
    new: &TerminalSnapshot,
) -> usize {
    if scrollback == 0 {
        return 0;
    }
    let old_top = history_window_end(old).saturating_sub(scrollback.min(old.scrollback_extent));
    let new_start = new.history_origin;
    let new_end = history_window_end(new);
    let new_top = old_top.clamp(new_start, new_end);
    new_end.saturating_sub(new_top).min(new.scrollback_extent)
}

fn translate_review_selection(
    scrollback: usize,
    cursor: (u16, u16),
    old: &TerminalSnapshot,
    new: &TerminalSnapshot,
) -> (usize, (u16, u16)) {
    let new_rows = new.size().0;
    let new_cols = new.size().1;
    let clamped_cursor = (
        cursor.0.min(new_rows.saturating_sub(1)),
        cursor.1.min(new_cols.saturating_sub(1)),
    );
    if scrollback == 0 {
        return (0, clamped_cursor);
    }
    if old.screen != new.screen || old.size() != new.size() {
        return (
            translate_scrollback_offset(scrollback, old, new),
            clamped_cursor,
        );
    }

    let old_top = history_window_end(old).saturating_sub(scrollback.min(old.scrollback_extent));
    let selected = old_top.saturating_add(usize::from(cursor.0)).clamp(
        new.history_origin,
        history_window_end(new).saturating_add(usize::from(new_rows.saturating_sub(1))),
    );
    let new_top = selected
        .saturating_sub(usize::from(cursor.0))
        .clamp(new.history_origin, history_window_end(new));
    let new_scrollback = history_window_end(new)
        .saturating_sub(new_top)
        .min(new.scrollback_extent);
    let new_cursor_row = selected
        .saturating_sub(new_top)
        .min(usize::from(new_rows.saturating_sub(1))) as u16;
    (new_scrollback, (new_cursor_row, clamped_cursor.1))
}

fn translate_history_position(
    position: HistoryPosition,
    old: &TerminalSnapshot,
    new: &TerminalSnapshot,
) -> Option<HistoryPosition> {
    if old.screen != new.screen || old.size() != new.size() {
        return None;
    }
    let absolute_row = old.history_origin.checked_add(position.row)?;
    let row = absolute_row.checked_sub(new.history_origin)?;
    let logical_rows = new
        .scrollback_extent
        .checked_add(usize::from(new.size().0))?;
    (row < logical_rows).then(|| HistoryPosition {
        row,
        col: position.col.min(new.size().1.saturating_sub(1)),
    })
}

fn snapshot_at_scrollback(snapshot: &TerminalSnapshot, scrollback: usize) -> TerminalSnapshot {
    let height = snapshot.rows.len();
    let history_len = snapshot.scrollback.len();
    let offset = scrollback.min(history_len);
    let end = history_len.saturating_add(height).saturating_sub(offset);
    let start = end.saturating_sub(height);
    let rows = if offset == 0 {
        Arc::clone(&snapshot.rows)
    } else {
        snapshot
            .scrollback
            .iter()
            .chain(snapshot.rows.iter())
            .skip(start)
            .take(height)
            .cloned()
            .collect::<Vec<_>>()
            .into()
    };
    let mut cursor = snapshot.cursor;
    if offset != 0 {
        cursor.visible = false;
    }
    TerminalSnapshot {
        rows,
        scrollback: VecDeque::new(),
        cursor,
        geometry: snapshot.geometry,
        screen: snapshot.screen,
        modes: snapshot.modes.clone(),
        title: snapshot.title.clone(),
        working_directory: snapshot.working_directory.clone(),
        semantic_marks: snapshot.semantic_marks.clone(),
        history_origin: snapshot.history_origin,
        scrollback_extent: snapshot.scrollback_extent,
        viewport: if offset == 0 {
            Viewport::Live
        } else {
            Viewport::Scrollback(offset)
        },
    }
}

/// Fits an immutable frame to the current compositor grid without reflowing
/// its text. Accessibility retains the original snapshot and geometry; this
/// clone only prevents a resized overlay/underlay from leaving live cells
/// uncovered on the physical terminal.
fn fit_snapshot_to_geometry(snapshot: &mut TerminalSnapshot, geometry: TerminalGeometry) {
    let cols = usize::from(geometry.cols);
    let rows = Arc::make_mut(&mut snapshot.rows);
    for row in rows.iter_mut() {
        let cells = Arc::make_mut(&mut row.cells);
        cells.resize(cols, crate::terminal::Cell::default());
        if cells.last().is_some_and(crate::terminal::Cell::is_wide) {
            *cells.last_mut().expect("row has a final cell") = crate::terminal::Cell::default();
        }
    }
    rows.resize_with(usize::from(geometry.rows), || crate::terminal::Row {
        cells: Arc::new(vec![crate::terminal::Cell::default(); cols]),
        wrapped: false,
    });
    snapshot.geometry = geometry;
    snapshot.cursor.row = snapshot.cursor.row.min(geometry.rows.saturating_sub(1));
    snapshot.cursor.col = snapshot.cursor.col.min(geometry.cols.saturating_sub(1));
}

fn contents_between_snapshot_history(
    snapshot: &TerminalSnapshot,
    start: HistoryPosition,
    end: HistoryPosition,
) -> Option<String> {
    let rows = snapshot.scrollback.iter().chain(snapshot.rows.iter());
    let logical_rows = snapshot
        .scrollback
        .len()
        .saturating_add(snapshot.rows.len());
    if start > end || end.row >= logical_rows {
        return None;
    }
    let cols = snapshot.size().1;
    let mut contents = String::new();
    for (absolute_row, row) in rows
        .enumerate()
        .skip(start.row)
        .take(end.row.saturating_sub(start.row).saturating_add(1))
    {
        let start_col = if absolute_row == start.row {
            start.col
        } else {
            0
        };
        let end_col = if absolute_row == end.row {
            end.col
        } else {
            cols
        };
        for col in start_col..end_col.min(cols) {
            contents.push_str(
                row.cells
                    .get(usize::from(col))
                    .map_or("", crate::terminal::Cell::contents),
            );
        }
        if absolute_row != end.row && !row.wrapped {
            contents.push('\n');
        }
    }
    Some(contents)
}

fn compute_row_hashes(source: &str, out: &mut Vec<u64>) {
    out.clear();
    for line in source.split_terminator('\n') {
        out.push(fnv1a_64(line.as_bytes()));
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::{
        PRESENTED_HISTORY_MAX_DELTA_DEPTH, PRESENTED_HISTORY_MAX_RETAINED_ROWS, View,
        compute_row_hashes, fnv1a_64,
    };
    use crate::{
        presentation::SurfaceId,
        terminal::{HistoryPosition, SemanticKind as Osc133Kind, Viewport},
    };
    use std::sync::Arc;

    fn accessibility_apc(payload: &str) -> Vec<u8> {
        let mut sequence = b"\x1B_".to_vec();
        sequence.extend_from_slice(payload.as_bytes());
        sequence.extend_from_slice(b"\x1B\\");
        sequence
    }

    #[test]
    fn application_accessibility_is_generic_bounded_and_fails_safe() {
        let mut view = View::new(3, 20);
        view.process_changes(&accessibility_apc("Lector;A11y;1;set;auto=0;cursor=0"));
        let policy = view.application_accessibility_policy();
        assert!(policy.suppress_auto_read);
        assert!(policy.suppress_cursor_tracking);

        view.process_changes(&accessibility_apc("Lector;A11y;1;say;68656c6c6f"));
        let speech = view.take_presented_application_speech();
        assert_eq!(speech.len(), 1);
        assert_eq!(speech[0].text, "hello");
        assert_eq!(speech[0].indentation, None);
        assert!(view.take_presented_application_speech().is_empty());

        view.process_changes(&accessibility_apc("Lector;A11y;1;end"));
        assert_eq!(view.application_accessibility_policy(), Default::default());

        view.process_changes(&accessibility_apc("Lector;A11y;1;set;auto=0;cursor=0"));
        view.process_changes(b"\x1B[!p");
        assert_eq!(view.application_accessibility_policy(), Default::default());

        // A client may reassert immediately after terminal initialization in
        // the same PTY batch.
        let mut reset_and_reassert = b"\x1Bc".to_vec();
        reset_and_reassert.extend(accessibility_apc("Lector;A11y;1;set;auto=0;cursor=0"));
        view.process_changes(&reset_and_reassert);
        assert!(
            view.application_accessibility_policy()
                .suppress_cursor_tracking
        );

        view.process_changes(b"\x1B[?1049h");
        assert_eq!(view.application_accessibility_policy(), Default::default());
    }

    #[test]
    fn application_accessibility_follows_presentation_and_drops_hidden_speech() {
        let mut view = View::new(3, 20);
        view.enable_presentation_tracking();
        view.process_changes(&accessibility_apc("Lector;A11y;1;set;auto=0;cursor=0"));
        view.process_changes(&accessibility_apc("Lector;A11y;1;say;7265616479"));

        assert_eq!(view.application_accessibility_policy(), Default::default());
        assert!(view.take_presented_application_speech().is_empty());
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(frame));
        assert!(
            view.application_accessibility_policy()
                .suppress_cursor_tracking
        );
        let speech = view.take_presented_application_speech();
        assert_eq!(speech.len(), 1);
        assert_eq!(speech[0].text, "ready");

        let hidden = accessibility_apc("Lector;A11y;1;say;68696464656e");
        view.process_changes_with_batch(&hidden, false);
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(frame));
        assert!(view.take_presented_application_speech().is_empty());
    }

    #[test]
    fn resize_clamps_review_cursor_and_clears_displaced_mark() {
        let mut view = View::new(4, 8);
        view.set_review_cursor_position((3, 7));
        view.set_review_mark();

        view.set_size(2, 5);

        assert_eq!(view.review_cursor_position(), (1, 4));
        assert_eq!(view.review_mark_position(), None);
    }

    #[test]
    fn resize_preserves_mark_when_review_cursor_remains_valid() {
        let mut view = View::new(4, 8);
        view.set_review_cursor_position((1, 2));
        view.set_review_mark();

        view.set_size(3, 6);

        assert_eq!(view.review_cursor_position(), (1, 2));
        assert_eq!(
            view.review_mark_position(),
            Some(HistoryPosition { row: 1, col: 2 })
        );
    }

    #[test]
    fn finalize_advances_screen_and_clears_pending_update() {
        let mut view = View::new(2, 8);
        view.process_changes(b"hello");
        assert_eq!(view.update_summary().printed_text(), "hello");
        assert_ne!(view.screen().contents(), view.prev_screen().contents());

        view.finalize_changes(42);

        assert_eq!(view.update_summary().batch_count, 0);
        assert_eq!(view.screen().contents(), view.prev_screen().contents());
        assert_eq!(view.prev_screen_time, 42);
    }

    #[test]
    fn standalone_batch_retains_its_full_nonrenderer_summary() {
        let mut view = View::new(2, 16);
        let batch = view.process_changes_with_batch(b"hello\x07\x1b]2;pane title\x07\x1b[6n", true);
        let retained = view.update_summary();

        assert_eq!(retained.batch_count, batch.batch_count);
        assert_eq!(retained.effects, batch.effects);
        assert_eq!(retained.pty_replies, batch.pty_replies);
        assert_eq!(retained.printed_text(), batch.printed_text());
        assert_eq!(retained.damage, batch.damage);
        assert_eq!(retained.changed_rows, batch.changed_rows);
        assert_eq!(retained.cursor_before, batch.cursor_before);
        assert_eq!(retained.cursor_after, batch.cursor_after);
        assert_eq!(retained.screen_before, batch.screen_before);
        assert_eq!(retained.screen_after, batch.screen_after);
        assert!(
            retained.operations.is_empty(),
            "renderer-only operations must not accumulate in the standalone summary"
        );
    }

    #[test]
    fn unscrolled_updates_do_not_materialize_retained_history() {
        let mut view = View::new_with_scrollback(2, 8, 32);
        let mut history = Vec::new();
        for _ in 0..40 {
            history.extend_from_slice(b"line\r\n");
        }
        view.process_changes(&history);
        view.finalize_changes(0);
        assert_eq!(view.scrollback_len(), 32);

        view.engine.reset_snapshot_refresh_counts();
        view.process_changes(b"current");
        assert_eq!(
            view.engine.snapshot_refresh_counts(),
            (1, 0),
            "an unscrolled update should refresh only the live Ghostty grid"
        );

        view.engine.reset_snapshot_refresh_counts();
        let snapshot = view.with_live_screen(|view| view.live_screen().clone());
        assert_eq!(snapshot.viewport, Viewport::Live);
        assert_eq!(
            view.engine.snapshot_refresh_counts(),
            (0, 0),
            "borrowing an already-live screen should not refresh or copy history"
        );
    }

    #[test]
    fn cached_contents_follow_process_and_finalize_lifecycle() {
        let mut view = View::new(2, 8);
        view.process_changes(b"old");
        view.finalize_changes(1);
        view.process_changes(b"\rnew");

        let (previous, current, previous_hashes, current_hashes) = view.full_contents_cached();
        assert!(previous.starts_with("old"));
        assert!(current.starts_with("new"));
        assert_ne!(previous_hashes, current_hashes);

        view.finalize_changes(2);
        let (previous, current, previous_hashes, current_hashes) = view.full_contents_cached();
        assert_eq!(previous, current);
        assert_eq!(previous_hashes, current_hashes);
    }

    #[test]
    fn presentation_tracking_defers_ordinary_output_until_its_frame_flushes() {
        let mut view = View::new(2, 16);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\x1b[2Knew");
        let frame = view.capture_live_presentation_frame(SurfaceId(7));
        assert!(
            frame.history.is_none(),
            "ordinary visible updates must not copy unchanged scrollback"
        );

        assert_eq!(view.line(0), "old");
        assert!(!view.application_transaction_open());
        assert!(view.accessibility_awaiting_presentation());
        assert!(view.apply_presented_frame(frame));
        assert_eq!(view.line(0), "new");
        assert!(!view.accessibility_awaiting_presentation());
    }

    #[test]
    fn tracking_keeps_a_real_close_private_until_the_closed_frame_flushes() {
        let mut view = View::new(1, 20);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?2026h\r\x1b[2Kpartial");
        assert!(view.application_transaction_open());
        assert_eq!(view.line(0), "old");

        view.process_changes(b"\r\x1b[2Kcommitted\x1b[?2026l");
        let closed = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(closed.synchronized_output_closed);
        assert!(!view.application_transaction_open());
        assert!(view.accessibility_awaiting_presentation());
        assert_eq!(view.line(0), "old");

        assert!(view.apply_presented_frame(closed));
        assert_eq!(view.line(0), "committed");
        assert!(!view.accessibility_awaiting_presentation());
        assert!(view.accessibility_presentation_synchronized_output_closed());
    }

    #[test]
    fn osc133_prompt_boundaries_do_not_become_stabilization_boundaries() {
        let mut view = View::new(1, 24);
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b]133;A\x07$ ");
        let partial = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(!partial.synchronized_output_closed);
        assert!(view.apply_presented_frame(partial));

        view.process_changes(b"ready \x1b]133;B\x07");
        let complete = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(!complete.synchronized_output_closed);
        assert!(view.apply_presented_frame(complete));
        assert!(!view.accessibility_presentation_synchronized_output_closed());
    }

    #[test]
    fn a_new_prompt_marker_at_the_same_cell_remains_distinct_semantic_history() {
        let mut view = View::new(1, 24);
        view.process_changes(b"\x1b]133;A\x07$ ");
        let first_count = view.osc133_marks().len();
        view.finalize_changes(1);

        view.process_changes(b"\r\x1b]133;A\x07$ ");
        assert_eq!(view.osc133_marks().len(), first_count + 1);
    }

    #[test]
    fn cursor_restore_at_end_of_update_is_retained_as_painting_provenance() {
        let mut view = View::new(1, 24);
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?25llegacy redraw\x1b[?25h");
        let restored = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(restored.cursor_visibility_restored);
        assert!(view.apply_presented_frame(restored));
        assert!(view.presented_revision_cursor_restored);

        view.process_changes(b" trailing");
        let trailing = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(!trailing.cursor_visibility_restored);
    }

    #[test]
    fn alternate_screen_review_context_restores_the_primary_selection() {
        let mut view = View::new(4, 40);
        view.process_changes(b"saved review\r\nprimary cursor");
        view.finalize_changes(0);
        view.prepare_review_cursor_for_activation();
        view.set_review_cursor_position((0, 0));

        view.process_changes(b"\x1b[?1049h\x1b[2J\x1b[Halternate\x1b[3;5H");
        view.prepare_review_cursor_for_activation();
        assert_eq!(view.review_cursor_position(), (2, 4));
        view.finalize_changes(1);

        view.process_changes(b"\x1b[?1049l");
        view.prepare_review_cursor_for_activation();
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.line(0), "saved review");
    }

    #[test]
    fn output_after_a_synchronized_close_requires_ordinary_stabilization() {
        let mut view = View::new(1, 24);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?2026h\rfinal\x1b[?2026l trailing");
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(!frame.synchronized_output_closed);
        assert!(view.apply_presented_frame(frame));
        assert!(!view.accessibility_presentation_synchronized_output_closed());
    }

    #[test]
    fn timed_out_frames_publish_the_exact_revision_that_reached_the_terminal() {
        let mut view = View::new(1, 20);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?2026h\r\x1b[2Kpartial");
        let timed_out = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\x1b[2Knewer");
        let newest = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(timed_out));
        assert_eq!(view.line(0), "partial");
        assert!(view.application_transaction_open());
        assert!(view.accessibility_awaiting_presentation());

        assert!(view.apply_presented_frame(newest));
        assert_eq!(view.line(0), "newer");
        assert!(!view.accessibility_awaiting_presentation());
    }

    #[test]
    fn parser_ahead_receipt_publishes_its_exact_lf_and_continuation_facts() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"receipt line\r\n");
        let completed = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"newer parser text\x1b[");
        let continuation = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(completed));
        assert!(view.accessibility_awaiting_presentation());
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "receipt line\n"
        );
        assert!(!view.accessibility_update_summary().parser_continuation);
        assert!(!view.accessibility_update_summary().output_report_structural);
        assert!(view.accessibility_completes_linear_output_record());

        view.finalize_changes(1);
        assert!(view.apply_presented_frame(continuation));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "newer parser text"
        );
        assert!(view.accessibility_update_summary().parser_continuation);
        assert!(!view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn parser_ahead_receipt_keeps_its_structural_classifier_fact() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[2J\x1b[Hstructural frame");
        let structural = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b" newer ordinary text");

        assert!(view.apply_presented_frame(structural));
        assert!(view.accessibility_update_summary().output_report_structural);
        assert!(!view.accessibility_update_summary().parser_continuation);
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "structural frame"
        );
    }

    #[test]
    fn receipt_evidence_preserves_the_starting_column_for_carriage_return_classification() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"prefix");
        let baseline = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(baseline));
        view.finalize_changes(1);

        view.process_changes(b"\rreplacement\n");
        let overwritten = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(overwritten));
        assert_eq!(view.accessibility_update_summary().cursor_before.col, 6);
        assert!(!view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn screen_transition_epoch_preserves_an_older_receipt_without_crossing_contexts() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"primary record\r\n");
        let primary = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\x1b[?1049h\x1b[2J\x1b[Halternate line");
        let alternate = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(primary));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "primary record\n"
        );
        view.finalize_changes(1);

        assert!(view.apply_presented_frame(alternate));
        assert_eq!(
            view.accessibility_update_summary().screen_before,
            crate::terminal::ScreenIdentity::Primary
        );
        assert_eq!(
            view.accessibility_update_summary().screen_after,
            crate::terminal::ScreenIdentity::Alternate
        );
        assert!(view.accessibility_update_summary().output_report_structural);
        assert!(
            !view
                .accessibility_update_summary()
                .printed_text()
                .contains("primary record")
        );
    }

    #[test]
    fn ordered_split_receipts_extend_only_the_presented_evidence_prefix() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"split");
        let partial = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b" record\r\n");
        let complete = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(partial));
        assert_eq!(view.accessibility_update_summary().printed_text(), "split");
        assert!(!view.accessibility_completes_linear_output_record());

        assert!(view.apply_presented_frame(complete));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "split record\n"
        );
        assert!(view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn replacement_receipt_collects_skipped_revision_evidence_once() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"replacement");
        let obsolete = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b" winner\r\n");
        let winner = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(winner));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "replacement winner\n"
        );
        assert!(view.accessibility_completes_linear_output_record());
        assert!(!view.apply_presented_frame(obsolete));
    }

    #[test]
    fn accessibility_handoff_epoch_rejects_stale_receipt_evidence() {
        let mut view = View::new(4, 40);
        view.enable_presentation_tracking();

        view.process_changes(b"stale record\r\n\x1b]133;B\x07");
        let stale = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(!stale.synchronized_output_closed);
        view.clear_update_summary();
        let handoff_epoch = view.live_accessibility_epoch;
        view.process_changes(b"fresh record\r\n");
        let fresh = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(stale));
        assert_eq!(view.presented_accessibility_epoch, handoff_epoch);
        assert_eq!(view.accessibility_update_summary().batch_count, 0);
        assert!(!view.accessibility_presentation_synchronized_output_closed());
        assert!(!view.accessibility_completes_linear_output_record());

        assert!(view.apply_presented_frame(fresh));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "fresh record\n"
        );
        assert!(view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn accessibility_journal_is_bounded_and_falls_back_after_eviction() {
        let mut view = View::new(2, 40);
        view.enable_presentation_tracking();

        for _ in 0..=super::ACCESSIBILITY_JOURNAL_MAX_ENTRIES {
            view.process_changes(b"x");
        }
        let newest = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.accessibility_journal.len() <= super::ACCESSIBILITY_JOURNAL_MAX_ENTRIES);
        assert!(view.accessibility_journal_bytes <= super::ACCESSIBILITY_JOURNAL_MAX_BYTES);
        assert!(view.apply_presented_frame(newest));
        assert!(view.accessibility_update_summary().batch_count > 0);
        assert!(view.accessibility_update_summary().printed_runs.is_empty());
        assert!(
            !view.accessibility_completes_linear_output_record(),
            "an evicted prefix must use the authoritative snapshot diff"
        );
    }

    #[test]
    fn tracked_output_under_sustained_backpressure_has_no_unbounded_legacy_summary() {
        let mut view = View::new(2, 40);
        view.enable_presentation_tracking();

        // No presentation frame is applied: this models a physical writer
        // which remains backpressured while the parser and replaceable render
        // continue advancing. Exercise both batch-producing call sites and the
        // summary-free model-only path.
        for index in 0..super::ACCESSIBILITY_JOURNAL_MAX_ENTRIES.saturating_mul(2) {
            if index % 2 == 0 {
                let batch = view.process_changes_with_batch(b"streaming output ", true);
                assert_eq!(batch.batch_count, 1);
                assert!(
                    batch.printed_runs.is_empty(),
                    "print provenance should move to the bounded receipt journal"
                );
            } else {
                view.process_changes(b"more output ");
            }
        }

        assert!(view.accessibility_awaiting_presentation());
        assert!(view.accessibility_journal.len() <= super::ACCESSIBILITY_JOURNAL_MAX_ENTRIES);
        assert!(view.accessibility_journal_bytes <= super::ACCESSIBILITY_JOURNAL_MAX_BYTES);
        assert!(view.accessibility_journal_gap_start.is_some());

        let legacy = view.update_summary();
        assert_eq!(legacy.batch_count, 0);
        assert!(legacy.printed_runs.is_empty());
        assert!(legacy.changed_rows.is_empty());
        assert!(legacy.operations.is_empty());
        assert!(legacy.effects.events.is_empty());
        assert!(legacy.pty_replies.is_empty());
        assert_eq!(view.accessibility_update_summary().batch_count, 0);
    }

    #[test]
    fn oversized_evidence_keeps_later_lf_ambiguous_until_a_diff_baseline() {
        let mut view = View::new(2, 80);
        view.enable_presentation_tracking();

        let oversized = vec![b'x'; super::ACCESSIBILITY_JOURNAL_MAX_BYTES + 1];
        view.process_changes(&oversized);
        view.process_changes(b"\r\nlater line\r\n");
        let frame = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(frame));
        assert!(view.accessibility_update_summary().batch_count > 0);
        assert!(view.accessibility_update_summary().printed_runs.is_empty());
        assert!(!view.accessibility_completes_linear_output_record());

        view.finalize_changes(1);
        view.process_changes(b"after baseline\r\n");
        let after_baseline = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(after_baseline));
        assert_eq!(
            view.accessibility_update_summary().printed_text(),
            "after baseline\n"
        );
        assert!(view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn snapshot_diff_fallback_retains_fixed_size_output_provenance() {
        let mut view = View::new(4, 40);
        view.enable_presentation_tracking();
        view.process_changes(b"\x1b[?2026h\x1b[2J\x1b[Hfirst\r\nsecond\x1b[4;1Hstatus\x1b[?2026l");
        let frame = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(frame));
        let update = view.accessibility_update_summary();
        assert!(update.printed_runs.is_empty());
        assert!(update.output_report_structural);
        assert_eq!(update.line_feed_boundaries, 1);
        assert!(update.cursor_operations >= 2);
    }

    #[test]
    fn completed_record_validation_is_cached_for_one_presented_revision() {
        let mut view = View::new(3, 40);
        view.enable_presentation_tracking();
        view.process_changes(b"cached line\r\n");
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(frame));

        assert!(view.accessibility_completes_linear_output_record());
        view.completed_linear_record_report.clear();
        view.completed_linear_record_report
            .push_str("cache sentinel");
        assert!(view.accessibility_completes_linear_output_record());
        assert_eq!(view.completed_linear_record_report, "cache sentinel");

        view.finalize_changes(1);
        assert!(!view.accessibility_completes_linear_output_record());
    }

    #[test]
    fn presented_cursor_follow_compares_consecutive_physical_frames() {
        let mut view = View::new(3, 20);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?2026h\x1b[3;1Hpartial");
        let timed_out = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(timed_out));
        assert!(view.review_cursor_follow_pending());
        view.follow_application_cursor();
        assert_eq!(view.review_cursor_position().0, 2);

        view.process_changes(b"\x1b[1;1Hcommitted\x1b[?2026l");
        let closed = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(view.apply_presented_frame(closed));
        assert!(view.review_cursor_follow_pending());
        view.follow_application_cursor();
        assert_eq!(view.review_cursor_position().0, 0);
    }

    #[test]
    fn presented_history_growth_keeps_an_unscrolled_review_viewport_live() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nthree");
        let three = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nfour");
        let four = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(three));
        assert_eq!(view.scrollback(), 0);
        assert_eq!(view.line(0), "two");
        assert_eq!(view.line(1), "three");

        assert!(view.apply_presented_frame(four));
        assert_eq!(view.scrollback(), 0);
        assert_eq!(view.line(0), "three");
        assert_eq!(view.line(1), "four");
    }

    #[test]
    fn dropped_history_receipts_replay_the_complete_delta_chain() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nthree");
        let _dropped_one = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nfour");
        let _dropped_two = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nfive");
        let newest = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(newest));
        assert_eq!(view.committed_snapshot.history_origin, 0);
        assert_eq!(
            view.committed_snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn malformed_history_metadata_rejects_without_moving_committed_rows() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.enable_presentation_tracking();
        view.process_changes(b"\r\nthree");
        let valid = view.capture_live_presentation_frame(SurfaceId(1));
        let mut malformed = valid.clone();
        malformed.snapshot.history_origin = malformed.snapshot.history_origin.saturating_add(1);

        assert!(!view.apply_presented_frame(malformed));
        assert_eq!(view.presented_history_revision, 0);
        assert!(view.committed_snapshot.scrollback.is_empty());
        assert!(view.apply_presented_frame(valid));
        assert_eq!(view.committed_snapshot.scrollback[0].contents(), "one");
    }

    #[test]
    fn resurfacing_after_a_disjoint_hidden_history_gap_uses_a_full_root() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.enable_presentation_tracking();

        // Model a view which keeps parsing while no scene includes it. The
        // new logical window no longer overlaps the last presented interval.
        view.process_changes(b"\r\nfive\r\nsix\r\nseven\r\neight");
        let resurfaced = view.capture_live_presentation_frame(SurfaceId(1));
        let history = resurfaced.history.as_deref().expect("resurface root");
        assert!(history.full_replacement);
        assert!(history.previous.is_none());
        assert!(view.apply_presented_frame(resurfaced));
        assert_eq!(
            view.committed_snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["five", "six"]
        );
    }

    #[test]
    fn a_flushed_intermediate_history_receipt_is_skipped_by_the_newer_chain() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nthree");
        let dropped = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nfour");
        let intermediate = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nfive");
        let newest = view.capture_live_presentation_frame(SurfaceId(1));

        drop(dropped);
        assert!(view.apply_presented_frame(intermediate));
        assert_eq!(view.presented_history_revision, 2);
        assert!(view.apply_presented_frame(newest));
        assert_eq!(view.presented_history_revision, 3);
        assert_eq!(
            view.committed_snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn capped_history_chain_evicts_exact_prefix_when_intermediate_is_dropped() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nfive");
        let _dropped = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\r\nsix");
        let newest = view.capture_live_presentation_frame(SurfaceId(1));

        assert!(view.apply_presented_frame(newest));
        assert_eq!(view.committed_snapshot.history_origin, 2);
        assert_eq!(view.committed_snapshot.scrollback_extent, 2);
        assert_eq!(
            view.committed_snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["three", "four"]
        );
    }

    #[test]
    fn parser_ahead_alternate_handoff_uses_independent_full_roots() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?1049h");
        let alternate = view.capture_live_presentation_frame(SurfaceId(1));
        let alternate_history = alternate.history.as_deref().expect("alternate root");
        assert!(alternate_history.full_replacement);
        assert!(alternate_history.rows.is_empty());

        view.process_changes(b"\x1b[?1049l");
        let primary = view.capture_live_presentation_frame(SurfaceId(1));
        let primary_history = primary.history.as_deref().expect("primary root");
        assert!(primary_history.full_replacement);
        assert!(primary_history.previous.is_none());
        assert_eq!(
            primary_history
                .rows
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );

        assert!(view.apply_presented_frame(alternate));
        assert!(view.screen().alternate_screen());
        assert!(view.apply_presented_frame(primary));
        assert!(!view.screen().alternate_screen());
        assert_eq!(
            view.committed_snapshot
                .scrollback
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn false_positive_history_change_still_carries_a_zero_row_generation() {
        let mut view = View::new(2, 4);
        view.enable_presentation_tracking();

        // The observer conservatively treats a right-margin write on the
        // bottom row as possible wrap/scroll. Ghostty remains wrap-pending, so
        // the absolute history interval itself is unchanged.
        view.process_changes(b"\x1b[2;4Hx");
        let frame = view.capture_live_presentation_frame(SurfaceId(1));
        let history = frame.history.as_deref().expect("history generation");
        assert!(history.rows.is_empty());
        assert_eq!(history.revision, frame.history_revision);
        assert!(view.apply_presented_frame(frame));
    }

    #[test]
    fn clear_reset_and_reflow_history_generations_start_full_roots() {
        for structural_update in [b"\x1b[3J".as_slice(), b"\x1bc".as_slice()] {
            let mut view = View::new_with_scrollback(2, 12, 2);
            view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
            view.enable_presentation_tracking();
            view.process_changes(structural_update);
            let frame = view.capture_live_presentation_frame(SurfaceId(1));
            let history = frame.history.as_deref().expect("structural root");
            assert!(history.full_replacement);
            assert!(history.previous.is_none());
            assert!(view.apply_presented_frame(frame));
        }

        let mut resized = View::new_with_scrollback(2, 12, 2);
        resized.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        resized.enable_presentation_tracking();
        resized.process_changes(b"\r\nfive");
        let _pending = resized.capture_live_presentation_frame(SurfaceId(1));
        resized.set_size(3, 12);
        let frame = resized.capture_live_presentation_frame(SurfaceId(1));
        let history = frame.history.as_deref().expect("reflow root");
        assert!(history.full_replacement);
        assert!(history.previous.is_none());
        assert!(resized.apply_presented_frame(frame));
    }

    #[test]
    fn history_delta_chain_compacts_without_invalidating_started_receipts() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.enable_presentation_tracking();
        let mut oldest = None;
        let mut newest = None;
        let mut saw_compacted_root = false;

        for index in 0..(PRESENTED_HISTORY_MAX_DELTA_DEPTH + 32) {
            view.process_changes(format!("\r\nline-{index}").as_bytes());
            let frame = view.capture_live_presentation_frame(SurfaceId(1));
            let history = frame.history.as_ref().expect("history delta");
            if oldest.is_none() {
                oldest = Some(Arc::clone(history));
            } else if history.full_replacement {
                saw_compacted_root = true;
            }
            assert!(history.depth <= PRESENTED_HISTORY_MAX_DELTA_DEPTH);
            assert!(history.retained_rows <= PRESENTED_HISTORY_MAX_RETAINED_ROWS);
            newest = Some(frame);
        }

        let oldest = oldest.expect("first started receipt");
        assert!(!oldest.rows.is_empty());
        assert!(saw_compacted_root);
        assert!(view.apply_presented_frame(newest.expect("newest receipt")));
        assert_eq!(view.committed_snapshot.scrollback.len(), 2);
        assert!(!oldest.rows[0].contents().is_empty());
    }

    #[test]
    fn capped_history_receipts_keep_the_selected_cell_and_mark_anchored() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.set_accessible_scrollback(2);
        view.set_review_cursor_position((1, 0));
        view.set_review_mark();
        assert_eq!(view.line(1), "two");
        assert_eq!(
            view.review_mark_position(),
            Some(HistoryPosition { row: 1, col: 0 })
        );
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nfive");
        let five = view.capture_live_presentation_frame(SurfaceId(1));
        assert_eq!(five.snapshot.history_origin, 1);
        view.process_changes(b"\r\nsix");
        let six = view.capture_live_presentation_frame(SurfaceId(1));
        assert_eq!(view.line(1), "two", "unflushed output stays private");
        assert!(view.apply_presented_frame(five));
        assert_eq!(view.scrollback(), 2);
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.line(0), "two");
        assert_eq!(
            view.review_mark_position(),
            Some(HistoryPosition { row: 0, col: 0 })
        );

        assert!(view.apply_presented_frame(six));
        assert_eq!(view.line(0), "three", "an evicted selection clamps oldest");
        assert_eq!(view.review_mark_position(), None);
    }

    #[test]
    fn capped_history_translates_review_state_across_close_and_reopen() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.set_accessible_scrollback(2);
        view.set_review_cursor_position((1, 0));
        view.set_review_mark();

        view.process_changes(b"\x1b[?2026h\r\nfive");
        assert_eq!(view.line(1), "two");
        view.process_changes(b"\x1b[?2026l\x1b[?2026hpartial");

        assert!(view.holds_synchronized_output());
        assert_eq!(view.scrollback(), 2);
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.line(0), "two");
        assert_eq!(
            view.review_mark_position(),
            Some(HistoryPosition { row: 0, col: 0 })
        );
        let snapshot = view.snapshot_with_history();
        assert_eq!(snapshot.history_origin, 1);
        assert_eq!(snapshot.scrollback[0].contents(), "two");
    }

    #[test]
    fn primary_history_dirty_before_alternate_screen_gets_its_own_receipt() {
        let mut view = View::new_with_scrollback(2, 12, 2);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nfive\x1b[?1049h");
        let alternate = view.capture_live_presentation_frame(SurfaceId(1));
        assert!(alternate.snapshot.alternate_screen());
        assert_eq!(
            alternate
                .history
                .as_deref()
                .map(|history| history.rows.len()),
            Some(0),
            "the active history basis changed even though the final screen is alternate"
        );
        assert!(view.apply_presented_frame(alternate));

        view.process_changes(b"\x1b[?1049l");
        let primary = view.capture_live_presentation_frame(SurfaceId(1));
        assert_eq!(primary.snapshot.history_origin, 1);
        let history = primary
            .history
            .as_deref()
            .expect("refreshed primary history");
        assert_eq!(
            history
                .rows
                .iter()
                .map(|row| row.contents())
                .collect::<Vec<_>>(),
            ["two", "three"]
        );
        assert!(view.apply_presented_frame(primary));
    }

    #[test]
    fn pending_frames_share_the_same_changed_history_generation() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.enable_presentation_tracking();

        view.process_changes(b"\r\nthree");
        let first = view.capture_live_presentation_frame(SurfaceId(1));
        view.process_changes(b"\x1b[H");
        let second = view.capture_live_presentation_frame(SurfaceId(1));

        let first_history = first.history.as_ref().expect("changed history payload");
        let second_history = second.history.as_ref().expect("shared history payload");
        assert!(std::sync::Arc::ptr_eq(first_history, second_history));
    }

    #[test]
    fn tracking_keeps_old_geometry_until_the_resize_frame_flushes() {
        let mut view = View::new(2, 8);
        view.enable_presentation_tracking();

        view.set_size(3, 20);
        let resized = view.capture_live_presentation_frame(SurfaceId(1));

        assert_eq!(view.size(), (2, 8));
        assert_eq!(view.live_size(), (3, 20));
        assert!(view.apply_presented_frame(resized));
        assert_eq!(view.size(), (3, 20));
    }

    #[test]
    fn committed_transition_receipt_updates_geometry_without_exposing_working_text() {
        let mut view = View::new(2, 8);
        view.process_changes(b"old");
        view.enable_presentation_tracking();

        view.process_changes(b"\x1b[?2026h\r\x1b[2Kpartial");
        view.set_size(3, 20);
        let transition = view.capture_committed_presentation_frame(SurfaceId(1));

        assert_eq!(view.size(), (2, 8));
        assert!(view.apply_presented_frame(transition));
        assert_eq!(view.size(), (3, 20));
        assert_eq!(view.line(0), "old");
        assert!(view.accessibility_awaiting_presentation());
    }

    #[test]
    fn synchronized_output_reads_the_exact_pre_frame_snapshot_until_close() {
        let mut view = View::new(2, 20);
        view.process_changes(b"old");
        // This ordinary update deliberately has not crossed the speech-diff
        // finalization boundary. The transaction checkpoint must still see it.
        view.process_changes(b"\r\x1b[2Kpre-frame\x1b[?2026h\r\x1b[2Kpartial");

        assert!(view.holds_synchronized_output());
        assert_eq!(view.line(0), "pre-frame");
        assert_eq!(view.live_screen().contents_between(0, 0, 0, 20), "partial");

        view.process_changes(b"\r\x1b[2Kcommitted\x1b[?2026l");

        assert!(!view.holds_synchronized_output());
        assert_eq!(view.line(0), "committed");
    }

    #[test]
    fn fragmented_synchronization_marker_keeps_frozen_history_addressable() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo\r\nthree");
        assert_eq!(view.scrollback_len(), 1);

        view.process_changes(b"\x1b[?20");
        view.process_changes(b"26h\x1b[2J\x1b[Hnew");

        assert!(view.holds_synchronized_output());
        assert_eq!(view.line(0), "two");
        view.set_accessible_scrollback(1);
        assert_eq!(view.line(0), "one");
        let snapshot = view.snapshot_with_history();
        assert_eq!(snapshot.scrollback[0].contents(), "one");

        view.process_changes(b"\x1b[?2026l");
        assert!(!view.holds_synchronized_output());
        assert_eq!(view.line(0), "one");
        let live = view.with_live_screen(|view| view.live_screen().contents_between(0, 0, 0, 12));
        assert_eq!(live, "new");
    }

    #[test]
    fn selecting_frozen_scrollback_invalidates_the_visible_content_cache() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo\r\nthree");
        view.process_changes(b"\x1b[?2026h\x1b[2J\x1b[Hpartial");

        assert_eq!(view.contents_full(), "two\nthree\n");
        view.set_accessible_scrollback(1);
        assert_eq!(view.contents_full(), "one\ntwo\n");
    }

    #[test]
    fn committed_cursor_movement_before_a_fragmented_opener_still_follows() {
        let mut view = View::new(3, 16);
        view.process_changes(b"old");
        view.finalize_changes(0);

        view.process_changes(b"\x1b[2;1Hcommitted\x1b[?20");
        view.process_changes(b"26h\x1b[3;1Hpartial");

        assert!(view.holds_synchronized_output());
        assert!(view.review_cursor_follow_pending());
        view.follow_application_cursor();
        assert_eq!(view.review_cursor_position(), (1, 9));
        assert_eq!(view.line(1), "committed");
        assert_eq!(view.live_screen().cursor_position(), (2, 7));
        assert!(!view.review_cursor_follow_pending());
    }

    #[test]
    fn synchronized_output_stays_frozen_until_a_real_close() {
        let mut view = View::new(1, 16);
        view.process_changes(b"old");
        view.process_changes(b"\x1b[?2026h\r\x1b[2Kpartial");
        assert_eq!(view.line(0), "old");

        view.process_changes(b"\r\x1b[2Knewer");
        assert!(view.holds_synchronized_output());
        assert_eq!(view.line(0), "old");

        view.process_changes(b"\x1b[?2026l\x1b[?2026h\r\x1b[2Knext");
        assert!(view.holds_synchronized_output());
        assert_eq!(view.line(0), "newer");
        view.process_changes(b"\x1b[?2026l");
        assert_eq!(view.line(0), "next");
    }

    #[test]
    fn synchronized_reopen_commits_history_changed_in_an_earlier_chunk() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.process_changes(b"\x1b[?2026h\r\nthree");

        view.process_changes(b"\x1b[?2026l\x1b[?2026hpartial");

        assert!(view.holds_synchronized_output());
        // An unscrolled review viewport follows the newly committed live
        // frame; retained history remains available without replacing row 0.
        assert_eq!(view.line(0), "two");
        let snapshot = view.snapshot_with_history();
        assert_eq!(snapshot.scrollback[0].contents(), "one");
        view.follow_application_cursor();
        assert_eq!(view.line(0), "two");
    }

    #[test]
    fn synchronized_close_keeps_an_unscrolled_review_viewport_live() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo");
        view.process_changes(b"\x1b[?2026h\r\nthree");
        assert_eq!(view.line(0), "one");

        view.process_changes(b"\x1b[?2026l");

        assert_eq!(view.scrollback(), 0);
        assert_eq!(view.line(0), "two");
        assert_eq!(view.line(1), "three");
    }

    #[test]
    fn resize_does_not_commit_an_open_synchronized_frame() {
        let mut view = View::new(2, 16);
        view.process_changes(b"old");
        view.process_changes(b"\x1b[?2026h\r\x1b[2Kpartial");

        view.set_size(3, 20);
        assert!(view.holds_synchronized_output());
        assert_eq!(view.size(), (2, 16));
        assert_eq!(view.line(0), "old");
        assert_eq!(view.live_screen().size(), (3, 20));

        view.process_changes(b"\x1b[?2026l");
        assert!(!view.holds_synchronized_output());
        assert_eq!(view.size(), (3, 20));
        assert_eq!(view.line(0), "partial");
    }

    #[test]
    fn vertical_navigation_skips_blank_lines_and_stops_at_content_boundaries() {
        let mut view = View::new(5, 10);
        view.process_changes(b"top\r\n\r\nmiddle\r\n\r\nbottom");

        assert!(!view.review_cursor_up(true));
        assert!(view.review_cursor_down(true));
        assert_eq!(view.review_cursor_position(), (2, 0));
        assert!(view.review_cursor_down(true));
        assert_eq!(view.review_cursor_position(), (4, 0));
        assert!(!view.review_cursor_down(true));
        assert!(view.review_cursor_up(true));
        assert_eq!(view.review_cursor_position(), (2, 0));
        assert!(view.review_cursor_up(false));
        assert_eq!(view.review_cursor_position(), (1, 0));
        assert!(view.review_cursor_down(false));
        assert_eq!(view.review_cursor_position(), (2, 0));
    }

    #[test]
    fn word_navigation_handles_first_last_and_inter_word_whitespace() {
        let mut view = View::new(1, 10);
        view.process_changes(b"one  two");

        assert!(!view.review_cursor_prev_word());
        assert!(view.review_cursor_next_word());
        assert_eq!(view.review_cursor_position(), (0, 5));
        assert_eq!(view.word(0, 5), "two");
        assert!(!view.review_cursor_next_word());
        view.set_review_cursor_col(7);
        assert!(view.review_cursor_prev_word());
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.word(0, 4), "one  ");
    }

    #[test]
    fn horizontal_navigation_skips_wide_continuations_and_obeys_edges() {
        let mut view = View::new(1, 6);
        view.process_changes("a界b".as_bytes());

        assert!(!view.review_cursor_left());
        assert!(view.review_cursor_right());
        assert_eq!(view.review_cursor_position(), (0, 1));
        assert_eq!(view.character(0, 1), "界");
        assert!(view.review_cursor_right());
        assert_eq!(view.review_cursor_position(), (0, 3));
        assert!(view.review_cursor_left());
        assert_eq!(view.review_cursor_position(), (0, 1));
        assert!(view.review_cursor_left());
        assert_eq!(view.review_cursor_position(), (0, 0));

        view.set_review_cursor_col(5);
        assert!(!view.review_cursor_right());
    }

    #[test]
    fn indentation_and_full_content_accessors_report_changes_without_blank_resets() {
        let mut view = View::new(3, 12);
        view.process_changes(b"  alpha\r\n    beta\x1B[1;1H");

        assert_eq!(view.review_cursor_indentation_level(), (2, true));
        assert_eq!(view.review_cursor_indentation_level(), (2, false));
        assert_eq!(view.application_cursor_indentation_level(), (2, true));
        assert_eq!(view.application_cursor_indentation_level(), (2, false));

        view.set_review_cursor_row(1);
        assert_eq!(view.review_cursor_indentation_level(), (4, true));
        view.set_review_cursor_row(2);
        assert_eq!(view.review_cursor_indentation_level(), (4, false));

        let mut contents = String::from("stale");
        view.contents_full_into(&mut contents);
        assert_eq!(contents, "  alpha\n    beta\n\n");
        assert_eq!(view.line(1), "    beta");
    }

    #[test]
    fn row_hashes_are_stable_and_reuse_the_destination() {
        assert_eq!(fnv1a_64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);

        let mut hashes = vec![0, 1, 2];
        compute_row_hashes("a\n\n", &mut hashes);
        assert_eq!(hashes, [fnv1a_64(b"a"), fnv1a_64(b"")]);
    }

    #[test]
    fn line_navigation_stops_at_visible_viewport_boundaries() {
        let mut view = View::new(3, 12);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert_eq!(view.scrollback_len(), 2);
        assert_eq!(view.line(0), "three");

        assert!(!view.review_cursor_up(false));
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.scrollback(), 0);
        assert_eq!(view.line(0), "three");

        assert!(!view.review_cursor_up(true));
        assert_eq!(view.review_cursor_position(), (0, 0));
        assert_eq!(view.scrollback(), 0);
        assert_eq!(view.line(0), "three");
    }

    #[test]
    fn osc133_marks_and_submitted_input_survive_scrolling() {
        let mut view = View::new(3, 20);
        view.process_changes(
            b"\x1B]133;A\x07$ \x1B]133;B\x07echo one\r\n\x1B]133;C\x07out\r\n\x1B]133;D;0\x07\r\n\x1B]133;A\x07$ \x1B]133;B\x07echo two\r\n\x1B]133;C\x07done\r\n\x1B]133;D;1\x07",
        );
        assert!(view.scrollback_len() >= 3);
        assert_eq!(
            view.osc133_marks()
                .iter()
                .filter(|mark| matches!(mark.kind, Osc133Kind::PromptStart))
                .count(),
            2
        );
        assert_eq!(view.last_submitted_input().as_deref(), Some("echo two"));
    }

    #[test]
    fn review_copy_can_span_retained_history_and_the_live_screen() {
        let mut view = View::new(2, 8);
        view.process_changes(b"one\r\ntwo\r\nthree");
        view.set_review_history_position(HistoryPosition { row: 0, col: 0 });
        view.set_review_mark();
        view.follow_application_cursor();
        view.set_review_cursor_col(4);

        assert_eq!(
            view.copy_review_selection(view.review_mark_position().unwrap()),
            Some("one\ntwo\nthree".into())
        );
    }

    #[test]
    fn active_semantic_input_uses_the_existing_b_marker_after_readline_redraws() {
        let mut view = View::new(3, 20);
        view.process_changes(b"\x1B]133;A\x07$ \x1B]133;B\x07old");
        assert_eq!(view.active_semantic_input().as_deref(), Some("old"));

        // Readline redraws prompt + recalled text, but emits no new OSC 133 B.
        view.process_changes(b"\r\x1B[K$ recalled");
        assert_eq!(view.active_semantic_input().as_deref(), Some("recalled"));
        view.process_changes(b"\x1B[4D");
        assert_eq!(view.active_semantic_input().as_deref(), Some("recalled"));

        view.process_changes(b"\r\n\x1B]133;C\x07");
        assert_eq!(view.active_semantic_input(), None);
    }
}

use super::{
    ext::ScreenExt,
    presentation::{
        PaneMediaStore, PresentationError, PresentedViewFrame, SurfaceId, ViewId, ViewRevision,
    },
    terminal::{
        GhosttyEngine, GhosttyReviewMark, HistoryPosition, SemanticKind as Osc133Kind,
        SemanticMark as Osc133Mark, TerminalEngine, TerminalGeometry, TerminalSnapshot,
        UpdateSummary, Viewport,
    },
};
use std::{
    cmp::min,
    sync::atomic::{AtomicU64, Ordering},
};

/// A bounded history avoids unbounded memory growth while retaining enough
/// output for extended review and semantic-prompt navigation.
pub const SCROLLBACK_LINES: usize = 10_000;

static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

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

pub struct View {
    view_id: ViewId,
    presentation_tracking: bool,
    live_revision: ViewRevision,
    presented_revision: ViewRevision,
    finalized_presented_revision: ViewRevision,
    live_history_revision: u64,
    presented_history_revision: u64,
    shared_live_history: Option<(u64, std::sync::Arc<[crate::terminal::Row]>)>,
    application_transaction_open: bool,
    unpresented_synchronized_output: bool,
    engine: GhosttyEngine,
    committed_snapshot: TerminalSnapshot,
    accessibility_read_state: AccessibilityReadState,
    media: PaneMediaStore,
    pending_update: UpdateSummary,
    /// Parser metadata which is safe to use with the currently presented
    /// accessibility snapshot. When a receipt arrives behind the live parser,
    /// this is deliberately empty: snapshot diffing remains exact, whereas
    /// the live summary may already contain text from a newer, invisible
    /// frame.
    presented_update: UpdateSummary,
    prev_screen: TerminalSnapshot,
    prev_screen_time: u128,
    review_cursor_position: (u16, u16),
    review_cursor_follow_pending: bool,
    review_cursor_screen_transition_pending: bool,
    review_scrollback: usize,
    retained_history_len: usize,
    review_mark: Option<GhosttyReviewMark>,
    review_cursor_indent_level: u16,
    application_cursor_indent_level: u16,
    cached_full: String,
    cached_prev_full: String,
    cached_full_valid: bool,
    cached_prev_full_valid: bool,
    cached_full_row_hashes: Vec<u64>,
    cached_prev_full_row_hashes: Vec<u64>,
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
        let cursor_position = engine.snapshot().cursor_position();
        let prev_screen = engine.snapshot().clone();
        let committed_snapshot = engine.snapshot_with_history();
        View {
            view_id: ViewId(NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed)),
            presentation_tracking: false,
            live_revision: ViewRevision(0),
            presented_revision: ViewRevision(0),
            finalized_presented_revision: ViewRevision(0),
            live_history_revision: 0,
            presented_history_revision: 0,
            shared_live_history: None,
            application_transaction_open: false,
            unpresented_synchronized_output: false,
            engine,
            committed_snapshot,
            accessibility_read_state: AccessibilityReadState::Live,
            media: PaneMediaStore::new(Default::default()),
            pending_update: UpdateSummary::default(),
            presented_update: UpdateSummary::default(),
            prev_screen,
            prev_screen_time: 0,
            review_cursor_position: cursor_position,
            review_cursor_follow_pending: false,
            review_cursor_screen_transition_pending: false,
            review_scrollback: 0,
            retained_history_len: 0,
            review_mark: None,
            review_cursor_indent_level: 0,
            application_cursor_indent_level: 0,
            cached_full: String::new(),
            cached_prev_full: String::new(),
            cached_full_valid: false,
            cached_prev_full_valid: false,
            cached_full_row_hashes: Vec::new(),
            cached_prev_full_row_hashes: Vec::new(),
        }
    }

    /// Processes new changes, updating the internal screen representation
    pub fn process_changes(&mut self, buf: &[u8]) {
        let _ = self.process_changes_inner(buf, false, true);
    }

    /// Processes one parser batch and returns that batch's update summary.
    ///
    /// When `retain_for_accessibility` is true, the view's pending summary
    /// still accumulates normally for finalization. Callers which only need the
    /// just-observed renderer/effect delta leave it false, avoiding both a
    /// cumulative merge and cloning [`Self::update_summary`], whose size can
    /// grow across many batches.
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
        let old_live_snapshot = self.engine.snapshot().clone();
        let old_review_scrollback = self.review_scrollback;
        let was_synchronized = self.engine.snapshot().modes.synchronized_output;
        let accessible_scrollback_before = self.scrollback();
        let review_mark_before = self.review_mark_position();

        // Output is always interpreted against the live drawing screen. The
        // selected review viewport is restored afterward.
        self.engine.select_viewport(Viewport::Live);
        let update = TerminalEngine::advance(&mut self.engine, buf);
        let synchronized_output_open_snapshot =
            self.engine.take_synchronized_output_open_snapshot();
        let synchronized = update.synchronized_output;
        self.application_transaction_open = synchronized;
        let synchronized_transaction_activity =
            was_synchronized || synchronized || update.synchronized_output_opened;
        let live_snapshot = self.engine.snapshot().clone();
        let batch_history_changed = update.history_changed
            || live_snapshot.scrollback_extent != old_live_snapshot.scrollback_extent
            || live_snapshot.history_origin != old_live_snapshot.history_origin;
        if self.presentation_tracking && batch_history_changed {
            self.live_history_revision = self
                .live_history_revision
                .checked_add(1)
                .expect("view history presentation revision exhausted");
            self.shared_live_history = None;
        }
        let screen_transition = update.screen_before != update.screen_after;
        if screen_transition {
            self.review_cursor_screen_transition_pending = true;
        }
        // This boundary flag drives the scheduler for the just-observed PTY
        // batch; unlike damage and printed runs it must not stay sticky until
        // speech finalization.
        let mut batch_update = if capture_batch {
            if retain_for_accessibility {
                let batch_update = update.clone();
                self.pending_update.synchronized_output_opened = false;
                self.pending_update.merge(update);
                Some(batch_update)
            } else {
                // There is no accessibility consumer for this pane. Move the
                // exact batch to its immediate renderer/effect consumer and
                // leave no cumulative vectors behind.
                self.pending_update = UpdateSummary::default();
                Some(update)
            }
        } else {
            self.pending_update.synchronized_output_opened = false;
            self.pending_update.merge(update);
            None
        };
        if self.presentation_tracking {
            self.advance_live_revision();
            self.unpresented_synchronized_output |= synchronized_transaction_activity;
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
        self.review_scrollback =
            translate_scrollback_offset(old_review_scrollback, &old_live_snapshot, &live_snapshot);
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
            self.publish_synchronized_output(live_snapshot, batch_history_changed);
            if let Some(update) = &mut batch_update {
                // Match the pending-summary contract: once an atomic update
                // closes, transient writes from inside it are not eligible for
                // auto-read or batch consumers either.
                update.printed_runs.clear();
            }
        } else {
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
            self.prev_screen = self.screen().clone();
        } else {
            let visible_offset = self.review_scrollback;
            self.engine.select_viewport(Viewport::Live);
            self.prev_screen = self.engine.snapshot().clone();
            self.engine
                .select_viewport(Viewport::Scrollback(visible_offset));
        }
        self.prev_screen_time = now_ms;
        self.pending_update = UpdateSummary::default();
        self.presented_update = UpdateSummary::default();
        if self.presentation_tracking {
            self.finalized_presented_revision = self.presented_revision;
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
        } else {
            self.committed_snapshot = self.live_snapshot_with_history();
        }
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
    /// do not use presentation receipts, so their legacy immediate behavior
    /// deliberately has no revision gate.
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
        let history = self.presentation_history_if_changed();
        PresentedViewFrame {
            view_id: self.view_id,
            revision: self.live_revision,
            surface_id,
            snapshot,
            history_revision: self.live_history_revision,
            history,
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
        PresentedViewFrame {
            view_id: self.view_id,
            revision: self.presented_revision,
            surface_id,
            snapshot: self.committed_presentation_snapshot(),
            history_revision: self.presented_history_revision,
            history: None,
        }
    }

    /// Publishes a model only after the render carrying it has completely
    /// flushed. Returns `false` for a frame routed to the wrong view, a frame
    /// from the future, or an obsolete duplicate.
    pub(crate) fn apply_presented_frame(&mut self, frame: PresentedViewFrame) -> bool {
        if !self.presentation_tracking
            || frame.view_id != self.view_id
            || frame.revision > self.live_revision
            || frame.revision < self.presented_revision
        {
            return false;
        }

        if frame.history_revision < self.presented_history_revision {
            return false;
        }
        let caught_up = frame.revision == self.live_revision;
        let mut snapshot = frame.snapshot;
        if frame.history_revision == self.presented_history_revision {
            snapshot.scrollback = std::mem::take(&mut self.committed_snapshot.scrollback);
        } else {
            let Some(history) = frame.history else {
                return false;
            };
            snapshot.scrollback = history.as_ref().to_vec();
            self.presented_history_revision = frame.history_revision;
        }
        self.presented_revision = frame.revision;
        let synchronized_accessibility_diff = self.unpresented_synchronized_output;
        self.install_presented_snapshot(snapshot, caught_up);
        if self.unpresented_synchronized_output {
            // Parser print runs include text overwritten inside an atomic
            // transaction. Diffing presented snapshots is the only truthful
            // source once any part of such a transaction is flushed.
            self.pending_update.printed_runs.clear();
            if caught_up {
                self.unpresented_synchronized_output = false;
            }
        }
        self.presented_update = if caught_up && !synchronized_accessibility_diff {
            self.pending_update.clone()
        } else {
            // `pending_update` has already advanced past this receipt. Its
            // printed runs and cursor hints are not safe for accessibility;
            // the two presented snapshots still provide an exact diff.
            UpdateSummary::default()
        };
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
        self.pending_update.printed_runs.clear();
        self.invalidate_visible_cache();
    }

    fn advance_live_revision(&mut self) {
        self.live_revision = ViewRevision(
            self.live_revision
                .0
                .checked_add(1)
                .expect("view presentation revision exhausted"),
        );
    }

    fn live_snapshot_with_history(&mut self) -> TerminalSnapshot {
        let visible_offset = self.review_scrollback;
        self.engine.select_viewport(Viewport::Live);
        let snapshot = self.engine.snapshot_with_history();
        self.engine
            .select_viewport(Viewport::Scrollback(visible_offset));
        snapshot
    }

    fn presentation_history_if_changed(
        &mut self,
    ) -> Option<std::sync::Arc<[crate::terminal::Row]>> {
        if self.live_history_revision == self.presented_history_revision {
            return None;
        }
        if let Some((revision, history)) = &self.shared_live_history
            && *revision == self.live_history_revision
        {
            return Some(std::sync::Arc::clone(history));
        }

        let history = std::sync::Arc::<[crate::terminal::Row]>::from(
            self.live_snapshot_with_history().scrollback,
        );
        self.shared_live_history = Some((self.live_history_revision, history.clone()));
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

    pub(crate) fn cancel_pending_screen_transition_follow(&mut self) {
        self.review_cursor_screen_transition_pending = false;
        self.review_cursor_follow_pending = false;
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
        &self.pending_update
    }

    /// Update metadata paired with [`Self::screen`]. This differs from
    /// [`Self::update_summary`] while the parser is ahead of the physical
    /// terminal: renderer/protocol consumers need the live summary, while
    /// accessibility must never consume hints from an unpresented frame.
    pub(crate) fn accessibility_update_summary(&self) -> &UpdateSummary {
        if self.presentation_tracking {
            &self.presented_update
        } else {
            &self.pending_update
        }
    }

    /// A view used as a shadow model may observe output without owning the
    /// application's PTY. Its terminal replies are useful to a real terminal
    /// owner but must not remain pending in an observational surface.
    pub(crate) fn discard_shadow_pty_replies(&mut self) {
        self.pending_update.pty_replies.clear();
    }

    /// Pane-scoped terminal side effects observed since the last finalized
    /// screen update. Borrowed callback data has already been copied into
    /// owned, normalized values before reaching this model.
    pub fn terminal_events(&self) -> &[crate::terminal::TerminalEvent] {
        &self.pending_update.effects.events
    }

    pub(crate) fn clear_update_summary(&mut self) {
        self.pending_update = UpdateSummary::default();
        self.presented_update = UpdateSummary::default();
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

    /// Moves the review cursor up a line.
    /// If skip_blank_lines is true,
    /// the review cursor will move up to the previous non blank line,
    /// or remain in place if this is the first non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_up(&mut self, skip_blank_lines: bool) -> bool {
        if !skip_blank_lines {
            if self.review_cursor_position.0 > 0 {
                self.review_cursor_position.0 -= 1;
                return true;
            }
            let history_len = self.scrollback_len();
            if self.scrollback() >= history_len {
                return false;
            }
            self.set_accessible_scrollback(self.scrollback().saturating_add(1));
            return true;
        }
        let original = self.current_history_position();
        while self.review_cursor_up(false) {
            if !self.line(self.review_cursor_position.0).trim().is_empty() {
                return true;
            }
        }
        self.set_review_history_position(original);
        false
    }

    /// Moves the review cursor down a line.
    /// If skip_blank_lines is true,
    /// the review cursor will move down to the next non blank line,
    /// or remain in place if this is the last non blank line.
    /// This method will return true only if the cursor moved.
    pub fn review_cursor_down(&mut self, skip_blank_lines: bool) -> bool {
        let last_row = self.size().0 - 1;
        if !skip_blank_lines {
            if self.review_cursor_position.0 < last_row {
                self.review_cursor_position.0 += 1;
                return true;
            }
            if self.scrollback() == 0 {
                return false;
            }
            self.set_accessible_scrollback(self.scrollback().saturating_sub(1));
            return true;
        }
        let original = self.current_history_position();
        while self.review_cursor_down(false) {
            if !self.line(self.review_cursor_position.0).trim().is_empty() {
                return true;
            }
        }
        self.set_review_history_position(original);
        false
    }

    pub fn osc133_marks(&self) -> &[Osc133Mark] {
        &self.screen().semantic_marks
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
        let alternate_screen = self.screen().alternate_screen();
        let current_marks: Vec<_> = self
            .osc133_marks()
            .iter()
            .filter(|mark| mark.alternate_screen == alternate_screen)
            .collect();
        let input_start = current_marks
            .iter()
            .rev()
            .find_map(|mark| match mark.kind {
                Osc133Kind::InputStart => Some(mark.position),
                Osc133Kind::PromptStart
                | Osc133Kind::CommandStart
                | Osc133Kind::CommandFinished { .. } => None,
            })?;
        let latest_phase = current_marks.last()?.kind;
        if !matches!(latest_phase, Osc133Kind::InputStart) {
            return None;
        }
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
        self.ensure_cached_full();
        self.ensure_cached_prev_full();
        (
            &self.cached_prev_full,
            &self.cached_full,
            &self.cached_prev_full_row_hashes,
            &self.cached_full_row_hashes,
        )
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
    let rows = snapshot
        .scrollback
        .iter()
        .chain(&snapshot.rows)
        .skip(start)
        .take(height)
        .cloned()
        .collect();
    let mut cursor = snapshot.cursor;
    if offset != 0 {
        cursor.visible = false;
    }
    TerminalSnapshot {
        rows,
        scrollback: Vec::new(),
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
    for row in &mut snapshot.rows {
        row.cells.resize(cols, crate::terminal::Cell::default());
        if row.cells.last().is_some_and(crate::terminal::Cell::is_wide) {
            *row.cells.last_mut().expect("row has a final cell") = crate::terminal::Cell::default();
        }
    }
    snapshot
        .rows
        .resize_with(usize::from(geometry.rows), || crate::terminal::Row {
            cells: vec![crate::terminal::Cell::default(); cols],
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
    let rows = snapshot.scrollback.iter().chain(&snapshot.rows);
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
    use super::{View, compute_row_hashes, fnv1a_64};
    use crate::{
        presentation::SurfaceId,
        terminal::{HistoryPosition, SemanticKind as Osc133Kind, Viewport},
    };

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
        assert!(!view.application_transaction_open());
        assert!(view.accessibility_awaiting_presentation());
        assert_eq!(view.line(0), "old");

        assert!(view.apply_presented_frame(closed));
        assert_eq!(view.line(0), "committed");
        assert!(!view.accessibility_awaiting_presentation());
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
            alternate.history.as_deref().map(<[_]>::len),
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
            history.iter().map(|row| row.contents()).collect::<Vec<_>>(),
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
        assert!(view.review_cursor_up(false));
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
    fn frozen_scrollback_navigation_invalidates_the_visible_content_cache() {
        let mut view = View::new(2, 12);
        view.process_changes(b"one\r\ntwo\r\nthree");
        view.process_changes(b"\x1b[?2026h\x1b[2J\x1b[Hpartial");

        assert_eq!(view.contents_full(), "two\nthree\n");
        assert!(view.review_cursor_up(false));
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
    fn line_navigation_crosses_the_live_viewport_into_scrollback() {
        let mut view = View::new(3, 12);
        view.process_changes(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        assert_eq!(view.scrollback_len(), 2);
        assert_eq!(view.line(0), "three");

        assert!(view.review_cursor_up(false));
        assert_eq!(view.scrollback(), 1);
        assert_eq!(view.line(0), "two");
        assert!(view.review_cursor_up(false));
        assert_eq!(view.scrollback(), 2);
        assert_eq!(view.line(0), "one");
        assert!(!view.review_cursor_up(false));
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
        view.set_review_cursor_position((0, 0));
        assert!(view.review_cursor_up(false));
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

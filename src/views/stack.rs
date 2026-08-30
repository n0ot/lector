use super::ViewController;
use crate::{
    presentation::{
        GridPoint, GridRect, PresentedAccessibilityBundle, PresentedFrameIndex, PresentedViewFrame,
        Scene, SurfaceId,
    },
    terminal::{TerminalGeometry, TerminalSnapshot},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompositorTransitionToken(u64);

pub struct ViewStack {
    views: Vec<Box<dyn ViewController>>,
    /// Views removed from the logical stack but still owned by an in-flight
    /// physical render. Screen-derived commands may continue reading one
    /// until the replacement scene flushes.
    retired_views: Vec<Box<dyn ViewController>>,
    base_count: usize,
    active_base: usize,
    next_compositor_transition: u64,
    compositor_transition: Option<CompositorTransitionToken>,
    presentation_tracking: bool,
    virtual_terminal_colors: crate::terminal_protocol::VirtualTerminalColors,
}

impl ViewStack {
    pub fn new(mut root: Box<dyn ViewController>) -> Self {
        // The root context is already active when the stack is constructed;
        // its independent review cursor must not be mistaken for an
        // uninitialized overlay on the first later compositor announcement.
        root.model().mark_review_context_active();
        root.model().mark_accessible_document_context_active();
        Self {
            views: vec![root],
            retired_views: Vec::new(),
            base_count: 1,
            active_base: 0,
            next_compositor_transition: 1,
            compositor_transition: None,
            presentation_tracking: false,
            virtual_terminal_colors: crate::terminal_protocol::VirtualTerminalColors::for_scheme(
                crate::terminal_protocol::ColorScheme::Dark,
            ),
        }
    }

    pub fn active_mut(&mut self) -> &mut dyn ViewController {
        let index = if self.has_overlay() {
            self.views.len().saturating_sub(1)
        } else {
            self.active_base
        };
        self.views
            .get_mut(index)
            .expect("view stack should always have a root view")
            .as_mut()
    }

    pub fn root_mut(&mut self) -> &mut dyn ViewController {
        self.views
            .first_mut()
            .expect("view stack should always have a root view")
            .as_mut()
    }

    pub fn push(&mut self, mut view: Box<dyn ViewController>) {
        view.set_virtual_terminal_colors(self.virtual_terminal_colors);
        if self.presentation_tracking {
            view.enable_presentation_tracking();
        }
        if view.as_any().is::<crate::views::TmuxConnectionView>() {
            let had_overlay = self.has_overlay();
            self.clear_overlays();
            if !had_overlay {
                self.deactivate_active_document_context();
            }
            self.views.insert(self.base_count, view);
            self.active_base = self.base_count;
            self.base_count = self.base_count.saturating_add(1);
            self.begin_compositor_transition();
        } else {
            self.deactivate_active_document_context();
            self.views.push(view);
        }
    }

    pub(crate) fn enable_presentation_tracking(&mut self) {
        self.presentation_tracking = true;
        for view in &mut self.views {
            view.enable_presentation_tracking();
        }
    }

    pub(crate) fn set_virtual_terminal_colors(
        &mut self,
        colors: crate::terminal_protocol::VirtualTerminalColors,
    ) {
        self.virtual_terminal_colors = colors;
        for view in self.views.iter_mut().chain(&mut self.retired_views) {
            view.set_virtual_terminal_colors(colors);
        }
    }

    pub fn pop(&mut self) -> bool {
        if !self.has_overlay() {
            return false;
        }
        self.deactivate_active_document_context();
        let removed = self.views.pop();
        if self.presentation_tracking
            && let Some(view) = removed
        {
            self.retired_views.push(view);
        }
        if !self.has_overlay() {
            self.begin_compositor_transition();
        }
        true
    }

    pub(crate) fn clear_overlays(&mut self) {
        if self.has_overlay() {
            self.deactivate_active_document_context();
            self.begin_compositor_transition();
        }
        if self.presentation_tracking {
            self.retired_views
                .extend(self.views.drain(self.base_count..));
        } else {
            self.views.truncate(self.base_count);
        }
    }

    pub(crate) const fn compositor_transition_pending(&self) -> bool {
        self.compositor_transition.is_some()
    }

    pub(crate) const fn compositor_transition(&self) -> Option<CompositorTransitionToken> {
        self.compositor_transition
    }

    pub(crate) fn complete_compositor_transition(
        &mut self,
        transition: CompositorTransitionToken,
    ) -> bool {
        if self.compositor_transition != Some(transition) {
            return false;
        }
        self.compositor_transition = None;
        true
    }

    fn begin_compositor_transition(&mut self) {
        let transition = CompositorTransitionToken(self.next_compositor_transition);
        self.next_compositor_transition = self.next_compositor_transition.wrapping_add(1).max(1);
        self.compositor_transition = Some(transition);
    }

    pub(crate) fn presented_holds_synchronized_output(&mut self) -> bool {
        let view = &mut self.views[self.active_base];
        if let Some(connection) = view
            .as_any_mut()
            .downcast_mut::<crate::views::TmuxConnectionView>()
        {
            connection.visible_holds_synchronized_output()
        } else {
            view.model().holds_synchronized_output()
        }
    }

    pub(crate) fn remove_tmux_connections(&mut self, connection_ids: &[u64]) {
        let old_base_count = self.base_count;
        let active_connection = self.views[self.active_base]
            .as_any()
            .downcast_ref::<crate::views::TmuxConnectionView>()
            .map(crate::views::TmuxConnectionView::connection_id);
        if !self.has_overlay()
            && active_connection
                .is_some_and(|connection_id| connection_ids.contains(&connection_id))
        {
            self.deactivate_active_document_context();
        }
        let mut removed_base_count = 0usize;
        let mut retained = Vec::with_capacity(self.views.len());
        for (original_index, mut view) in std::mem::take(&mut self.views).into_iter().enumerate() {
            let is_base = original_index < old_base_count;
            let remove = is_base
                && view
                    .as_any_mut()
                    .downcast_mut::<crate::views::TmuxConnectionView>()
                    .is_some_and(|connection| connection_ids.contains(&connection.connection_id()));
            if remove {
                removed_base_count = removed_base_count.saturating_add(1);
                if self.presentation_tracking {
                    self.retired_views.push(view);
                }
            } else {
                retained.push(view);
            }
        }
        self.views = retained;
        self.base_count = old_base_count.saturating_sub(removed_base_count);
        self.active_base = match active_connection {
            Some(connection_id) if !connection_ids.contains(&connection_id) => self
                .tmux_connection_index(connection_id)
                .unwrap_or_else(|| self.base_count.saturating_sub(1)),
            Some(_) => self.base_count.saturating_sub(1),
            None => 0,
        };
        if active_connection.is_some_and(|connection_id| connection_ids.contains(&connection_id)) {
            self.begin_compositor_transition();
        }
    }

    pub(crate) fn activate_terminal(&mut self) {
        let had_overlay = self.has_overlay();
        self.clear_overlays();
        if self.active_base != 0 {
            if !had_overlay {
                self.deactivate_active_document_context();
            }
            self.begin_compositor_transition();
        }
        self.active_base = 0;
    }

    pub(crate) fn activate_tmux_connection(&mut self, connection_id: u64) -> bool {
        let Some(index) = self.tmux_connection_index(connection_id) else {
            return false;
        };
        let had_overlay = self.has_overlay();
        self.clear_overlays();
        if self.active_base != index {
            if !had_overlay {
                self.deactivate_active_document_context();
            }
            self.begin_compositor_transition();
        }
        self.active_base = index;
        true
    }

    fn deactivate_active_document_context(&mut self) {
        self.active_mut()
            .model()
            .deactivate_accessible_document_context();
    }

    fn tmux_connection_index(&mut self, connection_id: u64) -> Option<usize> {
        self.views[..self.base_count].iter_mut().position(|view| {
            view.as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()
                .is_some_and(|connection| connection.connection_id() == connection_id)
        })
    }

    pub(crate) fn tmux_connection_mut(
        &mut self,
        connection_id: u64,
    ) -> Option<&mut crate::views::TmuxConnectionView> {
        self.views[..self.base_count].iter_mut().find_map(|view| {
            let connection = view
                .as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()?;
            (connection.connection_id() == connection_id).then_some(connection)
        })
    }

    pub(crate) fn active_tmux_connection_mut(
        &mut self,
    ) -> Option<&mut crate::views::TmuxConnectionView> {
        if self.has_overlay() {
            return None;
        }
        self.views
            .get_mut(self.active_base)?
            .as_any_mut()
            .downcast_mut::<crate::views::TmuxConnectionView>()
    }

    pub(crate) fn presented_tmux_connection_mut(
        &mut self,
    ) -> Option<&mut crate::views::TmuxConnectionView> {
        self.views
            .get_mut(self.active_base)?
            .as_any_mut()
            .downcast_mut::<crate::views::TmuxConnectionView>()
    }

    pub(crate) fn active_tmux_chooser_mut(&mut self) -> Option<&mut crate::views::TmuxChooserView> {
        self.views
            .last_mut()?
            .as_any_mut()
            .downcast_mut::<crate::views::TmuxChooserView>()
    }

    pub(crate) fn active_tmux_connection_chooser_mut(
        &mut self,
    ) -> Option<&mut crate::views::TmuxConnectionChooserView> {
        if !self.has_overlay() {
            return None;
        }
        self.views
            .last_mut()
            .expect("an overlay should have a final view")
            .as_any_mut()
            .downcast_mut::<crate::views::TmuxConnectionChooserView>()
    }

    pub fn has_overlay(&self) -> bool {
        self.views.len() > self.base_count
    }

    pub(crate) fn live_snapshots(&mut self) -> Vec<TerminalSnapshot> {
        let overlay_count = self.views.len().saturating_sub(self.base_count);
        let mut snapshots = Vec::with_capacity(overlay_count.saturating_add(1));
        snapshots.push(
            self.views[self.active_base]
                .model()
                .with_live_screen(|model| model.live_screen().clone()),
        );
        snapshots.extend(self.views[self.base_count..].iter_mut().map(|view| {
            view.model()
                .with_live_screen(|model| model.live_screen().clone())
        }));
        snapshots
    }

    /// Snapshots suitable for a compositor-owned presentation. The hidden
    /// application contributes its committed live viewport, never its mutable
    /// working frame or the user's selected scrollback viewport. Overlays are
    /// themselves the compositor's live drawing surfaces, so their snapshots
    /// must stay current with the render receipt being constructed.
    pub(crate) fn committed_presentation_snapshots(&mut self) -> Vec<TerminalSnapshot> {
        let mut snapshots = vec![
            self.views[self.active_base]
                .model()
                .committed_presentation_snapshot(),
        ];
        snapshots.extend(self.views[self.base_count..].iter_mut().map(|view| {
            view.model()
                .with_live_screen(|model| model.live_screen().clone())
        }));
        snapshots
    }

    /// Captures the accessibility state represented by the scene currently
    /// being enqueued. The scheduler treats this bundle as opaque cargo and
    /// returns it only after that exact render has flushed.
    pub(crate) fn capture_presentation_bundle(
        &mut self,
        live_application: bool,
    ) -> PresentedAccessibilityBundle {
        let (active_label, tracks_terminal_title) =
            self.active_accessibility_label(live_application);
        let mut frames = Vec::<PresentedViewFrame>::new();
        let base_active_view;

        if let Some(connection) = self.presented_tmux_connection_mut()
            && connection.is_ready()
            && !connection.is_showing_portal()
        {
            let (active_view, pane_frames) = if live_application {
                let (active_view, pane_frames) = connection.capture_live_presentation_frames();
                (active_view, pane_frames)
            } else {
                connection.capture_committed_presentation_frames()
            };
            base_active_view = active_view;
            frames.extend(pane_frames);
        } else {
            let base = self.views[self.active_base].model();
            base_active_view = Some(base.view_id());
            if live_application {
                frames.push(base.capture_live_presentation_frame(SurfaceId(1)));
            } else {
                frames.push(base.capture_committed_presentation_frame(SurfaceId(1)));
            }
        }

        let mut active_view = base_active_view;
        for (overlay_index, view) in self.views[self.base_count..].iter_mut().enumerate() {
            let model = view.model();
            active_view = Some(model.view_id());
            let surface_id = SurfaceId(
                u64::try_from(overlay_index)
                    .unwrap_or(u64::MAX)
                    .saturating_add(2),
            );
            frames.push(model.capture_live_presentation_frame(surface_id));
        }

        PresentedAccessibilityBundle::new(active_view, frames)
            .with_active_label(active_label, tracks_terminal_title)
    }

    /// Returns the label represented by the scene being captured. Unlike a
    /// later controller lookup, this remains exact when a tmux location changes
    /// without replacing its active pane, or when an overlay transition is
    /// waiting behind terminal backpressure.
    pub(crate) fn active_accessibility_label(&mut self, live_application: bool) -> (String, bool) {
        let active = self.active_mut();
        if let Some(connection) = active
            .as_any()
            .downcast_ref::<crate::views::TmuxConnectionView>()
        {
            return (connection.accessible_title(), false);
        }
        if active.kind() == crate::views::ViewKind::Terminal {
            let title = if live_application {
                active.model().live_screen().title.as_deref()
            } else {
                active.model().screen().title.as_deref()
            };
            return (
                title.filter(|title| !title.is_empty()).map_or_else(
                    || "terminal".to_owned(),
                    |title| format!("terminal, {title}"),
                ),
                true,
            );
        }
        (active.title().to_owned(), false)
    }

    pub(crate) fn apply_presented_bundle(&mut self, bundle: &PresentedAccessibilityBundle) {
        let [first, rest @ ..] = bundle.frames.as_slice() else {
            return;
        };
        if rest.is_empty() {
            self.apply_presented_frame(first);
            return;
        }

        let frames = PresentedFrameIndex::new(&bundle.frames);
        for view in self.views.iter_mut().chain(&mut self.retired_views) {
            if let Some(connection) = view
                .as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()
            {
                connection.apply_presented_frames(&frames);
                continue;
            }
            let model = view.model();
            if let Some(frame) = frames.get(model.view_id()) {
                model.apply_presented_frame_ref(frame);
            }
        }
    }

    fn apply_presented_frame(&mut self, frame: &PresentedViewFrame) {
        for view in self.views.iter_mut().chain(&mut self.retired_views) {
            if let Some(connection) = view
                .as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()
            {
                if connection.apply_presented_frame(frame) {
                    break;
                }
            } else if view.model().apply_presented_frame_ref(frame) {
                break;
            }
        }
    }

    /// Drops logically removed views once neither the currently presented
    /// scene nor any scheduler-owned render receipt can select them again.
    pub(crate) fn retain_accessibility_views(&mut self, retained: &[crate::presentation::ViewId]) {
        for view in self.views.iter_mut().chain(&mut self.retired_views) {
            if let Some(connection) = view
                .as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()
            {
                connection.retain_accessibility_views(retained);
            }
        }
        self.retired_views.retain_mut(|view| {
            if let Some(connection) = view
                .as_any_mut()
                .downcast_mut::<crate::views::TmuxConnectionView>()
            {
                retained
                    .iter()
                    .any(|view_id| connection.model_by_id_mut(*view_id).is_some())
            } else {
                retained.contains(&view.model().view_id())
            }
        });
    }

    pub(crate) fn logical_active_view_id(&mut self) -> crate::presentation::ViewId {
        self.active_mut().model().view_id()
    }

    pub(crate) fn contains_view_id(&mut self, view_id: crate::presentation::ViewId) -> bool {
        self.model_by_id_mut(view_id).is_some()
    }

    pub(crate) fn model_by_id_mut(
        &mut self,
        view_id: crate::presentation::ViewId,
    ) -> Option<&mut crate::view::View> {
        for view in self.views.iter_mut().chain(&mut self.retired_views) {
            if view.kind() == crate::views::ViewKind::TmuxConnection {
                let connection = view
                    .as_any_mut()
                    .downcast_mut::<crate::views::TmuxConnectionView>()
                    .expect("tmux connection view kind must match its controller type");
                if let Some(model) = connection.model_by_id_mut(view_id) {
                    return Some(model);
                }
                continue;
            }
            let model = view.model();
            if model.view_id() == view_id {
                return Some(model);
            }
        }
        None
    }

    pub(crate) fn overlay_snapshots(&mut self) -> Vec<TerminalSnapshot> {
        self.views[self.base_count..]
            .iter_mut()
            .map(|view| {
                view.model()
                    .with_live_screen(|model| model.live_screen().clone())
            })
            .collect()
    }

    pub(crate) fn append_live_media(&mut self, scene: &mut Scene) -> anyhow::Result<()> {
        let indices = std::iter::once(self.active_base)
            .chain(self.base_count..self.views.len())
            .collect::<Vec<_>>();
        for (surface_index, view_index) in indices.into_iter().enumerate() {
            let view = &mut self.views[view_index];
            let id = SurfaceId(
                u64::try_from(surface_index)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            );
            view.model()
                .with_live_screen(|model| -> anyhow::Result<()> {
                    let geometry = model.live_screen().geometry;
                    model.presentation_media()?.append_to_scene(
                        id,
                        GridPoint::new(0, 0),
                        GridRect::new(GridPoint::new(0, 0), geometry.rows, geometry.cols),
                        scene,
                    )?;
                    Ok(())
                })?;
        }
        Ok(())
    }

    /// Appends media only for surfaces whose live model is committed. The
    /// terminal snapshot does not retain historical Kitty pixel state, so a
    /// frozen surface must not leak newly-mutated working-frame media while a
    /// committed underlay is being revealed.
    pub(crate) fn append_committed_presentation_media(
        &mut self,
        scene: &mut Scene,
    ) -> anyhow::Result<()> {
        let indices = std::iter::once(self.active_base)
            .chain(self.base_count..self.views.len())
            .collect::<Vec<_>>();
        for (surface_index, view_index) in indices.into_iter().enumerate() {
            let view = &mut self.views[view_index];
            let model = view.model();
            if model.holds_synchronized_output() {
                continue;
            }
            let id = SurfaceId(
                u64::try_from(surface_index)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            );
            model.with_live_screen(|model| -> anyhow::Result<()> {
                let geometry = model.live_screen().geometry;
                model.presentation_media()?.append_to_scene(
                    id,
                    GridPoint::new(0, 0),
                    GridRect::new(GridPoint::new(0, 0), geometry.rows, geometry.cols),
                    scene,
                )?;
                Ok(())
            })?;
        }
        Ok(())
    }

    pub(crate) fn append_overlay_media(&mut self, scene: &mut Scene) -> anyhow::Result<()> {
        for (overlay_index, view) in self.views[self.base_count..].iter_mut().enumerate() {
            let id = SurfaceId(
                u64::try_from(overlay_index)
                    .unwrap_or(u64::MAX)
                    .saturating_add(2),
            );
            view.model()
                .with_live_screen(|model| -> anyhow::Result<()> {
                    let geometry = model.live_screen().geometry;
                    model.presentation_media()?.append_to_scene(
                        id,
                        GridPoint::new(0, 0),
                        GridRect::new(GridPoint::new(0, 0), geometry.rows, geometry.cols),
                        scene,
                    )?;
                    Ok(())
                })?;
        }
        Ok(())
    }

    pub fn on_resize(&mut self, rows: u16, cols: u16) {
        self.on_resize_with_geometry(TerminalGeometry::from_cells(rows, cols));
    }

    pub fn on_resize_with_geometry(&mut self, geometry: TerminalGeometry) {
        for view in &mut self.views {
            view.on_resize_with_geometry(geometry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ViewStack;
    use crate::{
        presentation::PresentedAccessibilityBundle,
        tmux_control::CommandStatus,
        tmux_model::TmuxTopology,
        views::{MessageView, PtyView, TmuxConnectionView},
    };

    fn ready_tmux_connection(connection_id: u64) -> TmuxConnectionView {
        const LAYOUT: &str = "abcd,20x4,0,0{10x4,0,0,20,9x4,11,0,21}";
        let lines = [
            b"S\t$1\twork".to_vec(),
            format!("W\t$1\t@10\t1\t1\t{LAYOUT}\t{LAYOUT}\t*\teditor").into_bytes(),
            b"P\t@10\t%20\t1\t1\t0\t0\t10\t4\t0\t0\t0\t1\t0\t0\t0\t0\tleft".to_vec(),
            b"P\t@10\t%21\t2\t0\t11\t0\t9\t4\t0\t0\t0\t1\t0\t0\t0\t0\tright".to_vec(),
            b"A\t$1".to_vec(),
        ];
        let mut topology = TmuxTopology::new(connection_id);
        topology.replace_inventory(&lines).expect("topology");
        let mut connection = TmuxConnectionView::new(4, 20, connection_id);
        let requests = connection.sync_topology(&topology).expect("sync topology");
        for request in requests {
            connection
                .apply_bootstrap(
                    request.pane_id,
                    CommandStatus::Success,
                    &[format!("pane {}", request.pane_id.0).into_bytes()],
                    0,
                )
                .expect("bootstrap pane");
        }
        connection
    }

    #[test]
    fn root_cannot_be_popped_and_overlay_push_pop_restores_it() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));

        assert!(!stack.has_overlay());
        assert!(!stack.pop());
        assert_eq!(stack.active_mut().title(), "Terminal");

        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));
        assert!(stack.has_overlay());
        assert_eq!(stack.root_mut().title(), "Terminal");
        assert_eq!(stack.active_mut().title(), "Notice");
        assert!(stack.pop());
        assert!(!stack.has_overlay());
        assert_eq!(stack.active_mut().title(), "Terminal");
    }

    #[test]
    fn an_older_compositor_receipt_cannot_complete_a_newer_transition() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));

        stack.push(Box::new(MessageView::new(4, 10, "First", "body")));
        assert!(stack.pop());
        let first = stack
            .compositor_transition()
            .expect("first overlay pop starts a transition");

        stack.push(Box::new(MessageView::new(4, 10, "Second", "body")));
        assert!(stack.pop());
        let second = stack
            .compositor_transition()
            .expect("second overlay pop supersedes the transition");
        assert_ne!(first, second);

        assert!(!stack.complete_compositor_transition(first));
        assert_eq!(stack.compositor_transition(), Some(second));
        assert!(stack.complete_compositor_transition(second));
        assert!(!stack.compositor_transition_pending());
    }

    #[test]
    fn resize_updates_every_view_in_the_stack() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));

        stack.on_resize(7, 20);

        assert_eq!(stack.root_mut().model().size(), (7, 20));
        assert_eq!(stack.active_mut().model().size(), (7, 20));
    }

    #[test]
    fn multi_view_receipt_routes_each_indexed_frame_to_its_owner() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.enable_presentation_tracking();
        stack.root_mut().model().process_changes(b"root");
        let root = stack.logical_active_view_id();

        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));
        stack.active_mut().model().process_changes(b"overlay");
        let overlay = stack.logical_active_view_id();
        let bundle = stack.capture_presentation_bundle(true);
        assert_eq!(bundle.frames.len(), 2);

        stack.apply_presented_bundle(&bundle);

        assert!(
            !stack
                .model_by_id_mut(root)
                .expect("root frame owner")
                .accessibility_awaiting_presentation()
        );
        assert!(
            !stack
                .model_by_id_mut(overlay)
                .expect("overlay frame owner")
                .accessibility_awaiting_presentation()
        );
    }

    #[test]
    fn tmux_connections_are_independent_bases_and_removal_preserves_overlays() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.push(Box::new(TmuxConnectionView::new(4, 10, 1)));
        stack.push(Box::new(TmuxConnectionView::new(4, 10, 2)));
        assert_eq!(
            stack
                .active_mut()
                .as_any()
                .downcast_ref::<TmuxConnectionView>()
                .unwrap()
                .connection_id(),
            2
        );

        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));
        stack.remove_tmux_connections(&[2]);
        assert!(stack.has_overlay());
        assert_eq!(stack.active_mut().title(), "Notice");
        assert!(stack.pop());
        assert_eq!(
            stack
                .active_mut()
                .as_any()
                .downcast_ref::<TmuxConnectionView>()
                .unwrap()
                .connection_id(),
            1
        );

        stack.activate_terminal();
        assert_eq!(stack.active_mut().title(), "Terminal");
    }

    #[test]
    fn removed_tmux_connection_remains_addressable_until_replacement_is_presented() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.enable_presentation_tracking();
        stack.push(Box::new(ready_tmux_connection(1)));
        let removed_view = stack.logical_active_view_id();
        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));
        let replacement_view = stack.logical_active_view_id();

        stack.remove_tmux_connections(&[1]);

        assert!(stack.has_overlay());
        assert!(stack.contains_view_id(removed_view));
        stack.apply_presented_bundle(&PresentedAccessibilityBundle::new(
            Some(removed_view),
            Vec::new(),
        ));
        assert!(stack.contains_view_id(removed_view));

        stack.apply_presented_bundle(&PresentedAccessibilityBundle::new(
            Some(replacement_view),
            Vec::new(),
        ));
        stack.retain_accessibility_views(&[replacement_view]);
        assert!(!stack.contains_view_id(removed_view));
        assert!(stack.contains_view_id(replacement_view));
    }

    #[test]
    fn retired_overlays_are_bounded_by_presented_and_scheduler_owned_view_ids() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.enable_presentation_tracking();
        let root = stack.logical_active_view_id();

        for index in 0..64 {
            stack.push(Box::new(MessageView::new(
                4,
                10,
                "Notice",
                format!("body {index}"),
            )));
            let overlay = stack.logical_active_view_id();
            assert!(stack.pop());
            stack.retain_accessibility_views(&[root, overlay]);
            assert!(stack.contains_view_id(overlay));
            stack.retain_accessibility_views(&[root]);
            assert!(!stack.contains_view_id(overlay));
        }
    }
}

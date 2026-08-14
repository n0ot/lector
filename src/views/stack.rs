use super::ViewController;
use crate::{
    presentation::{GridPoint, GridRect, Scene, SurfaceId},
    terminal::{TerminalGeometry, TerminalSnapshot},
};

pub struct ViewStack {
    views: Vec<Box<dyn ViewController>>,
    base_count: usize,
    active_base: usize,
}

impl ViewStack {
    pub fn new(root: Box<dyn ViewController>) -> Self {
        Self {
            views: vec![root],
            base_count: 1,
            active_base: 0,
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

    pub fn push(&mut self, view: Box<dyn ViewController>) {
        if view.as_any().is::<crate::views::TmuxConnectionView>() {
            self.clear_overlays();
            self.views.insert(self.base_count, view);
            self.active_base = self.base_count;
            self.base_count = self.base_count.saturating_add(1);
        } else {
            self.views.push(view);
        }
    }

    pub fn pop(&mut self) -> bool {
        if !self.has_overlay() {
            return false;
        }
        self.views.pop();
        true
    }

    pub(crate) fn clear_overlays(&mut self) {
        self.views.truncate(self.base_count);
    }

    pub(crate) fn remove_tmux_connections(&mut self, connection_ids: &[u64]) {
        let old_base_count = self.base_count;
        let active_connection = self.views[self.active_base]
            .as_any()
            .downcast_ref::<crate::views::TmuxConnectionView>()
            .map(crate::views::TmuxConnectionView::connection_id);
        let mut original_index = 0;
        let mut removed_base_count = 0;
        self.views.retain_mut(|view| {
            let is_base = original_index < old_base_count;
            original_index = original_index.saturating_add(1);
            let remove = is_base
                && view
                    .as_any_mut()
                    .downcast_mut::<crate::views::TmuxConnectionView>()
                    .is_some_and(|connection| connection_ids.contains(&connection.connection_id()));
            removed_base_count += usize::from(remove);
            !remove
        });
        self.base_count = old_base_count.saturating_sub(removed_base_count);
        self.active_base = match active_connection {
            Some(connection_id) if !connection_ids.contains(&connection_id) => self
                .tmux_connection_index(connection_id)
                .unwrap_or_else(|| self.base_count.saturating_sub(1)),
            Some(_) => self.base_count.saturating_sub(1),
            None => 0,
        };
    }

    pub(crate) fn activate_terminal(&mut self) {
        self.clear_overlays();
        self.active_base = 0;
    }

    pub(crate) fn activate_tmux_connection(&mut self, connection_id: u64) -> bool {
        let Some(index) = self.tmux_connection_index(connection_id) else {
            return false;
        };
        self.clear_overlays();
        self.active_base = index;
        true
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
        let indices = std::iter::once(self.active_base)
            .chain(self.base_count..self.views.len())
            .collect::<Vec<_>>();
        indices
            .into_iter()
            .map(|index| {
                let view = &mut self.views[index];
                view.model()
                    .with_live_screen(|model| model.screen().clone())
            })
            .collect()
    }

    pub(crate) fn overlay_snapshots(&mut self) -> Vec<TerminalSnapshot> {
        self.views[self.base_count..]
            .iter_mut()
            .map(|view| {
                view.model()
                    .with_live_screen(|model| model.screen().clone())
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
                    let geometry = model.screen().geometry;
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
                    let geometry = model.screen().geometry;
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
    use crate::views::{MessageView, PtyView, TmuxConnectionView};

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
    fn resize_updates_every_view_in_the_stack() {
        let mut stack = ViewStack::new(Box::new(PtyView::new(4, 10)));
        stack.push(Box::new(MessageView::new(4, 10, "Notice", "body")));

        stack.on_resize(7, 20);

        assert_eq!(stack.root_mut().model().size(), (7, 20));
        assert_eq!(stack.active_mut().model().size(), (7, 20));
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
}

use super::ViewController;
use crate::{
    presentation::{GridPoint, GridRect, Scene, SurfaceId},
    terminal::{TerminalGeometry, TerminalSnapshot},
};

pub struct ViewStack {
    views: Vec<Box<dyn ViewController>>,
}

impl ViewStack {
    pub fn new(root: Box<dyn ViewController>) -> Self {
        Self { views: vec![root] }
    }

    pub fn active_mut(&mut self) -> &mut dyn ViewController {
        self.views
            .last_mut()
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
        self.views.push(view);
    }

    pub fn pop(&mut self) -> bool {
        if self.views.len() <= 1 {
            return false;
        }
        self.views.pop();
        true
    }

    pub fn has_overlay(&self) -> bool {
        self.views.len() > 1
    }

    pub(crate) fn live_snapshots(&mut self) -> Vec<TerminalSnapshot> {
        self.views
            .iter_mut()
            .map(|view| {
                view.model()
                    .with_live_screen(|model| model.screen().clone())
            })
            .collect()
    }

    pub(crate) fn append_live_media(&mut self, scene: &mut Scene) -> anyhow::Result<()> {
        for (index, view) in self.views.iter_mut().enumerate() {
            let id = SurfaceId(u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1));
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
    use crate::views::{MessageView, PtyView};

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
}

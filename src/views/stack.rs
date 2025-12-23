use super::ViewController;

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

    pub fn on_resize(&mut self, rows: u16, cols: u16) {
        for view in &mut self.views {
            view.on_resize(rows, cols);
        }
    }
}

//! Source-owned navigation memory. Captures retain validity guards, not live
//! terminal references: frozen Review remains navigable while its source runs.
use super::{document::SearchDirection, parser::FindDirection};
use crate::terminal::HistoryPosition;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

#[derive(Clone, Debug)]
pub(crate) struct LastSearch {
    pub query: String,
    pub direction: SearchDirection,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LastFind {
    pub direction: FindDirection,
    pub till: bool,
    pub target: char,
}

#[derive(Clone)]
struct Mark {
    row: usize,
    col: u16,
    validity: Rc<Cell<usize>>,
}

#[derive(Default)]
pub(crate) struct NavigationMemory {
    pub search: Option<LastSearch>,
    pub find: Option<LastFind>,
    marks: [Option<Mark>; 26],
    captures: Vec<Weak<Cell<usize>>>,
}

pub(crate) type SharedNavigation = Rc<RefCell<NavigationMemory>>;

impl NavigationMemory {
    /// Conservatively invalidate captured coordinates at/after the first row
    /// whose identity may have changed. Earlier immutable history stays valid.
    pub fn invalidate_from(&mut self, row: usize) {
        self.captures.retain(|weak| {
            if let Some(validity) = weak.upgrade() {
                validity.set(validity.get().min(row));
                true
            } else {
                false
            }
        });
    }

    pub fn capture(&mut self, origin: usize, rows: usize, current: bool) -> NavigationCapture {
        self.captures.retain(|weak| weak.strong_count() != 0);
        let validity = Rc::new(Cell::new(if current { usize::MAX } else { 0 }));
        self.captures.push(Rc::downgrade(&validity));
        let marks = std::array::from_fn(|index| {
            self.marks[index].as_ref().and_then(|mark| {
                let row = mark.row.checked_sub(origin)?;
                (mark.row < mark.validity.get() && row < rows)
                    .then_some(HistoryPosition { row, col: mark.col })
            })
        });
        NavigationCapture {
            origin,
            validity,
            marks,
        }
    }

    pub fn set_mark(&mut self, capture: &NavigationCapture, name: char, position: HistoryPosition) {
        self.marks[mark_index(name)] = Some(Mark {
            row: capture.origin.saturating_add(position.row),
            col: position.col,
            validity: capture.validity.clone(),
        });
    }

    pub fn has_mark(&self, name: char) -> bool {
        self.marks[mark_index(name)].is_some()
    }
}

pub(crate) fn mark_index(name: char) -> usize {
    name as usize - 'a' as usize
}

pub(crate) struct NavigationCapture {
    origin: usize,
    validity: Rc<Cell<usize>>,
    pub marks: [Option<HistoryPosition>; 26],
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absolute_marks_translate_only_within_retained_lineage() {
        let mut memory = NavigationMemory::default();
        let capture = memory.capture(40, 10, true);
        memory.set_mark(&capture, 'a', HistoryPosition { row: 3, col: 2 });
        assert_eq!(
            memory.capture(42, 12, true).marks[0],
            Some(HistoryPosition { row: 1, col: 2 })
        );
        assert_eq!(memory.capture(44, 12, true).marks[0], None);
        memory.invalidate_from(43);
        assert_eq!(memory.capture(40, 10, true).marks[0], None);
    }
    #[test]
    fn unpresented_capture_does_not_grant_persistent_identity() {
        let mut memory = NavigationMemory::default();
        let capture = memory.capture(0, 10, false);
        memory.set_mark(&capture, 'a', HistoryPosition { row: 2, col: 0 });
        assert_eq!(memory.capture(0, 10, true).marks[0], None);
    }
}

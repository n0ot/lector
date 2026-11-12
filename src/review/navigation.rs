//! Source-owned navigation memory. Captures retain validity guards, not live
//! terminal references: frozen Review remains navigable while its source runs.
use super::{
    document::{SearchDirection, SearchMode},
    parser::FindDirection,
};
use crate::terminal::HistoryPosition;
use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

#[derive(Clone, Debug)]
pub(crate) struct LastSearch {
    pub query: String,
    pub direction: SearchDirection,
    pub mode: SearchMode,
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
    jumps: JumpList,
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
            jumps: self.jumps.capture(origin, rows),
        }
    }

    pub fn set_mark(&mut self, capture: &NavigationCapture, name: char, position: HistoryPosition) {
        self.marks[mark_index(name)] = Some(Mark {
            row: capture.origin.saturating_add(position.row),
            col: position.col,
            validity: capture.validity.clone(),
        });
    }

    pub fn record_jump(&mut self, capture: &mut NavigationCapture, position: HistoryPosition) {
        capture.jumps.record(Mark {
            row: position.row,
            col: position.col,
            validity: capture.validity.clone(),
        });
        self.save_jumps(capture);
    }

    pub fn travel_jump(
        &mut self,
        capture: &mut NavigationCapture,
        position: HistoryPosition,
        forward: bool,
        count: usize,
    ) -> Option<HistoryPosition> {
        let target = capture.jumps.travel(
            Mark {
                row: position.row,
                col: position.col,
                validity: capture.validity.clone(),
            },
            forward,
            count,
        )?;
        self.save_jumps(capture);
        Some(target)
    }

    fn save_jumps(&mut self, capture: &NavigationCapture) {
        self.jumps = capture.jumps.clone();
        for entry in &mut self.jumps.entries {
            entry.row = entry.row.saturating_add(capture.origin);
        }
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
    jumps: JumpList,
}

/// Neovim's default jumplist keeps later entries after a new jump from the
/// middle. Review keeps distinct columns on a line so terminal-line searches
/// remain reversible. A capture owns
/// frozen coordinates; validity is consulted only when opening a new capture.
#[derive(Clone, Default)]
struct JumpList {
    entries: Vec<Mark>,
    index: usize,
}

impl JumpList {
    const LIMIT: usize = 100;

    fn record(&mut self, position: Mark) {
        self.entries
            .retain(|entry| entry.row != position.row || entry.col != position.col);
        self.entries.push(position);
        if self.entries.len() > Self::LIMIT {
            self.entries.remove(0);
        }
        self.index = self.entries.len();
    }

    fn travel(&mut self, current: Mark, forward: bool, count: usize) -> Option<HistoryPosition> {
        // An unavailable count must not alter either the index or saved return
        // position. Save the live endpoint only when starting backward travel.
        let mut next = self.clone();
        if !forward && next.index == next.entries.len() {
            next.record(current);
            next.index = next.entries.len().checked_sub(1)?;
        }
        let index = if forward {
            next.index.checked_add(count)?
        } else {
            next.index.checked_sub(count)?
        };
        let target = next.entries.get(index)?;
        let position = HistoryPosition {
            row: target.row,
            col: target.col,
        };
        next.index = index;
        *self = next;
        Some(position)
    }

    fn capture(&self, origin: usize, rows: usize) -> Self {
        let mut result = Self::default();
        for (index, entry) in self.entries.iter().enumerate() {
            let Some(row) = entry.row.checked_sub(origin) else {
                continue;
            };
            if entry.row >= entry.validity.get() || row >= rows {
                continue;
            }
            result.entries.push(Mark {
                row,
                ..entry.clone()
            });
            if index < self.index {
                result.index += 1;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn position(row: usize, col: u16) -> HistoryPosition {
        HistoryPosition { row, col }
    }

    #[test]
    fn jump_order_counts_and_new_jumps_preserve_forward_entries() {
        let mut memory = NavigationMemory::default();
        let mut capture = memory.capture(0, 200, true);
        for p in [position(1, 2), position(4, 3), position(8, 4)] {
            memory.record_jump(&mut capture, p);
        }
        let current = position(12, 5);
        assert_eq!(
            memory.travel_jump(&mut capture, current, false, 2),
            Some(position(4, 3))
        );
        assert_eq!(
            memory.travel_jump(&mut capture, position(4, 3), true, 2),
            Some(current)
        );
        assert_eq!(
            memory.travel_jump(&mut capture, current, false, 2),
            Some(position(4, 3))
        );
        // Default Neovim ordering keeps the later locations, moves this origin
        // to the end, and resets the traversal index for the new jump.
        memory.record_jump(&mut capture, position(4, 3));
        assert_eq!(
            memory.travel_jump(&mut capture, position(20, 1), false, 1),
            Some(position(4, 3))
        );
        assert_eq!(
            memory.travel_jump(&mut capture, position(4, 3), false, 1),
            Some(current)
        );
        assert_eq!(memory.travel_jump(&mut capture, current, false, 999), None);
        assert_eq!(
            memory.travel_jump(&mut capture, current, true, 1),
            Some(position(4, 3))
        );
    }

    #[test]
    fn same_line_jumps_keep_exact_columns_and_history_is_bounded() {
        let mut memory = NavigationMemory::default();
        let mut capture = memory.capture(0, 200, true);
        memory.record_jump(&mut capture, position(1, 2));
        assert_eq!(
            memory.travel_jump(&mut capture, position(1, 9), false, 1),
            Some(position(1, 2))
        );
        assert_eq!(
            memory.travel_jump(&mut capture, position(1, 2), true, 1),
            Some(position(1, 9))
        );
        for row in 0..150 {
            memory.record_jump(&mut capture, position(row, 0));
        }
        assert_eq!(memory.jumps.entries.len(), JumpList::LIMIT);
        assert_eq!(memory.jumps.entries[0].row, 50);
    }

    #[test]
    fn jump_captures_translate_and_expire_without_mutating_frozen_history() {
        let mut memory = NavigationMemory::default();
        let mut capture = memory.capture(40, 10, true);
        memory.record_jump(&mut capture, position(3, 2));
        memory.record_jump(&mut capture, position(5, 4));
        let mut shifted = memory.capture(42, 10, true);
        assert_eq!(
            memory.travel_jump(&mut shifted, position(6, 1), false, 1),
            Some(position(3, 4))
        );
        memory.invalidate_from(43);
        assert_eq!(
            memory.travel_jump(&mut capture, position(8, 1), false, 1),
            Some(position(5, 4))
        );
        assert!(memory.capture(40, 10, true).jumps.entries.is_empty());
        let mut unpresented = memory.capture(0, 10, false);
        memory.record_jump(&mut unpresented, position(2, 0));
        assert!(memory.capture(0, 10, true).jumps.entries.is_empty());
    }

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

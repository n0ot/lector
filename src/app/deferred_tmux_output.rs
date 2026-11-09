use crate::tmux_model::PaneId;
use std::collections::{BTreeMap, VecDeque};

type PaneKey = (u64, PaneId);
const LIMIT_BYTES: usize = 64 * 1024;

pub(super) enum EnqueueResult {
    Queued { pane_bytes: usize },
    Overflow { dropped_bytes: usize },
}

/// Owns deferred pane bytes and their round-robin drain order together.
/// Transport, visibility, and pause/resynchronization decisions belong to App.
#[derive(Default)]
pub(super) struct DeferredTmuxOutput {
    panes: BTreeMap<PaneKey, VecDeque<u8>>,
    order: VecDeque<PaneKey>,
    bytes: usize,
}

impl DeferredTmuxOutput {
    pub(super) fn contains(&self, key: PaneKey) -> bool {
        self.panes.contains_key(&key)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub(super) fn len_bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn panes(&self) -> impl Iterator<Item = PaneKey> + '_ {
        self.panes.keys().copied()
    }

    pub(super) fn enqueue(&mut self, key: PaneKey, bytes: &[u8]) -> EnqueueResult {
        if self.bytes.saturating_add(bytes.len()) > LIMIT_BYTES {
            return EnqueueResult::Overflow {
                dropped_bytes: self.remove(key),
            };
        }
        if !self.panes.contains_key(&key) {
            self.order.push_back(key);
        }
        let pane = self.panes.entry(key).or_default();
        pane.extend(bytes);
        self.bytes += bytes.len();
        EnqueueResult::Queued {
            pane_bytes: pane.len(),
        }
    }

    pub(super) fn remove(&mut self, key: PaneKey) -> usize {
        let dropped = self.panes.remove(&key).map_or(0, |pane| pane.len());
        self.bytes -= dropped;
        self.order.retain(|candidate| *candidate != key);
        dropped
    }

    pub(super) fn remove_connections(&mut self, connections: &[u64]) {
        let removed = self
            .panes()
            .filter(|(connection, _)| connections.contains(connection))
            .collect::<Vec<_>>();
        for key in removed {
            self.remove(key);
        }
    }

    /// Remove one pane's next chunk, rotating an unfinished pane to the back.
    /// The caller spends its turn budget and processes each chunk immediately.
    pub(super) fn drain_next(&mut self, budget: usize) -> Option<(PaneKey, Vec<u8>)> {
        if budget == 0 {
            return None;
        }
        let key = self.order.pop_front()?;
        let pane = self
            .panes
            .get_mut(&key)
            .expect("scheduled pane is retained");
        let count = budget.min(pane.len());
        let bytes = pane.drain(..count).collect();
        if pane.is_empty() {
            self.panes.remove(&key);
        } else {
            self.order.push_back(key);
        }
        self.bytes -= count;
        Some((key, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const A: PaneKey = (1, PaneId(1));
    const B: PaneKey = (1, PaneId(2));
    const C: PaneKey = (2, PaneId(1));

    fn assert_consistent(queue: &DeferredTmuxOutput) {
        assert_eq!(
            queue.bytes,
            queue.panes.values().map(VecDeque::len).sum::<usize>()
        );
        assert!(queue.bytes <= LIMIT_BYTES);
        assert_eq!(queue.order.len(), queue.panes.len());
        assert_eq!(
            queue.order.iter().copied().collect::<BTreeSet<_>>(),
            queue.panes().collect()
        );
    }

    #[test]
    fn interleaved_panes_preserve_bytes_and_rotate_partial_drains() {
        let mut queue = DeferredTmuxOutput::default();
        for (key, bytes) in [(A, b"ab".as_slice()), (B, b"XYZ"), (A, b"cde")] {
            assert!(matches!(
                queue.enqueue(key, bytes),
                EnqueueResult::Queued { .. }
            ));
            assert_consistent(&queue);
        }
        assert!(queue.drain_next(0).is_none());
        for (budget, expected) in [
            (2, (A, b"ab".to_vec())),
            (4, (B, b"XYZ".to_vec())),
            (4, (A, b"cde".to_vec())),
        ] {
            assert_eq!(queue.drain_next(budget), Some(expected));
            assert_consistent(&queue);
        }
        assert!(queue.is_empty());
        assert!(queue.drain_next(1).is_none());
    }

    #[test]
    fn overflow_drops_only_the_rejected_panes_existing_bytes() {
        let mut queue = DeferredTmuxOutput::default();
        assert!(matches!(
            queue.enqueue(C, &vec![b'x'; LIMIT_BYTES + 1]),
            EnqueueResult::Overflow { dropped_bytes: 0 }
        ));
        assert!(queue.is_empty());
        assert_consistent(&queue);
        queue.enqueue(A, &vec![b'a'; LIMIT_BYTES / 2]);
        queue.enqueue(B, &vec![b'b'; LIMIT_BYTES / 2]);
        assert!(matches!(
            queue.enqueue(C, b"x"),
            EnqueueResult::Overflow { dropped_bytes: 0 }
        ));
        assert_consistent(&queue);
        assert!(
            matches!(queue.enqueue(A, b"x"), EnqueueResult::Overflow { dropped_bytes } if dropped_bytes == LIMIT_BYTES / 2)
        );
        assert!(!queue.contains(A));
        assert_consistent(&queue);
        // Re-enqueuing a removed pane schedules it after retained panes.
        queue.enqueue(A, b"new");
        assert_eq!(
            queue.drain_next(LIMIT_BYTES),
            Some((B, vec![b'b'; LIMIT_BYTES / 2]))
        );
        assert_eq!(queue.drain_next(LIMIT_BYTES), Some((A, b"new".to_vec())));
        assert_consistent(&queue);
    }

    #[test]
    fn pane_and_connection_removal_clear_accounting_and_schedule() {
        let mut queue = DeferredTmuxOutput::default();
        queue.enqueue(A, b"abc");
        queue.enqueue(B, b"de");
        queue.enqueue(C, b"fghi");
        assert_eq!(queue.remove(B), 2);
        assert_eq!(queue.remove(B), 0);
        assert_consistent(&queue);
        queue.remove_connections(&[1, 99]);
        assert_eq!(queue.len_bytes(), 4);
        assert_consistent(&queue);
        assert_eq!(queue.drain_next(2), Some((C, b"fg".to_vec())));
        queue.remove_connections(&[2]);
        assert!(queue.is_empty());
        assert_consistent(&queue);
    }

    #[test]
    fn empty_panes_are_scheduled_once_and_removed_when_drained() {
        let mut queue = DeferredTmuxOutput::default();
        queue.enqueue(A, b"");
        queue.enqueue(A, b"");
        queue.enqueue(B, b"b");
        assert_consistent(&queue);
        assert_eq!(queue.drain_next(1), Some((A, Vec::new())));
        assert_eq!(queue.drain_next(1), Some((B, b"b".to_vec())));
        assert_consistent(&queue);
    }
}

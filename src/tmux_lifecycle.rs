//! Connection ownership and lifecycle decisions that do not depend on the UI.

use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

const MAX_CONNECTIONS: usize = 4_096;
const MAX_CONNECTION_DEPTH: usize = 64;

/// The transport through which a tmux control connection was discovered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayOrigin {
    /// A control marker arrived directly from Lector's root PTY.
    Direct,
    /// A nested control marker arrived in a pane owned by another connection.
    Pane {
        parent_connection_id: u64,
        session_id: u64,
        window_id: u64,
        pane_id: u64,
    },
}

/// Exceptional controls for the transport which owns a tmux control client.
/// These are separate from pane input so they cannot accidentally target the
/// child application visible inside tmux.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayControlAction {
    GracefulDetach,
    ForceAbandon,
}

impl GatewayControlAction {
    #[must_use]
    pub const fn requires_confirmation(self) -> bool {
        matches!(self, Self::ForceAbandon)
    }

    #[must_use]
    pub const fn transport_bytes(self) -> Option<&'static [u8]> {
        match self {
            Self::GracefulDetach => None,
            Self::ForceAbandon => Some(b"\x1c"),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum LifecycleError {
    #[error("tmux connection already exists")]
    DuplicateConnection,
    #[error("tmux parent connection does not exist")]
    MissingParent,
    #[error("tmux connection hierarchy exceeds its resource bound")]
    TooManyConnections,
    #[error("tmux connection hierarchy exceeds its nesting-depth bound")]
    TooDeep,
}

/// Tracks gateway ownership so destroying an outer pane cannot orphan children.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectionHierarchy {
    origins: BTreeMap<u64, GatewayOrigin>,
}

impl ConnectionHierarchy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        connection_id: u64,
        origin: GatewayOrigin,
    ) -> Result<(), LifecycleError> {
        if self.origins.contains_key(&connection_id) {
            return Err(LifecycleError::DuplicateConnection);
        }
        if self.origins.len() == MAX_CONNECTIONS {
            return Err(LifecycleError::TooManyConnections);
        }
        if let GatewayOrigin::Pane {
            parent_connection_id,
            ..
        } = origin
        {
            if parent_connection_id == connection_id
                || !self.origins.contains_key(&parent_connection_id)
            {
                return Err(LifecycleError::MissingParent);
            }
            let mut depth = 1_usize;
            let mut ancestor = parent_connection_id;
            loop {
                if depth >= MAX_CONNECTION_DEPTH {
                    return Err(LifecycleError::TooDeep);
                }
                match self.origins.get(&ancestor).copied() {
                    Some(GatewayOrigin::Direct) => break,
                    Some(GatewayOrigin::Pane {
                        parent_connection_id,
                        ..
                    }) => {
                        depth = depth.saturating_add(1);
                        ancestor = parent_connection_id;
                    }
                    None => return Err(LifecycleError::MissingParent),
                }
            }
        }
        self.origins.insert(connection_id, origin);
        Ok(())
    }

    #[must_use]
    pub fn contains(&self, connection_id: u64) -> bool {
        self.origins.contains_key(&connection_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.origins.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    #[must_use]
    pub fn origin(&self, connection_id: u64) -> Option<GatewayOrigin> {
        self.origins.get(&connection_id).copied()
    }

    /// Return a stable, deepest-first teardown order without changing the
    /// hierarchy. A graceful cascade waits for each connection in this order
    /// to actually leave control mode before asking its parent to detach.
    #[must_use]
    pub fn teardown_order(&self, connection_id: u64) -> Vec<u64> {
        if !self.origins.contains_key(&connection_id) {
            return Vec::new();
        }
        let mut ordered = Vec::new();
        self.collect_descendants(connection_id, &mut ordered);
        ordered.push(connection_id);
        ordered
    }

    /// Return every connection in a stable, deepest-first teardown order.
    /// Independent direct transports are ordered by connection id.
    #[must_use]
    pub fn all_teardown_order(&self) -> Vec<u64> {
        let roots = self
            .origins
            .iter()
            .filter_map(|(id, origin)| matches!(origin, GatewayOrigin::Direct).then_some(*id))
            .collect::<Vec<_>>();
        let mut ordered = Vec::with_capacity(self.origins.len());
        for root in roots {
            self.collect_descendants(root, &mut ordered);
            ordered.push(root);
        }
        ordered
    }

    /// Remove a connection and all descendants, deepest descendants first.
    pub fn remove_connection(&mut self, connection_id: u64) -> Vec<u64> {
        if !self.origins.contains_key(&connection_id) {
            return Vec::new();
        }
        let mut removed = Vec::new();
        self.collect_descendants(connection_id, &mut removed);
        removed.push(connection_id);
        for id in &removed {
            self.origins.remove(id);
        }
        removed
    }

    /// Resolve every child transported through a destroyed parent pane.
    pub fn remove_gateway_pane(&mut self, parent_connection_id: u64, pane_id: u64) -> Vec<u64> {
        let roots = self
            .origins
            .iter()
            .filter_map(|(id, origin)| match origin {
                GatewayOrigin::Pane {
                    parent_connection_id: parent,
                    pane_id: pane,
                    ..
                } if *parent == parent_connection_id && *pane == pane_id => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.remove_roots(&roots)
    }

    /// Resolve every child transported through any pane in a destroyed window.
    pub fn remove_gateway_window(&mut self, parent_connection_id: u64, window_id: u64) -> Vec<u64> {
        let roots = self
            .origins
            .iter()
            .filter_map(|(id, origin)| match origin {
                GatewayOrigin::Pane {
                    parent_connection_id: parent,
                    window_id: window,
                    ..
                } if *parent == parent_connection_id && *window == window_id => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.remove_roots(&roots)
    }

    fn remove_roots(&mut self, roots: &[u64]) -> Vec<u64> {
        let mut removed = Vec::new();
        for root in roots {
            if !self.origins.contains_key(root) {
                continue;
            }
            self.collect_descendants(*root, &mut removed);
            removed.push(*root);
        }
        let mut unique = BTreeSet::new();
        removed.retain(|id| unique.insert(*id));
        for id in &removed {
            self.origins.remove(id);
        }
        removed
    }

    fn collect_descendants(&self, parent: u64, output: &mut Vec<u64>) {
        let children = self
            .origins
            .iter()
            .filter_map(|(id, origin)| match origin {
                GatewayOrigin::Pane {
                    parent_connection_id,
                    ..
                } if *parent_connection_id == parent => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        for child in children {
            self.collect_descendants(child, output);
            output.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConnectionHierarchy, GatewayOrigin, LifecycleError};

    #[test]
    fn rejects_duplicate_or_orphaned_connections() {
        let mut hierarchy = ConnectionHierarchy::new();
        hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
        assert_eq!(
            hierarchy.insert(1, GatewayOrigin::Direct),
            Err(LifecycleError::DuplicateConnection)
        );
        assert_eq!(
            hierarchy.insert(
                2,
                GatewayOrigin::Pane {
                    parent_connection_id: 99,
                    session_id: 1,
                    window_id: 1,
                    pane_id: 1,
                }
            ),
            Err(LifecycleError::MissingParent)
        );
    }

    #[test]
    fn removing_a_root_is_postorder_and_idempotent() {
        let mut hierarchy = ConnectionHierarchy::new();
        hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
        hierarchy
            .insert(
                2,
                GatewayOrigin::Pane {
                    parent_connection_id: 1,
                    session_id: 1,
                    window_id: 10,
                    pane_id: 20,
                },
            )
            .unwrap();
        hierarchy
            .insert(
                3,
                GatewayOrigin::Pane {
                    parent_connection_id: 2,
                    session_id: 1,
                    window_id: 30,
                    pane_id: 40,
                },
            )
            .unwrap();

        assert_eq!(hierarchy.teardown_order(1), vec![3, 2, 1]);
        assert_eq!(hierarchy.remove_connection(1), vec![3, 2, 1]);
        assert!(hierarchy.remove_connection(1).is_empty());
        assert!(hierarchy.teardown_order(1).is_empty());
        assert!(hierarchy.is_empty());
    }

    #[test]
    fn all_teardown_order_covers_each_tree_deepest_first() {
        let mut hierarchy = ConnectionHierarchy::new();
        hierarchy.insert(10, GatewayOrigin::Direct).unwrap();
        hierarchy.insert(1, GatewayOrigin::Direct).unwrap();
        hierarchy
            .insert(
                2,
                GatewayOrigin::Pane {
                    parent_connection_id: 1,
                    session_id: 1,
                    window_id: 10,
                    pane_id: 20,
                },
            )
            .unwrap();
        hierarchy
            .insert(
                3,
                GatewayOrigin::Pane {
                    parent_connection_id: 2,
                    session_id: 1,
                    window_id: 30,
                    pane_id: 40,
                },
            )
            .unwrap();
        hierarchy
            .insert(
                4,
                GatewayOrigin::Pane {
                    parent_connection_id: 1,
                    session_id: 2,
                    window_id: 50,
                    pane_id: 60,
                },
            )
            .unwrap();

        assert_eq!(hierarchy.all_teardown_order(), vec![3, 2, 4, 1, 10]);
    }
}

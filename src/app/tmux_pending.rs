use super::*;
use crate::tmux_model::{SessionId, TmuxPaneIdentity, TmuxTopology, WindowId};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxPaneVisit {
    serial: u64,
    chooser: ViewId,
    pub destination: TmuxPaneDestination,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TmuxPaneDestination {
    pub pane: TmuxPaneIdentity,
    pub session_id: SessionId,
    window_id: WindowId,
}

pub(super) struct PendingTmuxPaneVisit {
    pub visit: TmuxPaneVisit,
    pub switches_session: bool,
    pub command_succeeded: Option<bool>,
    location_observed: bool,
}

impl TmuxPaneVisit {
    pub(super) fn command(self) -> PendingTmuxCommand {
        let destination = self.destination;
        PendingTmuxCommand {
            connection_id: destination.pane.connection_id,
            // tmux validates the entire destination before changing the client.
            // IDs also survive window renumbering while this command is queued.
            bytes: format!(
                "switch-client -E -t '${}:@{}.%{}'\n",
                destination.session_id.0, destination.window_id.0, destination.pane.pane_id.0,
            )
            .into_bytes(),
            expected_replies: vec![ExpectedTmuxReply::PaneVisit(self)],
            kind: PendingTmuxCommandKind::Ordinary,
        }
    }
}

impl App {
    fn pending_tmux_pane_rows(&self) -> Vec<(TmuxPaneIdentity, String)> {
        let current = self.view_stack.base_tmux_pane();
        let mut rows = Vec::new();
        for connection in &self.tmux_connections {
            if !connection.has_inventory || !self.tmux_gateway_path_is_reachable(connection.id) {
                continue;
            }
            let Some(view) = self.view_stack.tmux_connection(connection.id) else {
                continue;
            };
            let topology = &connection.topology;
            let attached = topology.attached_session();
            for pane in topology.panes().values() {
                let Some((kind, status)) = view.pending_reason(pane.id) else {
                    continue;
                };
                let Some(identity) = view.pane_identity(pane.id) else {
                    continue;
                };
                if Some(identity) == current || view.pane_portal_target(pane.id).is_some() {
                    continue;
                }
                let Some((session_id, index)) = pending_pane_window_link(topology, pane.window_id)
                else {
                    continue;
                };
                let mut parts = Vec::new();
                if self.active_tmux_connection != Some(connection.id) {
                    parts.push(topology.label().to_owned());
                }
                if Some(session_id) != attached {
                    let name = &topology.sessions()[&session_id].name;
                    if !name.is_empty() {
                        parts.push(format!("session {name}"));
                    }
                }
                parts.extend([format!("window {index}"), format!("pane {}", pane.index)]);
                let message = if let Some(status) = status {
                    if let Some(app) = status.app() {
                        parts.push(app.to_owned());
                    }
                    parts.push(status.reason().to_owned());
                    status.message()
                } else {
                    parts.push("bell".to_owned());
                    None
                };
                let mut label = parts.join(", ").replace(char::is_control, " ");
                if let Some(message) = message.map(str::trim).filter(|text| !text.is_empty()) {
                    label.push_str(". ");
                    // Keep speech bounded even when the application sends a
                    // protocol-sized message. Preserve whole Unicode scalars.
                    let mut chars = message.chars();
                    label.extend(chars.by_ref().take(160));
                    if chars.next().is_some() {
                        label.push('…');
                    }
                }
                rows.push((kind, identity, (session_id, index, pane.index), label));
            }
        }
        rows.sort_by_key(|(kind, identity, location, _)| {
            (*kind, identity.connection_id, *location, identity.pane_id)
        });
        rows.into_iter()
            .map(|(_, identity, _, label)| (identity, label))
            .collect()
    }

    pub fn show_pending_tmux_panes(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let items = self.pending_tmux_pane_rows();
        if items.is_empty() {
            sr.speak("No pending panes", false)?;
            return Ok(());
        }
        let (rows, cols) = self.view_stack.root_mut().model().live_size();
        let chooser = views::TmuxChooserView::pending_panes(rows, cols, &items);
        self.handle_view_action(sr, views::ViewAction::Push(Box::new(chooser)), term_out)
    }

    pub(super) fn refresh_pending_tmux_panes(&mut self) -> bool {
        if !self.view_stack.has_pending_tmux_pane_chooser() {
            return false;
        }
        let rows = self.pending_tmux_pane_rows();
        self.view_stack.sync_pending_tmux_pane_choosers(&rows)
    }

    pub(super) fn confirm_presented_tmux_pane(&mut self, pane: Option<TmuxPaneIdentity>) {
        let previous = std::mem::replace(&mut self.last_presented_tmux_pane, pane);
        if let Some(pane) = pane.filter(|pane| Some(*pane) != previous)
            && let Some(view) = self.view_stack.tmux_connection_mut(pane.connection_id)
        {
            view.acknowledge_pending(pane);
        }
    }

    pub(super) fn start_pending_tmux_pane_visit(
        &mut self,
        sr: &mut ScreenReader,
        pane: TmuxPaneIdentity,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if let Some((visit, result)) = self
            .pending_tmux_pane_visit
            .as_ref()
            .map(|pending| (pending.visit, pending.command_succeeded))
            && self.pending_tmux_pane_visit_is_current(visit)
            && result != Some(false)
        {
            return Ok(());
        }
        let chooser = self.view_stack.logical_active_view_id();
        let Some(connection) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == pane.connection_id)
        else {
            return self.pending_tmux_pane_unavailable(sr, term_out);
        };
        let topology = &connection.topology;
        let Some(window_id) = topology.pane(pane.pane_id).map(|pane| pane.window_id) else {
            return self.pending_tmux_pane_unavailable(sr, term_out);
        };
        let Some((session_id, _)) = pending_pane_window_link(topology, window_id) else {
            return self.pending_tmux_pane_unavailable(sr, term_out);
        };
        let destination = TmuxPaneDestination {
            pane,
            session_id,
            window_id,
        };
        let switches_session = topology.attached_session() != Some(session_id);
        if self
            .pending_tmux_session_switches
            .contains_key(&pane.connection_id)
            || !self.tmux_pane_destination_exists(destination)
            || !self.remember_tmux_gateway_parent_locations(pane.connection_id)
            || !self.queue_tmux_gateway_path_resumes(pane.connection_id)
        {
            return self.pending_tmux_pane_unavailable(sr, term_out);
        }
        self.tmux_pane_visit_serial = self
            .tmux_pane_visit_serial
            .checked_add(1)
            .context("tmux pane visit serial space is exhausted")?;
        let visit = TmuxPaneVisit {
            serial: self.tmux_pane_visit_serial,
            chooser,
            destination,
        };
        self.pending_tmux_pane_visit = Some(PendingTmuxPaneVisit {
            visit,
            switches_session,
            command_succeeded: None,
            location_observed: false,
        });
        if switches_session {
            if !self.queue_tmux_session_switch_to(
                pane.connection_id,
                session_id,
                Vec::new(),
                Some(visit),
            ) {
                self.pending_tmux_pane_visit = None;
                return self.pending_tmux_pane_unavailable(sr, term_out);
            }
        } else {
            self.pending_tmux_commands.push_back(visit.command());
        }
        Ok(())
    }

    fn tmux_pane_destination_exists(&self, destination: TmuxPaneDestination) -> bool {
        let pane = destination.pane;
        self.tmux_gateway_path_is_reachable(pane.connection_id)
            && self
                .view_stack
                .tmux_connection(pane.connection_id)
                .is_some_and(|view| {
                    view.pane_identity(pane.pane_id) == Some(pane)
                        && view.pane_portal_target(pane.pane_id).is_none()
                })
            && self
                .tmux_connections
                .iter()
                .find(|connection| connection.id == pane.connection_id)
                .is_some_and(|connection| {
                    let topology = &connection.topology;
                    topology
                        .pane(pane.pane_id)
                        .is_some_and(|pane| pane.window_id == destination.window_id)
                        && window_link_in_session(
                            topology,
                            destination.session_id,
                            destination.window_id,
                        )
                        .is_some()
                })
    }

    pub(super) fn pending_tmux_pane_visit_is_current(&mut self, visit: TmuxPaneVisit) -> bool {
        self.pending_tmux_pane_visit
            .as_ref()
            .is_some_and(|pending| pending.visit == visit)
            && self.view_stack.logical_active_view_id() == visit.chooser
            && self.tmux_pane_destination_exists(visit.destination)
    }

    pub(super) fn can_send_tmux_pane_visit(&mut self, visit: TmuxPaneVisit) -> bool {
        if !self.pending_tmux_pane_visit_is_current(visit) {
            return false;
        }
        let destination = visit.destination;
        let connection_id = destination.pane.connection_id;
        let Some(connection) = self
            .tmux_connections
            .iter()
            .find(|connection| connection.id == connection_id)
        else {
            return false;
        };
        if connection.topology.attached_session() == Some(destination.session_id) {
            return true;
        }
        // A queued same-session visit must never become an unplanned session
        // switch. Recheck carrier links immediately before routing the bytes.
        self.pending_tmux_session_switches
            .get(&connection_id)
            .is_some_and(|pending| pending.pane_visit == Some(visit))
            && connection
                .topology
                .session(destination.session_id)
                .is_some_and(|session| {
                    self.live_tmux_gateway_windows(connection_id)
                        .iter()
                        .all(|window| session.windows.values().any(|id| id == window))
                })
    }

    pub(super) fn reject_unsent_tmux_pane_visit(&mut self, visit: TmuxPaneVisit) {
        let connection_id = visit.destination.pane.connection_id;
        if self
            .pending_tmux_session_switches
            .get(&connection_id)
            .is_some_and(|pending| pending.pane_visit == Some(visit))
        {
            self.cancel_planned_tmux_session_switch(connection_id);
        }
        self.record_pending_tmux_pane_visit_reply(visit, false);
    }

    pub(super) fn record_pending_tmux_pane_visit_reply(
        &mut self,
        visit: TmuxPaneVisit,
        success: bool,
    ) {
        let Some(pending) = &mut self.pending_tmux_pane_visit else {
            return;
        };
        if pending.visit != visit {
            return;
        }
        pending.command_succeeded = Some(success);
        if success && !self.tmux_pane_visit_location_matches(visit.destination) {
            // Notifications can follow the command reply or be unavailable.
            // Request a generation newer than the reply to settle either case.
            self.queue_tmux_inventory(visit.destination.pane.connection_id);
        }
    }

    fn tmux_pane_visit_location_matches(&self, destination: TmuxPaneDestination) -> bool {
        self.tmux_pane_is_active_in_layout(destination) && self.tmux_pane_is_selected(destination)
    }

    fn tmux_pane_is_active_in_layout(&self, destination: TmuxPaneDestination) -> bool {
        self.view_stack
            .tmux_connection(destination.pane.connection_id)
            .is_some_and(|view| view.is_active_pane(destination.pane.pane_id))
    }

    fn tmux_pane_is_selected(&self, destination: TmuxPaneDestination) -> bool {
        self.tmux_connections
            .iter()
            .find(|connection| connection.id == destination.pane.connection_id)
            .is_some_and(|connection| {
                let topology = &connection.topology;
                topology.attached_session() == Some(destination.session_id)
                    && topology
                        .session(destination.session_id)
                        .and_then(|session| session.active_window)
                        == Some(destination.window_id)
                    && topology.attached_active_pane() == Some(destination.pane.pane_id)
            })
    }

    pub(super) fn settle_pending_tmux_pane_visit(
        &mut self,
        sr: &mut ScreenReader,
        inventory_finished_for: Option<u64>,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        let Some(pending) = &self.pending_tmux_pane_visit else {
            return Ok(());
        };
        let PendingTmuxPaneVisit {
            visit,
            switches_session,
            command_succeeded,
            location_observed,
        } = *pending;
        let connection_id = visit.destination.pane.connection_id;
        if self.view_stack.logical_active_view_id() != visit.chooser {
            self.pending_tmux_pane_visit = None;
            return Ok(());
        }
        if !self.tmux_pane_destination_exists(visit.destination)
            || command_succeeded == Some(false)
            || (switches_session
                && command_succeeded.is_none()
                && !self
                    .pending_tmux_session_switches
                    .contains_key(&connection_id))
        {
            self.pending_tmux_pane_visit = None;
            return self.pending_tmux_pane_unavailable(sr, term_out);
        }
        if command_succeeded != Some(true) {
            return Ok(());
        }
        let selected = self.tmux_pane_is_selected(visit.destination);
        if selected && let Some(pending) = &mut self.pending_tmux_pane_visit {
            pending.location_observed = true;
        }
        if !selected || !self.tmux_pane_is_active_in_layout(visit.destination) {
            // Once the target was selected, later focus elsewhere supersedes
            // this visit even while its first pane captures are outstanding.
            if (!selected && location_observed) || inventory_finished_for == Some(connection_id) {
                self.pending_tmux_pane_visit = None;
                return self.pending_tmux_pane_unavailable(sr, term_out);
            }
            return Ok(());
        }
        if !self
            .view_stack
            .tmux_connection(connection_id)
            .is_some_and(views::TmuxConnectionView::is_ready)
        {
            return Ok(());
        }
        self.pending_tmux_pane_visit = None;
        self.present_tmux_connection(connection_id, sr, term_out)?;
        Ok(())
    }

    fn pending_tmux_pane_unavailable(
        &mut self,
        sr: &mut ScreenReader,
        term_out: &mut dyn Write,
    ) -> Result<()> {
        if self.refresh_pending_tmux_panes() {
            self.render_active_view(term_out)?;
        }
        sr.speak("Pending pane unavailable", false)?;
        Ok(())
    }
}

/// Prefer the attached session and ignore private links used only to carry a
/// nested control transport. A linked pane still appears as just one row.
fn pending_pane_window_link(
    topology: &TmuxTopology,
    window_id: WindowId,
) -> Option<(SessionId, u32)> {
    topology
        .attached_session()
        .into_iter()
        .chain(topology.sessions().keys().copied())
        .find_map(|session_id| {
            Some((
                session_id,
                window_link_in_session(topology, session_id, window_id)?,
            ))
        })
}

fn window_link_in_session(
    topology: &TmuxTopology,
    session_id: SessionId,
    window_id: WindowId,
) -> Option<u32> {
    topology
        .session(session_id)?
        .windows
        .iter()
        .find_map(|(&index, &id)| {
            (id == window_id && !topology.is_internal_window_link(session_id, index, window_id))
                .then_some(index)
        })
}

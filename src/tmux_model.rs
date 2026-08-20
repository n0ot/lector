//! UI-independent tmux connection topology and notification reconciliation.

use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// One independently parsed control command is used for each inventory layer.
/// Keeping these as separate commands means a syntax error in one layer still
/// produces exactly one correlated reply and cannot strand the transaction.
pub const INVENTORY_COMMANDS: [&str; 12] = [
    "list-sessions -F 'S\t#{session_id}\t#{session_name}'\n",
    "list-windows -a -F 'W\t#{session_id}\t#{window_id}\t#{window_index}\t#{window_active}\t#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}'\n",
    "list-panes -a -F 'P\t#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_dead}\t#{cursor_x}\t#{cursor_y}\t#{cursor_flag}\t#{cursor_shape}\t#{alternate_on}\t#{pane_in_mode}\t#{pane_mode}\t#{history_size}\t#{pane_title}'\n",
    "display-message -p -F 'A\t#{session_id}\t#{host}'\n",
    "display-message -p -F 'O\tbase-index\t#{base-index}'\n",
    "display-message -p -F 'O\tpane-base-index\t#{pane-base-index}'\n",
    "display-message -p -F 'C\tclient_name\t#{client_name}'\n",
    "display-message -p -F 'O\tprefix\t#{prefix}'\n",
    "display-message -p -F 'O\tprefix2\t#{prefix2}'\n",
    "display-message -p -F 'O\tkey-table\t#{key-table}'\n",
    "display-message -p -F 'O\trepeat-time\t#{repeat-time}'\n",
    // The no-format form works with tmux 3.7b and emits canonical, reloadable
    // `bind-key` syntax.
    "list-keys -a\n",
];

pub const INVENTORY_REPLY_COUNT: usize = INVENTORY_COMMANDS.len();

/// Concatenated form retained for diagnostics, fixtures, and byte-level tests.
pub const INVENTORY_COMMAND: &str = concat!(
    "list-sessions -F 'S\t#{session_id}\t#{session_name}'\n",
    "list-windows -a -F 'W\t#{session_id}\t#{window_id}\t#{window_index}\t#{window_active}\t#{window_layout}\t#{window_visible_layout}\t#{window_flags}\t#{window_name}'\n",
    "list-panes -a -F 'P\t#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_left}\t#{pane_top}\t#{pane_width}\t#{pane_height}\t#{pane_dead}\t#{cursor_x}\t#{cursor_y}\t#{cursor_flag}\t#{cursor_shape}\t#{alternate_on}\t#{pane_in_mode}\t#{pane_mode}\t#{history_size}\t#{pane_title}'\n",
    "display-message -p -F 'A\t#{session_id}\t#{host}'\n",
    "display-message -p -F 'O\tbase-index\t#{base-index}'\n",
    "display-message -p -F 'O\tpane-base-index\t#{pane-base-index}'\n",
    "display-message -p -F 'C\tclient_name\t#{client_name}'\n",
    "display-message -p -F 'O\tprefix\t#{prefix}'\n",
    "display-message -p -F 'O\tprefix2\t#{prefix2}'\n",
    "display-message -p -F 'O\tkey-table\t#{key-table}'\n",
    "display-message -p -F 'O\trepeat-time\t#{repeat-time}'\n",
    "list-keys -a\n",
);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SessionId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WindowId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PaneId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WindowLink {
    pub session_id: SessionId,
    pub index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub windows: BTreeMap<u32, WindowId>,
    pub active_window: Option<WindowId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    pub id: WindowId,
    pub name: String,
    pub links: BTreeSet<WindowLink>,
    pub active_pane: Option<PaneId>,
    pub layout: String,
    pub visible_layout: String,
    pub flags: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    pub id: PaneId,
    pub window_id: WindowId,
    pub index: u32,
    pub title: String,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub dead: bool,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub cursor_visible: bool,
    pub cursor_shape: String,
    pub alternate_on: bool,
    pub pane_in_mode: u32,
    pub mode: String,
    pub history_size: u32,
}

/// Pane-local state sampled immediately around an authoritative capture.
///
/// Inventory is intentionally not used for recovery: alternate-screen, tmux
/// mode, cursor, and geometry state can all change between inventories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneCaptureMetadata {
    pub pane_id: PaneId,
    pub left: u32,
    pub top: u32,
    pub width: u32,
    pub height: u32,
    pub dead: bool,
    pub cursor_x: u32,
    pub cursor_y: u32,
    pub cursor_visible: bool,
    pub cursor_shape: String,
    pub alternate_on: bool,
    pub pane_in_mode: u32,
    pub history_size: u32,
}

impl PaneCaptureMetadata {
    #[must_use]
    pub fn capture_basis_matches(&self, other: &Self) -> bool {
        self.pane_id == other.pane_id
            && self.width == other.width
            && self.height == other.height
            && self.alternate_on == other.alternate_on
            && self.pane_in_mode == other.pane_in_mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxLocation {
    pub session_id: SessionId,
    pub session_name: String,
    pub window_id: WindowId,
    pub window_index: u32,
    pub window_name: String,
    pub pane_id: Option<PaneId>,
    pub pane_index: Option<u32>,
    pub pane_count: usize,
}

impl TmuxLocation {
    #[must_use]
    pub fn window_title(&self) -> String {
        if self.pane_count > 1
            && let Some(pane_index) = self.pane_index
        {
            format!("{}.{}: {}", self.window_index, pane_index, self.window_name)
        } else {
            format!("{}: {}", self.window_index, self.window_name)
        }
    }

    #[must_use]
    pub fn accessible_title(&self) -> String {
        format!("tmux, {}, {}", self.session_name, self.window_title())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxBinding {
    pub table: String,
    pub key: String,
    pub repeatable: bool,
    pub command: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileOutcome {
    Applied,
    Ignored,
    ResyncRequired,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TopologyError {
    #[error("malformed tmux inventory record")]
    MalformedInventory,
    #[error("invalid tmux {kind} id")]
    InvalidId { kind: &'static str },
    #[error("invalid tmux inventory number in {field}")]
    InvalidNumber { field: &'static str },
    #[error("invalid UTF-8 in tmux {field}")]
    InvalidUtf8 { field: &'static str },
    #[error("contradictory tmux inventory: {0}")]
    ContradictoryInventory(&'static str),
    #[error("connection label must contain 1 to 256 bytes")]
    InvalidLabel,
    #[error("malformed tmux notification")]
    MalformedNotification,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TmuxTopology {
    connection_id: u64,
    label: String,
    host: Option<String>,
    sessions: BTreeMap<SessionId, Session>,
    windows: BTreeMap<WindowId, Window>,
    panes: BTreeMap<PaneId, Pane>,
    attached_session: Option<SessionId>,
    options: BTreeMap<String, String>,
    client_info: BTreeMap<String, String>,
    bindings: BTreeMap<String, TmuxBinding>,
    key_tables: BTreeMap<String, BTreeMap<String, TmuxBinding>>,
    needs_resync: bool,
}

impl TmuxTopology {
    #[must_use]
    pub fn new(connection_id: u64) -> Self {
        Self {
            connection_id,
            label: format!("tmux {connection_id}"),
            host: None,
            sessions: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            attached_session: None,
            options: BTreeMap::new(),
            client_info: BTreeMap::new(),
            bindings: BTreeMap::new(),
            key_tables: BTreeMap::new(),
            needs_resync: false,
        }
    }

    #[must_use]
    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    pub fn set_label(&mut self, label: &str) -> Result<(), TopologyError> {
        let label = label.trim();
        if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
            return Err(TopologyError::InvalidLabel);
        }
        self.label = label.to_owned();
        Ok(())
    }

    #[must_use]
    pub fn sessions(&self) -> &BTreeMap<SessionId, Session> {
        &self.sessions
    }

    #[must_use]
    pub fn session(&self, id: SessionId) -> Option<&Session> {
        self.sessions.get(&id)
    }

    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    #[must_use]
    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.panes.get(&id)
    }

    #[must_use]
    pub fn panes(&self) -> &BTreeMap<PaneId, Pane> {
        &self.panes
    }

    #[must_use]
    pub fn windows(&self) -> &BTreeMap<WindowId, Window> {
        &self.windows
    }

    #[must_use]
    pub fn attached_session(&self) -> Option<SessionId> {
        self.attached_session
    }

    #[must_use]
    pub fn attached_active_pane(&self) -> Option<PaneId> {
        let session = self.session(self.attached_session?)?;
        self.window(session.active_window?)?.active_pane
    }

    #[must_use]
    pub fn attached_location(&self) -> Option<TmuxLocation> {
        let session = self.session(self.attached_session?)?;
        let window_id = session.active_window?;
        let (&window_index, _) = session
            .windows
            .iter()
            .find(|(_, candidate)| **candidate == window_id)?;
        let window = self.window(window_id)?;
        let pane_id = window.active_pane;
        let pane_index = pane_id.and_then(|pane_id| self.pane(pane_id).map(|pane| pane.index));
        let pane_count = self
            .panes
            .values()
            .filter(|pane| pane.window_id == window_id)
            .count();
        Some(TmuxLocation {
            session_id: session.id,
            session_name: session.name.clone(),
            window_id,
            window_index,
            window_name: window.name.clone(),
            pane_id,
            pane_index,
            pane_count,
        })
    }

    #[must_use]
    pub fn option(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    #[must_use]
    pub fn client_info(&self, name: &str) -> Option<&str> {
        self.client_info.get(name).map(String::as_str)
    }

    #[must_use]
    pub fn bindings(&self) -> &BTreeMap<String, TmuxBinding> {
        &self.bindings
    }

    #[must_use]
    pub fn binding(&self, key: &str) -> Option<&TmuxBinding> {
        self.bindings.get(key)
    }

    #[must_use]
    pub fn binding_in_table(&self, table: &str, key: &str) -> Option<&TmuxBinding> {
        if table == "prefix" {
            return self.binding(key);
        }
        self.key_tables.get(table)?.get(key)
    }

    #[must_use]
    pub fn has_key_table(&self, table: &str) -> bool {
        table == "prefix" && !self.bindings.is_empty() || self.key_tables.contains_key(table)
    }

    #[must_use]
    pub fn needs_resync(&self) -> bool {
        self.needs_resync
    }

    pub fn mark_resync_required(&mut self) {
        self.needs_resync = true;
    }

    /// Predict the state after Lector's targeted `copy-mode -q`. This keeps
    /// bootstrap capture commands behind that queued exit from using tmux's
    /// mode-only `capture-pane -M` path.
    pub fn clear_native_copy_mode(&mut self, pane_id: PaneId) {
        if let Some(pane) = self.panes.get_mut(&pane_id)
            && pane.mode == "copy-mode"
        {
            pane.pane_in_mode = 0;
            pane.mode.clear();
        }
    }

    pub fn update_pane_capture_metadata(
        &mut self,
        metadata: &PaneCaptureMetadata,
    ) -> Result<(), TopologyError> {
        let pane =
            self.panes
                .get_mut(&metadata.pane_id)
                .ok_or(TopologyError::ContradictoryInventory(
                    "pane capture metadata references a missing pane",
                ))?;
        pane.left = metadata.left;
        pane.top = metadata.top;
        pane.width = metadata.width;
        pane.height = metadata.height;
        pane.dead = metadata.dead;
        pane.cursor_x = metadata.cursor_x;
        pane.cursor_y = metadata.cursor_y;
        pane.cursor_visible = metadata.cursor_visible;
        pane.cursor_shape.clone_from(&metadata.cursor_shape);
        pane.alternate_on = metadata.alternate_on;
        pane.pane_in_mode = metadata.pane_in_mode;
        pane.history_size = metadata.history_size;
        Ok(())
    }

    pub fn replace_inventory(&mut self, lines: &[Vec<u8>]) -> Result<(), TopologyError> {
        let mut next = Self::new(self.connection_id);
        next.label.clone_from(&self.label);
        for line in lines {
            next.apply_inventory_line(line)?;
        }
        next.validate_inventory()?;
        *self = next;
        Ok(())
    }

    pub fn apply_notification(
        &mut self,
        name: &[u8],
        arguments: &[u8],
    ) -> Result<ReconcileOutcome, TopologyError> {
        match name {
            b"sessions-changed" => Ok(self.require_resync()),
            b"session-changed" => {
                let (id, name) = split_first(arguments)?;
                let id = parse_session_id(id)?;
                let name = text(name, "session name")?.to_owned();
                let missing = !self.sessions.contains_key(&id);
                self.sessions
                    .entry(id)
                    .or_insert_with(|| empty_session(id))
                    .name = name;
                self.attached_session = Some(id);
                Ok(if missing {
                    self.require_resync()
                } else {
                    ReconcileOutcome::Applied
                })
            }
            b"session-renamed" => {
                let Some(id) = self.attached_session else {
                    return Ok(self.require_resync());
                };
                let name = text(arguments, "session name")?.to_owned();
                let Some(session) = self.sessions.get_mut(&id) else {
                    return Ok(self.require_resync());
                };
                session.name = name;
                Ok(ReconcileOutcome::Applied)
            }
            b"session-window-changed" => {
                let (session, window) = split_first(arguments)?;
                let session_id = parse_session_id(session)?;
                let window_id = parse_window_id(single_field(window)?)?;
                let missing_session = !self.sessions.contains_key(&session_id);
                let missing_window = !self.windows.contains_key(&window_id);
                self.sessions
                    .entry(session_id)
                    .or_insert_with(|| empty_session(session_id))
                    .active_window = Some(window_id);
                self.windows
                    .entry(window_id)
                    .or_insert_with(|| empty_window(window_id));
                Ok(if missing_session || missing_window {
                    self.require_resync()
                } else {
                    ReconcileOutcome::Applied
                })
            }
            b"window-renamed" | b"unlinked-window-renamed" => {
                let (window, name) = split_first(arguments)?;
                let window_id = parse_window_id(window)?;
                let missing = !self.windows.contains_key(&window_id);
                self.windows
                    .entry(window_id)
                    .or_insert_with(|| empty_window(window_id))
                    .name = text(name, "window name")?.to_owned();
                Ok(if missing {
                    self.require_resync()
                } else {
                    ReconcileOutcome::Applied
                })
            }
            b"window-pane-changed" => {
                let (window, pane) = split_first(arguments)?;
                let window_id = parse_window_id(window)?;
                let pane_id = parse_pane_id(single_field(pane)?)?;
                let missing_window = !self.windows.contains_key(&window_id);
                let missing_pane = !self.panes.contains_key(&pane_id);
                self.windows
                    .entry(window_id)
                    .or_insert_with(|| empty_window(window_id))
                    .active_pane = Some(pane_id);
                self.panes
                    .entry(pane_id)
                    .or_insert_with(|| empty_pane(pane_id, window_id));
                Ok(if missing_window || missing_pane {
                    self.require_resync()
                } else {
                    ReconcileOutcome::Applied
                })
            }
            b"window-add" | b"unlinked-window-add" => {
                let window_id = parse_window_id(single_field(arguments)?)?;
                self.windows
                    .entry(window_id)
                    .or_insert_with(|| empty_window(window_id));
                Ok(self.require_resync())
            }
            b"window-close" => self.close_attached_window(arguments),
            b"unlinked-window-close" => {
                let window_id = parse_window_id(single_field(arguments)?)?;
                if self
                    .windows
                    .get(&window_id)
                    .is_some_and(|window| !window.links.is_empty())
                {
                    return Ok(self.require_resync());
                }
                self.remove_window(window_id);
                Ok(ReconcileOutcome::Applied)
            }
            b"layout-change" => {
                self.apply_layout_change(arguments)?;
                Ok(self.require_resync())
            }
            b"pane-mode-changed" => Ok(self.require_resync()),
            b"pane-exited" => {
                let pane_id = parse_pane_id(single_field(arguments)?)?;
                self.remove_pane(pane_id);
                Ok(ReconcileOutcome::Applied)
            }
            _ => Ok(ReconcileOutcome::Ignored),
        }
    }

    #[must_use]
    pub fn debug_dump(&self) -> String {
        use std::fmt::Write as _;
        let mut dump = format!("connection {}: {}\n", self.connection_id, self.label);
        for session in self.sessions.values() {
            let attached = if Some(session.id) == self.attached_session {
                " [attached]"
            } else {
                ""
            };
            let _ = writeln!(
                dump,
                "session ${}{}: {}",
                session.id.0, attached, session.name
            );
            for (index, window_id) in &session.windows {
                if let Some(window) = self.windows.get(window_id) {
                    let _ = writeln!(
                        dump,
                        "  window @{} index {}: {}",
                        window.id.0, index, window.name
                    );
                    for pane in self
                        .panes
                        .values()
                        .filter(|pane| pane.window_id == window.id)
                    {
                        let _ = writeln!(
                            dump,
                            "    pane %{} index {}: {}",
                            pane.id.0, pane.index, pane.title
                        );
                    }
                }
            }
        }
        dump
    }

    fn apply_inventory_line(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        match line.first().copied() {
            Some(b'S') => self.inventory_session(line),
            Some(b'W') => self.inventory_window(line),
            Some(b'P') => self.inventory_pane(line),
            Some(b'A') => self.inventory_attached(line),
            Some(b'O') => self.inventory_key_value(line, true),
            Some(b'C') => self.inventory_key_value(line, false),
            Some(b'B') => self.inventory_binding(line),
            Some(b'b') => self.inventory_legacy_binding(line),
            _ => Err(TopologyError::MalformedInventory),
        }
    }

    fn inventory_session(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let fields = split_inventory(line, 3)?;
        let id = parse_session_id(fields[1])?;
        let name = text(fields[2], "session name")?.to_owned();
        if self
            .sessions
            .insert(
                id,
                Session {
                    id,
                    name,
                    windows: BTreeMap::new(),
                    active_window: None,
                },
            )
            .is_some()
        {
            return Err(TopologyError::ContradictoryInventory(
                "duplicate session id",
            ));
        }
        Ok(())
    }

    fn inventory_window(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let fields = split_inventory(line, 9)?;
        let session_id = parse_session_id(fields[1])?;
        let window_id = parse_window_id(fields[2])?;
        let index = number(fields[3], "window index")?;
        let active = boolean(fields[4], "window active")?;
        let layout = text(fields[5], "window layout")?.to_owned();
        let visible_layout = text(fields[6], "window visible layout")?.to_owned();
        let flags = text(fields[7], "window flags")?.to_owned();
        let name = text(fields[8], "window name")?.to_owned();
        let session =
            self.sessions
                .get_mut(&session_id)
                .ok_or(TopologyError::ContradictoryInventory(
                    "window references missing session",
                ))?;
        if session.windows.insert(index, window_id).is_some() {
            return Err(TopologyError::ContradictoryInventory(
                "duplicate window index",
            ));
        }
        if active {
            session.active_window = Some(window_id);
        }
        let link = WindowLink { session_id, index };
        let window = self.windows.entry(window_id).or_insert_with(|| Window {
            id: window_id,
            name: name.clone(),
            links: BTreeSet::new(),
            active_pane: None,
            layout: layout.clone(),
            visible_layout: visible_layout.clone(),
            flags: flags.clone(),
        });
        if window.name != name || window.layout != layout || window.visible_layout != visible_layout
        {
            return Err(TopologyError::ContradictoryInventory(
                "linked window metadata differs",
            ));
        }
        window.flags = flags;
        window.links.insert(link);
        Ok(())
    }

    fn inventory_pane(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let tab_count = line.iter().filter(|byte| **byte == b'\t').count();
        let named_mode = tab_count >= 18;
        let extended = tab_count >= 17;
        let fields = split_inventory(
            line,
            if named_mode {
                19
            } else if extended {
                18
            } else {
                11
            },
        )?;
        let window_id = parse_window_id(fields[1])?;
        let pane_id = parse_pane_id(fields[2])?;
        let pane = Pane {
            id: pane_id,
            window_id,
            index: number(fields[3], "pane index")?,
            left: number(fields[5], "pane left")?,
            top: number(fields[6], "pane top")?,
            width: number(fields[7], "pane width")?,
            height: number(fields[8], "pane height")?,
            dead: boolean(fields[9], "pane dead")?,
            cursor_x: if extended {
                number(fields[10], "cursor x")?
            } else {
                0
            },
            cursor_y: if extended {
                number(fields[11], "cursor y")?
            } else {
                0
            },
            cursor_visible: if extended {
                boolean(fields[12], "cursor visible")?
            } else {
                true
            },
            cursor_shape: if extended {
                parse_cursor_shape(fields[13])?
            } else {
                "default".to_owned()
            },
            alternate_on: if extended {
                boolean(fields[14], "alternate screen")?
            } else {
                false
            },
            pane_in_mode: if extended {
                number(fields[15], "pane mode")?
            } else {
                0
            },
            mode: if named_mode {
                text(fields[16], "pane mode name")?.to_owned()
            } else {
                String::new()
            },
            history_size: if extended {
                number(fields[if named_mode { 17 } else { 16 }], "history size")?
            } else {
                0
            },
            title: text(
                fields[if named_mode {
                    18
                } else if extended {
                    17
                } else {
                    10
                }],
                "pane title",
            )?
            .to_owned(),
        };
        let active = boolean(fields[4], "pane active")?;
        let window =
            self.windows
                .get_mut(&window_id)
                .ok_or(TopologyError::ContradictoryInventory(
                    "pane references missing window",
                ))?;
        if active {
            window.active_pane = Some(pane_id);
        }
        if let Some(existing) = self.panes.get(&pane_id) {
            if existing != &pane {
                return Err(TopologyError::ContradictoryInventory(
                    "linked pane metadata differs",
                ));
            }
        } else {
            self.panes.insert(pane_id, pane);
        }
        Ok(())
    }

    fn inventory_attached(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let has_host = line.iter().filter(|byte| **byte == b'\t').count() >= 2;
        let fields = split_inventory(line, if has_host { 3 } else { 2 })?;
        let id = parse_session_id(fields[1])?;
        if self.attached_session.replace(id).is_some() {
            return Err(TopologyError::ContradictoryInventory(
                "multiple attached records",
            ));
        }
        if has_host {
            let host = text(fields[2], "server host")?;
            if host.len() > 256 || host.chars().any(char::is_control) {
                return Err(TopologyError::MalformedInventory);
            }
            self.host = (!host.is_empty()).then(|| host.to_owned());
        }
        Ok(())
    }

    fn inventory_key_value(&mut self, line: &[u8], option: bool) -> Result<(), TopologyError> {
        let fields = split_inventory(line, 3)?;
        if fields[1].is_empty() {
            return Err(TopologyError::MalformedInventory);
        }
        let key = text(fields[1], "metadata key")?.to_owned();
        let value = text(fields[2], "metadata value")?.to_owned();
        let map = if option {
            &mut self.options
        } else {
            &mut self.client_info
        };
        if map.insert(key, value).is_some() {
            return Err(TopologyError::ContradictoryInventory(
                "duplicate metadata key",
            ));
        }
        Ok(())
    }

    fn inventory_binding(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let has_table = line.iter().filter(|byte| **byte == b'\t').count() >= 4;
        let fields = split_inventory(line, if has_table { 5 } else { 4 })?;
        let table = if has_table {
            text(fields[1], "binding table")?
        } else {
            "prefix"
        };
        let key_index = usize::from(has_table) + 1;
        let repeat_index = key_index + 1;
        let command_index = repeat_index + 1;
        let key = text(fields[key_index], "binding key")?;
        let command = text(fields[command_index], "binding command")?;
        validate_binding(table, key, command)?;
        self.insert_binding(TmuxBinding {
            table: table.to_owned(),
            key: key.to_owned(),
            repeatable: boolean(fields[repeat_index], "binding repeatable")?,
            command: command.to_owned(),
        })
    }

    fn inventory_legacy_binding(&mut self, line: &[u8]) -> Result<(), TopologyError> {
        let mut lexer = LegacyBindingLexer::new(line);
        if lexer.next()?.as_deref() != Some(b"bind-key") {
            return Err(TopologyError::MalformedInventory);
        }

        let mut token = lexer.next()?.ok_or(TopologyError::MalformedInventory)?;
        let repeatable = token == b"-r";
        if repeatable {
            token = lexer.next()?.ok_or(TopologyError::MalformedInventory)?;
        }
        let table = match token.as_slice() {
            b"-T" => lexer.next()?.ok_or(TopologyError::MalformedInventory)?,
            // Treat the `-n` form as a root-table binding.
            b"-n" => b"root".to_vec(),
            _ => return Err(TopologyError::MalformedInventory),
        };
        // The key is positional after the table option and may itself begin
        // with `-`, so it must never be interpreted as another option.
        let key = lexer.next()?.ok_or(TopologyError::MalformedInventory)?;
        let command = lexer.remainder()?;
        let table = text(&table, "binding table")?;
        let key = text(&key, "binding key")?;
        let command = text(command, "binding command")?;
        validate_binding(table, key, command)?;
        self.insert_binding(TmuxBinding {
            table: table.to_owned(),
            key: key.to_owned(),
            repeatable,
            command: command.to_owned(),
        })
    }

    fn insert_binding(&mut self, binding: TmuxBinding) -> Result<(), TopologyError> {
        let table = binding.table.clone();
        let key = binding.key.clone();
        let bindings = if table == "prefix" {
            &mut self.bindings
        } else {
            self.key_tables.entry(table).or_default()
        };
        if bindings.insert(key, binding).is_some() {
            return Err(TopologyError::ContradictoryInventory(
                "duplicate key-table binding",
            ));
        }
        Ok(())
    }

    fn validate_inventory(&self) -> Result<(), TopologyError> {
        if let Some(attached) = self.attached_session
            && !self.sessions.contains_key(&attached)
        {
            return Err(TopologyError::ContradictoryInventory(
                "attached session is missing",
            ));
        }
        for pane in self.panes.values() {
            if !self.windows.contains_key(&pane.window_id) {
                return Err(TopologyError::ContradictoryInventory(
                    "pane window is missing",
                ));
            }
        }
        Ok(())
    }

    fn close_attached_window(
        &mut self,
        arguments: &[u8],
    ) -> Result<ReconcileOutcome, TopologyError> {
        let window_id = parse_window_id(single_field(arguments)?)?;
        let Some(session_id) = self.attached_session else {
            return Ok(self.require_resync());
        };
        if let Some(session) = self.sessions.get_mut(&session_id) {
            session.windows.retain(|_, id| *id != window_id);
            if session.active_window == Some(window_id) {
                session.active_window = None;
            }
        }
        if let Some(window) = self.windows.get_mut(&window_id) {
            window.links.retain(|link| link.session_id != session_id);
            if window.links.is_empty() {
                self.remove_window(window_id);
            }
        }
        Ok(ReconcileOutcome::Applied)
    }

    fn apply_layout_change(&mut self, arguments: &[u8]) -> Result<ReconcileOutcome, TopologyError> {
        let mut fields = arguments.splitn(4, |byte| *byte == b' ');
        let window_id =
            parse_window_id(fields.next().ok_or(TopologyError::MalformedNotification)?)?;
        let layout = text(
            fields.next().ok_or(TopologyError::MalformedNotification)?,
            "window layout",
        )?;
        let visible_layout = text(
            fields.next().ok_or(TopologyError::MalformedNotification)?,
            "window visible layout",
        )?;
        let flags = text(
            fields.next().ok_or(TopologyError::MalformedNotification)?,
            "window flags",
        )?;
        let missing = !self.windows.contains_key(&window_id);
        let window = self
            .windows
            .entry(window_id)
            .or_insert_with(|| empty_window(window_id));
        window.layout = layout.to_owned();
        window.visible_layout = visible_layout.to_owned();
        window.flags = flags.to_owned();
        Ok(if missing {
            self.require_resync()
        } else {
            ReconcileOutcome::Applied
        })
    }

    fn remove_pane(&mut self, pane_id: PaneId) {
        self.panes.remove(&pane_id);
        for window in self.windows.values_mut() {
            if window.active_pane == Some(pane_id) {
                window.active_pane = None;
            }
        }
    }

    fn remove_window(&mut self, window_id: WindowId) {
        self.windows.remove(&window_id);
        self.panes.retain(|_, pane| pane.window_id != window_id);
        for session in self.sessions.values_mut() {
            session.windows.retain(|_, id| *id != window_id);
            if session.active_window == Some(window_id) {
                session.active_window = None;
            }
        }
    }

    fn require_resync(&mut self) -> ReconcileOutcome {
        self.needs_resync = true;
        ReconcileOutcome::ResyncRequired
    }
}

fn validate_binding(table: &str, key: &str, command: &str) -> Result<(), TopologyError> {
    if table.is_empty()
        || table.len() > 128
        || key.is_empty()
        || key.len() > 128
        || command.is_empty()
        || command.len() > 64 * 1024
        || table
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n' | b'\t'))
        || key
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        || command
            .bytes()
            .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
    {
        return Err(TopologyError::MalformedInventory);
    }
    Ok(())
}

/// Lexer for the reloadable syntax printed by pre-`list-keys -F` tmux.
/// Only the `bind-key` prefix is decoded; the command remainder stays byte-for-
/// byte identical so Lector can safely send it back to the same tmux server.
struct LegacyBindingLexer<'a> {
    input: &'a [u8],
    offset: usize,
}

impl<'a> LegacyBindingLexer<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, offset: 0 }
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, TopologyError> {
        self.skip_whitespace();
        if self.offset == self.input.len() {
            return Ok(None);
        }

        let mut token = Vec::new();
        let mut quote = None;
        while self.offset < self.input.len() {
            let byte = self.input[self.offset];
            if quote.is_none() && byte.is_ascii_whitespace() {
                break;
            }
            self.offset += 1;
            match byte {
                b'\\' => {
                    let escaped = self
                        .input
                        .get(self.offset)
                        .copied()
                        .ok_or(TopologyError::MalformedInventory)?;
                    token.push(escaped);
                    self.offset += 1;
                }
                b'\'' | b'"' if quote == Some(byte) => quote = None,
                b'\'' | b'"' if quote.is_none() => quote = Some(byte),
                _ => token.push(byte),
            }
        }
        if quote.is_some() || token.is_empty() {
            return Err(TopologyError::MalformedInventory);
        }
        Ok(Some(token))
    }

    fn remainder(&mut self) -> Result<&'a [u8], TopologyError> {
        self.skip_whitespace();
        let remainder = &self.input[self.offset..];
        if remainder.is_empty() {
            return Err(TopologyError::MalformedInventory);
        }
        Ok(remainder)
    }

    fn skip_whitespace(&mut self) {
        while self
            .input
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }
}

fn empty_session(id: SessionId) -> Session {
    Session {
        id,
        name: String::new(),
        windows: BTreeMap::new(),
        active_window: None,
    }
}

fn empty_window(id: WindowId) -> Window {
    Window {
        id,
        name: String::new(),
        links: BTreeSet::new(),
        active_pane: None,
        layout: String::new(),
        visible_layout: String::new(),
        flags: String::new(),
    }
}

fn empty_pane(id: PaneId, window_id: WindowId) -> Pane {
    Pane {
        id,
        window_id,
        index: 0,
        title: String::new(),
        left: 0,
        top: 0,
        width: 0,
        height: 0,
        dead: false,
        cursor_x: 0,
        cursor_y: 0,
        cursor_visible: true,
        cursor_shape: "default".to_owned(),
        alternate_on: false,
        pane_in_mode: 0,
        mode: String::new(),
        history_size: 0,
    }
}

fn split_inventory(line: &[u8], fields: usize) -> Result<Vec<&[u8]>, TopologyError> {
    let parts = line
        .splitn(fields, |byte| *byte == b'\t')
        .collect::<Vec<_>>();
    if parts.len() != fields || parts[0].len() != 1 {
        return Err(TopologyError::MalformedInventory);
    }
    Ok(parts)
}

fn split_first(bytes: &[u8]) -> Result<(&[u8], &[u8]), TopologyError> {
    let index = bytes
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(TopologyError::MalformedNotification)?;
    if index == 0 || index + 1 == bytes.len() {
        return Err(TopologyError::MalformedNotification);
    }
    Ok((&bytes[..index], &bytes[index + 1..]))
}

fn single_field(bytes: &[u8]) -> Result<&[u8], TopologyError> {
    if bytes.is_empty() || bytes.contains(&b' ') {
        Err(TopologyError::MalformedNotification)
    } else {
        Ok(bytes)
    }
}

fn text<'a>(bytes: &'a [u8], field: &'static str) -> Result<&'a str, TopologyError> {
    std::str::from_utf8(bytes).map_err(|_| TopologyError::InvalidUtf8 { field })
}

fn number(bytes: &[u8], field: &'static str) -> Result<u32, TopologyError> {
    text(bytes, field)?
        .parse()
        .map_err(|_| TopologyError::InvalidNumber { field })
}

fn boolean(bytes: &[u8], field: &'static str) -> Result<bool, TopologyError> {
    match bytes {
        b"0" => Ok(false),
        b"1" => Ok(true),
        _ => Err(TopologyError::InvalidNumber { field }),
    }
}

fn parse_cursor_shape(bytes: &[u8]) -> Result<String, TopologyError> {
    let shape = text(bytes, "cursor shape")?;
    Ok(if shape.is_empty() { "default" } else { shape }.to_owned())
}

#[must_use]
pub fn pane_capture_metadata_command(pane_id: PaneId) -> Vec<u8> {
    format!(
        "display-message -p -t %{} -F 'R\t#{{pane_id}}\t#{{pane_left}}\t#{{pane_top}}\t#{{pane_width}}\t#{{pane_height}}\t#{{pane_dead}}\t#{{cursor_x}}\t#{{cursor_y}}\t#{{cursor_flag}}\t#{{cursor_shape}}\t#{{alternate_on}}\t#{{pane_in_mode}}\t#{{history_size}}'\n",
        pane_id.0
    )
    .into_bytes()
}

pub fn parse_pane_capture_metadata(
    line: &[u8],
    expected_pane_id: PaneId,
) -> Result<PaneCaptureMetadata, TopologyError> {
    let fields = split_inventory(line, 14)?;
    if fields[0] != b"R" {
        return Err(TopologyError::MalformedInventory);
    }
    let pane_id = parse_pane_id(fields[1])?;
    if pane_id != expected_pane_id {
        return Err(TopologyError::ContradictoryInventory(
            "pane capture metadata names a different pane",
        ));
    }
    Ok(PaneCaptureMetadata {
        pane_id,
        left: number(fields[2], "pane left")?,
        top: number(fields[3], "pane top")?,
        width: number(fields[4], "pane width")?,
        height: number(fields[5], "pane height")?,
        dead: boolean(fields[6], "pane dead")?,
        cursor_x: number(fields[7], "cursor x")?,
        cursor_y: number(fields[8], "cursor y")?,
        cursor_visible: boolean(fields[9], "cursor visible")?,
        cursor_shape: parse_cursor_shape(fields[10])?,
        alternate_on: boolean(fields[11], "alternate screen")?,
        pane_in_mode: number(fields[12], "pane mode")?,
        history_size: number(fields[13], "history size")?,
    })
}

fn parse_session_id(bytes: &[u8]) -> Result<SessionId, TopologyError> {
    parse_id(bytes, b'$', "session").map(SessionId)
}

fn parse_window_id(bytes: &[u8]) -> Result<WindowId, TopologyError> {
    parse_id(bytes, b'@', "window").map(WindowId)
}

fn parse_pane_id(bytes: &[u8]) -> Result<PaneId, TopologyError> {
    parse_id(bytes, b'%', "pane").map(PaneId)
}

fn parse_id(bytes: &[u8], prefix: u8, kind: &'static str) -> Result<u64, TopologyError> {
    let digits = bytes
        .strip_prefix(&[prefix])
        .ok_or(TopologyError::InvalidId { kind })?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(TopologyError::InvalidId { kind });
    }
    text(digits, "id")?
        .parse()
        .map_err(|_| TopologyError::InvalidId { kind })
}

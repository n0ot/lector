use std::sync::{Mutex, MutexGuard};

static REAL_TMUX_TEST_LOCK: Mutex<()> = Mutex::new(());

fn serialize_real_tmux_test() -> MutexGuard<'static, ()> {
    REAL_TMUX_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[path = "tmux_bells.rs"]
mod tmux_bells;
#[path = "tmux_completion.rs"]
mod tmux_completion;
#[path = "tmux_connections.rs"]
mod tmux_connections;
#[path = "tmux_control_fuzz.rs"]
mod tmux_control_fuzz;
#[path = "tmux_control_parser.rs"]
mod tmux_control_parser;
#[path = "tmux_gateway.rs"]
mod tmux_gateway;
#[path = "tmux_input.rs"]
mod tmux_input;
#[path = "tmux_interaction.rs"]
mod tmux_interaction;
#[path = "tmux_lifecycle.rs"]
mod tmux_lifecycle;
#[path = "tmux_nested.rs"]
mod tmux_nested;
#[path = "tmux_panes.rs"]
mod tmux_panes;
#[path = "tmux_prefix.rs"]
mod tmux_prefix;
#[path = "tmux_recovery.rs"]
mod tmux_recovery;
#[path = "tmux_topology.rs"]
mod tmux_topology;

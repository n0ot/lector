use lector::harness::Harness;

#[test]
fn deletion_survives_a_completed_unchanged_frame() {
    let mut harness = Harness::new(4, 20).unwrap();
    harness
        .run_script(
            r"
Scenario: an unchanged receipt precedes the edit
Given pty-stdout: \x1b[2;1H> abc
And settled:
And clear-speech:
When stdin: \x7f
And pty-stdout: \x1b[?2026h\x1b[0m\x1b[2;6H\x1b[?25h\x1b[?2026l
And settled:
And pty-stdout: \x1b[?2026h\x1b[2;5H\x1b[K\x1b[?2026l
And settled:
Then expect-speak: c
And expect-no-speak:

Scenario: unchanged receipts alone never announce deletion
Given pty-stdout: > abc
And settled:
And clear-speech:
When stdin: \x7f
And pty-stdout: \x1b[?2026h\x1b[0m\x1b[?25h\x1b[?2026l
And settled:
Then expect-no-speak:

Scenario: an incompatible replacement disproves the pending edit
Given pty-stdout: > abc
And settled:
And clear-speech:
When stdin: \x7f
And pty-stdout: \r# xyz
And settled:
And clear-speech:
And pty-stdout: \x1b[1;5H\x1b[K
And settled:
Then expect-no-speak:
",
        )
        .unwrap();
}

#[test]
fn harness_script_basic() {
    let script = include_str!("scripts/basic.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_lua_repl() {
    let script = include_str!("scripts/lua_repl.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_table_headers() {
    let script = include_str!("scripts/table_headers.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_table_real_world() {
    let script = include_str!("scripts/table_real_world.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_focus_events() {
    let script = include_str!("scripts/focus_events.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_command_modes() {
    let script = include_str!("scripts/command_modes.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_semantic_history() {
    let script = include_str!("scripts/semantic_history.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_auto_read() {
    let script = include_str!("scripts/auto_read.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_visual_focus() {
    let script = include_str!("scripts/visual_focus.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_terminal_effects_and_modes() {
    let script = include_str!("scripts/terminal_effects_and_modes.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_terminal_resize() {
    let script = include_str!("scripts/terminal_resize.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn harness_script_terminal_differential_corpus() {
    let script = include_str!("scripts/terminal_differential_corpus.txt");
    let mut harness = Harness::new(24, 80).expect("create harness");
    harness.run_script(script).expect("run script");
}

#[test]
fn review_link_copy_uses_frozen_osc8_and_plain_text_destinations() {
    let mut harness = Harness::new(4, 40).unwrap();
    harness
        .run_script(
            r"
Scenario: an OSC 8 label copies its destination through Review
When pty-stdout: \x1b]8;;https://example.test/path\x07label\x1b]8;;\x07\x1b[H
And stdin: \x1brgyq
And stdin: \x1b[18~
Then expect-pty-stdin: https://example.test/path

Scenario: a plain text URL copies without its surrounding punctuation
When pty-stdout: (https://example.test/path).\x1b[H
And stdin: \x1brlgyq
And stdin: \x1b[18~
Then expect-pty-stdin: https://example.test/path
",
        )
        .unwrap();
}

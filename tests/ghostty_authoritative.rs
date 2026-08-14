use lector::harness::Harness;

#[test]
fn ghostty_only_workflow_preserves_raw_presentation_and_accessibility_harnesses() {
    let chunks: &[&[u8]] = &[
        b"\x1b]133;A\x07dev@host$ \x1b]133;B\x07cargo test",
        b"\x1b]133;C\x07\r\n\x1b[31merror\x1b[0m: expected `;`\r\n",
        b"\x1b[?1049h\x1b[Heditor\x1b[?1049l",
        b"\x1b]133;D;1\x07",
    ];
    let expected = chunks.concat();
    let mut harness = Harness::new(8, 48).expect("create Ghostty workflow harness");
    for chunk in chunks {
        harness
            .handle_pty_output(chunk)
            .expect("process Ghostty-owned PTY output");
    }
    assert_eq!(harness.terminal_output(), expected);

    for script in [
        include_str!("scripts/semantic_history.txt"),
        include_str!("scripts/auto_read.txt"),
        include_str!("scripts/terminal_resize.txt"),
        include_str!("scripts/terminal_effects_and_modes.txt"),
    ] {
        let mut harness = Harness::new(24, 80).expect("create accessibility harness");
        harness
            .run_script(script)
            .expect("run Ghostty-only workflow");
    }
}

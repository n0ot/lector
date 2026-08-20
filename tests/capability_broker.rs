use lector::{
    harness::Harness,
    terminal::{
        ClipboardContent, ClipboardLocation, GhosttyEngine, TerminalEvent, TerminalGeometry,
    },
    terminal_protocol::{
        ApplicationReplyBroker, CapabilityOverrides, ColorScheme, EffectDisposition,
        PhysicalTerminalProfile, ProbePolicy, ProbeReport, ShutdownFenceBroker, StartupProbeBroker,
        TerminalEffectPolicy, TerminfoCapabilities, VirtualTerminalProfile,
    },
};

fn geometry() -> TerminalGeometry {
    TerminalGeometry::new(24, 80, 9, 18)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn physical_profile_uses_conservative_terminfo_probe_and_override_precedence() {
    let mut profile = PhysicalTerminalProfile::conservative(geometry());
    assert_eq!(profile.color_count, 8);
    assert!(!profile.true_color);
    assert!(!profile.synchronized_output);
    assert!(!profile.kitty_keyboard);
    assert!(!profile.kitty_graphics);
    assert!(!profile.clipboard_read);

    profile.apply_terminfo(&TerminfoCapabilities {
        color_count: Some(256),
        true_color: true,
        hyperlinks: true,
        ..TerminfoCapabilities::default()
    });
    profile.apply_probe(&ProbeReport {
        geometry: Some(TerminalGeometry::new(30, 100, 10, 20)),
        color_scheme: Some(ColorScheme::Dark),
        synchronized_output: Some(true),
        kitty_keyboard: Some(true),
        kitty_graphics: Some(false),
        focus_reporting: Some(true),
        ..ProbeReport::default()
    });
    profile.apply_overrides(&CapabilityOverrides {
        synchronized_output: Some(false),
        kitty_graphics: Some(true),
        clipboard_read: Some(false),
        ..CapabilityOverrides::default()
    });

    assert_eq!(profile.geometry, TerminalGeometry::new(30, 100, 10, 20));
    assert_eq!(profile.color_count, 256);
    assert!(profile.true_color);
    assert!(profile.hyperlinks);
    assert_eq!(profile.color_scheme, Some(ColorScheme::Dark));
    assert!(!profile.synchronized_output);
    assert!(profile.kitty_keyboard);
    assert!(profile.kitty_graphics);
    assert!(profile.focus_reporting);
    assert!(!profile.clipboard_read);
}

#[test]
fn terminfo_and_explicit_overrides_have_bounded_stable_parsers() {
    let terminfo = TerminfoCapabilities::from_infocmp(
        "lector|Lector virtual terminal,\n\tcolors#256,\n\tRGB,\n\tOSC8,\n\tSync,\n",
    );
    assert_eq!(terminfo.color_count, Some(256));
    assert!(terminfo.true_color);
    assert!(terminfo.hyperlinks);
    assert!(terminfo.synchronized_output);
    assert!(!terminfo.kitty_graphics);

    let overrides = CapabilityOverrides::from_pairs([
        ("LECTOR_OUTER_COLORS", "16"),
        ("LECTOR_OUTER_SYNC", "false"),
        ("LECTOR_OUTER_KITTY_GRAPHICS", "true"),
        ("LECTOR_OUTER_CLIPBOARD_READ", "false"),
    ])
    .expect("parse explicit overrides");
    assert_eq!(overrides.color_count, Some(16));
    assert_eq!(overrides.synchronized_output, Some(false));
    assert_eq!(overrides.kitty_graphics, Some(true));
    assert_eq!(overrides.clipboard_read, Some(false));
    assert!(CapabilityOverrides::from_pairs([("LECTOR_OUTER_SYNC", "perhaps")]).is_err());
}

#[test]
fn startup_probe_replies_are_fragment_safe_out_of_order_and_never_become_input() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 10);
    let queries = broker.startup_queries();
    assert_eq!(broker.outstanding_primary_device_attributes_replies(), 1);
    assert!(
        queries.ends_with(b"\x1b[c"),
        "DA1 must fence every earlier probe"
    );
    for expected in [
        b"\x1b[c".as_slice(),
        b"\x1b[>c",
        b"\x1b[=c",
        b"\x1b[14t",
        b"\x1b[16t",
        b"\x1b[18t",
        b"\x1b[?1004$p",
        b"\x1b[?2026$p",
        b"\x1b[?u",
        b"\x1b]10;?\x1b\\",
        b"\x1b]11;?\x1b\\",
    ] {
        assert!(contains(&queries, expected), "missing probe {expected:?}");
    }
    assert!(!contains(&queries, b"\x1b]52;"));

    // Deliberately place replies in a different order from their requests and
    // split every byte. Ordinary user input around them must survive exactly.
    let replies = b"a\x1b[?2026;1$y\x1b[?0u\x1b[8;30;100t\x1b[6;20;10t\x1b[4;600;1000t\x1b[>41;301;0c\x1b[?1004;2$y\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?64;22cz";
    let mut user_input = Vec::new();
    for byte in replies {
        user_input.extend(broker.ingest(&[*byte], 11));
    }
    assert_eq!(user_input, b"az");
    assert_eq!(broker.outstanding_primary_device_attributes_replies(), 0);
    assert!(broker.is_finished());
    assert_eq!(broker.malformed_replies(), 0);
    assert!(broker.profile().synchronized_output);
    assert!(!broker.profile().focus_reporting);
    assert!(broker.profile().kitty_keyboard);
    assert_eq!(
        broker.profile().geometry,
        TerminalGeometry::new(30, 100, 10, 20)
    );
    assert_eq!(broker.profile().color_scheme, Some(ColorScheme::Dark));
}

#[test]
fn shutdown_fence_is_fragment_safe_and_matches_only_the_fresh_primary_da_reply() {
    let mut broker = ShutdownFenceBroker::new(1);
    let replies = b"text\x1b[I\x1b[>41;301;0c\x1b[?6c\x1b[O\x1b[?62;22;52cafter";
    let mut matched_at = None;
    for (index, byte) in replies.iter().copied().enumerate() {
        if broker.ingest_byte(byte) {
            matched_at = Some(index);
            break;
        }
    }

    assert_eq!(broker.observed_replies(), 2);
    assert!(broker.is_matched());
    assert_eq!(matched_at, Some(replies.len() - b"after".len() - 1));

    let mut c1 = ShutdownFenceBroker::new(0);
    assert!(!c1.ingest_byte(0x9b));
    assert!(!c1.ingest_byte(b'?'));
    assert!(!c1.ingest_byte(b'6'));
    assert!(c1.ingest_byte(b'c'));
}

#[test]
fn clipboard_probe_is_explicitly_opted_in_and_consumed_locally() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(
        profile,
        ProbePolicy {
            clipboard_read: true,
        },
        0,
    );
    assert!(contains(&broker.startup_queries(), b"\x1b]52;c;?\x1b\\"));
    assert!(broker.ingest(b"\x1b]52;c;dGVzdA==\x1b\\", 1).is_empty());
    assert!(broker.profile().clipboard_read);
}

#[test]
fn startup_probe_bounds_malicious_replies_and_releases_unrelated_input_on_timeout() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 100);
    let _ = broker.startup_queries();
    assert_eq!(broker.next_deadline_ms(), Some(151));

    let mut malicious = b"\x1b]10;rgb:".to_vec();
    malicious.extend(std::iter::repeat_n(b'f', 8_192));
    assert!(broker.ingest(&malicious, 101).is_empty());
    assert!(broker.buffered_reply_bytes() <= 4_096);
    assert_eq!(broker.malformed_replies(), 1);
    assert_eq!(broker.ingest(b"\x1b\\key", 102), b"key");

    assert!(broker.ingest(b"\x1b[?2026;", 103).is_empty());
    assert_eq!(broker.next_deadline_ms(), Some(154));
    assert_eq!(broker.finish_if_timed_out(154), b"\x1b[?2026;");
    assert!(broker.is_finished());
    assert_eq!(broker.next_deadline_ms(), None);
}

#[test]
fn readable_probe_batch_wins_the_timeout_race_and_is_still_consumed() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();
    assert_eq!(broker.outstanding_primary_device_attributes_replies(), 1);

    let late_reply = b"\x1b[?62;22;52c";
    for (index, chunk) in late_reply.chunks(2).enumerate() {
        assert!(broker.ingest(chunk, 100 + index as u128).is_empty());
    }
    assert_eq!(broker.outstanding_primary_device_attributes_replies(), 0);
    assert!(broker.is_finished());
}

#[test]
fn timed_out_probe_broker_still_owns_delayed_terminal_replies() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();

    assert!(broker.finish_if_timed_out(100).is_empty());
    assert!(broker.is_finished());

    let delayed = b"c\x1b[>41;301;0c\x1b[?0u\x1b[8;24;80t\x1b[?64;22cz";
    let mut application_input = Vec::new();
    for byte in delayed {
        application_input.extend(broker.ingest(&[*byte], 101));
    }
    assert_eq!(application_input, b"cz");
    assert!(broker.profile().kitty_keyboard);

    assert!(broker.ingest(b"\x1b", 102).is_empty());
    assert_eq!(broker.next_deadline_ms(), Some(153));
    assert_eq!(broker.finish_if_timed_out(153), b"\x1b");
}

#[test]
fn virtual_profile_drives_every_ghostty_generated_application_reply() {
    let profile = VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark);
    let mut engine = GhosttyEngine::new_with_profile(24, 80, profile)
        .expect("create engine with Lector virtual profile");
    let update = engine
        .advance(
            b"\x05\x1b[>q\x1b[14t\x1b[16t\x1b[18t\x1b[?996n\x1b[c\x1b[>c\x1b[=c\x1b[?1004$p\x1b[?2026$p\x1b[?u\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]52;c;?\x1b\\",
        )
        .expect("advance virtual queries");
    let replies = update.pty_replies;

    for expected in [
        b"lector".as_slice(),
        b"\x1bP>|Lector 0.4.0\x1b\\",
        b"\x1b[4;432;720t",
        b"\x1b[6;18;9t",
        b"\x1b[8;24;80t",
        b"\x1b[?997;1n",
        b"\x1b[?64;22;28c",
        b"\x1b[>41;301;0c",
        b"\x1bP!|00000000\x1b\\",
        b"\x1b[?1004;2$y",
        b"\x1b[?2026;2$y",
        b"\x1b[?0u",
        b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\",
        b"\x1b]11;rgb:0000/0000/0000\x1b\\",
        b"\x1b]52;c;\x1b\\",
    ] {
        assert!(
            contains(&replies, expected),
            "missing reply {expected:?} in {replies:?}"
        );
    }
    assert!(!contains(&replies, b"Ghostty"));
    assert!(!contains(&replies, b"xterm"));
}

#[test]
fn application_reply_broker_never_crosses_pane_ownership() {
    let mut broker = ApplicationReplyBroker::<u64>::default();
    broker.queue(7, b"pane-seven-a");
    broker.queue(9, b"pane-nine");
    broker.queue(7, b"-b");

    assert_eq!(broker.take(9), b"pane-nine");
    assert_eq!(broker.take(7), b"pane-seven-a-b");
    assert!(broker.take(7).is_empty());
    assert!(broker.take(9).is_empty());
}

#[test]
fn overlapping_pane_queries_keep_different_geometry_replies_scoped() {
    let mut seven = GhosttyEngine::new_with_profile(
        10,
        20,
        VirtualTerminalProfile::lector(TerminalGeometry::new(10, 20, 8, 16), ColorScheme::Dark),
    )
    .expect("create pane seven");
    let mut nine = GhosttyEngine::new_with_profile(
        30,
        90,
        VirtualTerminalProfile::lector(TerminalGeometry::new(30, 90, 10, 20), ColorScheme::Light),
    )
    .expect("create pane nine");

    let mut broker = ApplicationReplyBroker::<u64>::default();
    for byte in b"\x1b[18t\x1b[?996n" {
        let reply = seven
            .advance(&[*byte])
            .expect("query pane seven")
            .pty_replies;
        broker.queue(7, &reply);
    }
    for byte in b"\x1b[?996n\x1b[18t" {
        let reply = nine.advance(&[*byte]).expect("query pane nine").pty_replies;
        broker.queue(9, &reply);
    }

    assert_eq!(broker.take(7), b"\x1b[8;10;20t\x1b[?997;1n");
    assert_eq!(broker.take(9), b"\x1b[?997;2n\x1b[8;30;90t");
}

#[test]
fn effect_policy_is_explicit_for_every_modeled_terminal_effect() {
    let policy = TerminalEffectPolicy::secure_default();
    let events = [
        TerminalEvent::TitleChanged("title".into()),
        TerminalEvent::WorkingDirectoryChanged("file:///tmp".into()),
        TerminalEvent::ClipboardWrite {
            location: ClipboardLocation::Standard,
            contents: vec![ClipboardContent {
                mime: "text/plain".into(),
                data: b"copy".to_vec(),
            }],
        },
        TerminalEvent::DesktopNotification {
            title: "build".into(),
            body: "done".into(),
        },
        TerminalEvent::ProgressReport {
            state: lector::terminal::ProgressState::Set,
            progress: Some(50),
        },
        TerminalEvent::UnknownSequence {
            content: b"unknown".to_vec(),
            truncated: false,
        },
    ];
    assert_eq!(policy.disposition(&events[0]), EffectDisposition::Model);
    assert_eq!(policy.disposition(&events[1]), EffectDisposition::Model);
    assert_eq!(
        policy.disposition(&events[2]),
        EffectDisposition::LocalClipboard
    );
    assert_eq!(policy.disposition(&events[3]), EffectDisposition::Drop);
    assert_eq!(policy.disposition(&events[4]), EffectDisposition::Model);
    assert_eq!(policy.disposition(&events[5]), EffectDisposition::Drop);
}

#[test]
fn real_application_harness_flushes_virtual_replies_at_the_pty_chunk_boundary() {
    let mut harness = Harness::new(24, 80).expect("create compositor harness");
    harness
        .handle_pty_output(b"before\x1b[18t\x1b[c\x1b[?uafter")
        .expect("handle application queries");
    harness
        .flush_application_replies()
        .expect("flush reply broker at PTY chunk boundary");

    let application_input = harness.application_input();
    assert!(contains(application_input, b"\x1b[8;24;80t"));
    assert!(contains(application_input, b"\x1b[?64;22;28c"));
    assert!(contains(application_input, b"\x1b[?0u"));
    assert!(!contains(harness.terminal_output(), b"\x1b[18t"));
    assert!(!contains(harness.terminal_output(), b"\x1b[c"));
}

#[test]
fn compositor_path_virtualizes_queries_and_sensitive_effects() {
    let mut harness = Harness::new(4, 20).expect("create compositor harness");
    harness
        .handle_pty_output(
            b"before\x1b[18t\x1b[c\x1b[?u\x1b]52;c;Y29weQ==\x1b\\\x1b]777;notify;title;body\x1b\\\x1b_unknown\x1b\\after",
        )
        .expect("handle virtualized application output");
    harness.tick(0).expect("flush application replies");

    assert!(!contains(harness.terminal_output(), b"Y29weQ=="));
    assert!(!contains(harness.terminal_output(), b"notify"));
    assert!(!contains(harness.terminal_output(), b"unknown"));
    assert_eq!(harness.clipboard_text(), Some("copy"));
    assert!(contains(harness.application_input(), b"\x1b[8;4;20t"));
    assert!(contains(harness.application_input(), b"\x1b[?64;22;28c"));
    assert!(contains(harness.application_input(), b"\x1b[?0u"));
}

#[test]
fn live_harness_consumes_outer_probe_replies_before_input_dispatch() {
    let mut harness = Harness::new(24, 80).expect("create probe harness");
    harness
        .start_capability_probes()
        .expect("write startup probe transaction");
    assert!(contains(harness.terminal_output(), b"\x1b[?2026$p"));

    for byte in b"\x1b[?2026;1$y\x1b[?0ux" {
        harness
            .handle_terminal_input(&[*byte])
            .expect("consume fragmented physical reply");
    }
    assert_eq!(harness.application_input(), b"x");
    assert!(harness.physical_profile().synchronized_output);
    assert!(harness.physical_profile().kitty_keyboard);
}

#[test]
fn delayed_outer_probe_replies_cannot_pollute_a_new_tmux_control_connection() {
    let mut harness = Harness::new(24, 80).expect("create probe harness");
    harness
        .start_capability_probes()
        .expect("write startup probe transaction");

    // Model a large initial Ghostty render: the terminal has queued its
    // replies, but Lector cannot read them until the original 50 ms fallback
    // deadline has passed. Meanwhile the child starts tmux control mode.
    harness.advance_clock(100);
    harness
        .handle_pty_output(b"\x1bP1000p%begin 1 1 0\n%end 1 1 0\n")
        .expect("start tmux gateway");
    harness
        .handle_terminal_input(
            b"\x1b[>41;301;0c\x1b[?0u\x1b[8;24;80t\x1b[6;18;9t\x1b[4;432;720t\x1b[?1004;2$y\x1b[?2026;1$y\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?64;22c",
        )
        .expect("consume delayed Ghostty replies");
    harness.tick(0).expect("drain tmux bootstrap commands");

    assert_eq!(
        harness.application_input(),
        [
            lector::app::TMUX_FLOW_CONTROL_COMMAND,
            lector::app::TMUX_FLOW_CONTROL_VERIFY_COMMAND,
            b"refresh-client -C 80x24\n",
            lector::tmux_model::INVENTORY_COMMAND.as_bytes(),
        ]
        .concat(),
        "only Lector's tmux commands may reach tmux -CC",
    );
}

#[test]
fn delayed_outer_probe_replies_after_timeout_cannot_become_application_keys() {
    let mut harness = Harness::new(24, 80).expect("create probe harness");
    harness
        .start_capability_probes()
        .expect("write startup probe transaction");

    harness.advance_clock(100);
    harness
        .tick(0)
        .expect("expire startup probe readiness wait");
    harness
        .handle_terminal_input(
            b"\x1b[>41;301;0c\x1b[?0u\x1b[8;24;80t\x1b[6;18;9t\x1b[4;432;720t\x1b[?1004;2$y\x1b[?2026;1$y\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?64;22ccodex",
        )
        .expect("consume delayed physical replies after readiness timeout");

    assert_eq!(harness.application_input(), b"codex");
}

use lector::{
    harness::Harness,
    terminal::{
        ClipboardContent, ClipboardLocation, GhosttyEngine, TerminalEngine, TerminalEvent,
        TerminalGeometry,
    },
    terminal_protocol::{
        ApplicationReplyBroker, CapabilityOverrides, ColorScheme, DefaultColor, EffectDisposition,
        PhysicalTerminalProfile, ProbePolicy, ProbeReport, ShutdownFenceBroker, StartupProbeBroker,
        TerminalEffectPolicy, TerminfoCapabilities, VirtualTerminalColors, VirtualTerminalProfile,
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
    assert_eq!(profile.default_foreground, None);
    assert_eq!(profile.default_background, None);
    assert!(!profile.synchronized_output);
    assert!(profile.kitty_keyboard);
    assert!(profile.kitty_graphics);
    assert!(profile.focus_reporting);
    assert!(!profile.clipboard_read);
}

#[test]
fn terminfo_and_explicit_overrides_have_bounded_stable_parsers() {
    let terminfo = TerminfoCapabilities::from_infocmp(
        "lector|Lector virtual terminal,\n\tcolors#256,\n\tRGB,\n\tOSC8,\n\tSync=\\E[?2026%?%p1%{1}%-%tl%eh%;,\n",
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
        b"\x1b[?996n",
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
    assert_eq!(
        broker.profile().default_foreground,
        Some(DefaultColor::WHITE)
    );
    assert_eq!(
        broker.profile().default_background,
        Some(DefaultColor::BLACK)
    );
}

#[test]
fn startup_da1_fence_relinquishes_the_input_stream_in_the_same_read() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();
    let input = b"\x1b[?64;22c\x1bi\x1bP\x1b]\x1b[\x1bO\x1b_";

    assert_eq!(
        broker.ingest(input, 1),
        b"\x1bi\x1bP\x1b]\x1b[\x1bO\x1b_",
        "bytes following the ordered fence are ordinary input, even when they resemble reply introducers",
    );
    assert!(broker.is_finished());
    assert_eq!(broker.next_deadline_ms(), None);
}

#[test]
fn startup_probe_buffers_only_prefixes_of_requested_reply_families() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();

    for input in [
        b"\x1bi".as_slice(),
        b"\x1b_".as_slice(),
        b"\x1bPx".as_slice(),
        b"\x1b]x".as_slice(),
    ] {
        assert_eq!(broker.ingest(input, 1), input, "input={input:?}");
        assert_eq!(broker.buffered_reply_bytes(), 0, "input={input:?}");
    }

    assert!(broker.ingest(b"\x1bP!|partial", 2).is_empty());
    assert_eq!(broker.finish_if_timed_out(53), b"\x1bP!|partial");
    assert!(broker.is_finished());
}

#[test]
fn startup_probe_retains_and_normalizes_exact_outer_default_colors() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();

    assert!(
        broker
            .ingest(
                b"\x1b]10;rgb:a/bb/ccc\x1b\\\x1b]11;rgb:1/22/333\x07\x1b[?64;22c",
                1,
            )
            .is_empty()
    );
    assert_eq!(
        broker.profile().default_foreground,
        Some(DefaultColor::new(0xaaaa, 0xbbbb, 0xcccc))
    );
    assert_eq!(
        broker.profile().default_background,
        Some(DefaultColor::new(0x1111, 0x2222, 0x3333))
    );
    assert_eq!(broker.profile().color_scheme, Some(ColorScheme::Dark));
    assert_eq!(
        broker.profile().virtual_terminal_colors(),
        Some(VirtualTerminalColors::new(
            ColorScheme::Dark,
            DefaultColor::new(0xaaaa, 0xbbbb, 0xcccc),
            DefaultColor::new(0x1111, 0x2222, 0x3333),
        ))
    );
}

#[test]
fn native_color_scheme_wins_and_exact_defaults_still_wait_for_each_other() {
    let mut harness = Harness::new(24, 80).expect("create native-theme harness");
    harness
        .start_capability_probes()
        .expect("start outer probes");
    harness
        .handle_pty_output(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
        .expect("handle eager child theme queries");

    // The semantic reply arrives first, but exact OSC defaults are still in
    // flight. Releasing now would make the child cache black/white fallbacks.
    harness
        .handle_terminal_input(b"\x1b[?997;2n")
        .expect("consume native light report");
    harness
        .flush_application_replies()
        .expect("keep exact defaults pending");
    assert!(harness.application_input().is_empty());

    // The later OSC pair looks dark by luminance. The native semantic result
    // remains authoritative while the exact values pass through unchanged.
    harness
        .handle_terminal_input(
            b"\x1b]10;rgb:eeee/dddd/cccc\x1b\\\x1b]11;rgb:1111/2222/3333\x1b\\\x1b[?64;22c",
        )
        .expect("consume exact defaults and fence");
    harness
        .flush_application_replies()
        .expect("release exact child profile");
    assert_eq!(
        harness.physical_profile().color_scheme,
        Some(ColorScheme::Light)
    );
    assert!(contains(
        harness.application_input(),
        b"\x1b]10;rgb:eeee/dddd/cccc\x1b\\"
    ));
    assert!(contains(
        harness.application_input(),
        b"\x1b]11;rgb:1111/2222/3333\x1b\\"
    ));
    assert!(contains(harness.application_input(), b"\x1b[?997;2n"));
}

#[test]
fn exact_defaults_do_not_beat_a_later_contradictory_native_scheme() {
    let mut harness = Harness::new(24, 80).expect("create reordered-theme harness");
    harness
        .start_capability_probes()
        .expect("start outer probes");
    harness
        .handle_pty_output(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
        .expect("handle eager child theme queries");
    harness
        .handle_terminal_input(b"\x1b]10;rgb:eeee/dddd/cccc\x1b\\\x1b]11;rgb:1111/2222/3333\x1b\\")
        .expect("consume exact dark-looking defaults first");
    harness
        .flush_application_replies()
        .expect("wait for native scheme");
    assert!(harness.application_input().is_empty());

    harness
        .handle_terminal_input(b"\x1b[?997;2n\x1b[?64;22c")
        .expect("consume contradictory native light result and fence");
    harness
        .flush_application_replies()
        .expect("release reconciled child profile");
    assert!(contains(harness.application_input(), b"\x1b[?997;2n"));
}

#[test]
fn one_outer_default_color_still_provides_a_bounded_scheme_fallback() {
    let profile = PhysicalTerminalProfile::conservative(geometry());
    let mut broker = StartupProbeBroker::new(profile, ProbePolicy::safe(), 0);
    let _ = broker.startup_queries();
    assert!(
        broker
            .ingest(b"\x1b]11;rgb:eeee/eeee/eeee\x1b\\\x1b[?64;22c", 1)
            .is_empty()
    );
    assert_eq!(broker.profile().color_scheme, Some(ColorScheme::Light));
    assert_eq!(broker.profile().default_foreground, None);
    assert_eq!(
        broker.profile().default_background,
        Some(DefaultColor::new(0xeeee, 0xeeee, 0xeeee))
    );
    assert_eq!(
        broker.profile().virtual_terminal_colors(),
        Some(VirtualTerminalColors::new(
            ColorScheme::Light,
            DefaultColor::BLACK,
            DefaultColor::new(0xeeee, 0xeeee, 0xeeee),
        ))
    );
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

    assert_eq!(broker.ingest(b"\x1b", 102), b"\x1b");
    assert_eq!(broker.next_deadline_ms(), None);
    assert!(broker.finish_if_timed_out(153).is_empty());
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
        b"\x1bP>|Lector 0.4.1\x1b\\",
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
fn running_virtual_terminal_updates_exact_defaults_and_color_scheme_without_reset() {
    let dark = VirtualTerminalColors::new(
        ColorScheme::Dark,
        DefaultColor::new(0xeeee, 0xdddd, 0xcccc),
        DefaultColor::new(0x1111, 0x2222, 0x3333),
    );
    let mut engine = GhosttyEngine::new_with_profile(
        24,
        80,
        VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark).with_colors(dark),
    )
    .expect("create dark virtual terminal");
    let dark_replies = engine
        .advance(b"before\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
        .expect("query dark virtual terminal")
        .pty_replies;
    assert!(contains(&dark_replies, b"\x1b]10;rgb:eeee/dddd/cccc\x1b\\"));
    assert!(contains(&dark_replies, b"\x1b]11;rgb:1111/2222/3333\x1b\\"));
    assert!(contains(&dark_replies, b"\x1b[?997;1n"));

    let light = VirtualTerminalColors::new(
        ColorScheme::Light,
        DefaultColor::new(0x1234, 0x2345, 0x3456),
        DefaultColor::new(0xdabc, 0xebcd, 0xfcde),
    );
    engine.set_virtual_terminal_colors(light);
    let light_replies = engine
        .advance(b"after\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
        .expect("query updated light virtual terminal")
        .pty_replies;
    assert!(contains(
        &light_replies,
        b"\x1b]10;rgb:1234/2345/3456\x1b\\"
    ));
    assert!(contains(
        &light_replies,
        b"\x1b]11;rgb:dabc/ebcd/fcde\x1b\\"
    ));
    assert!(contains(&light_replies, b"\x1b[?997;2n"));
    assert!(engine.snapshot().contents().contains("before"));
    assert!(engine.snapshot().contents().contains("after"));
}

#[test]
fn eager_child_color_queries_wait_for_and_match_light_and_dark_outer_profiles() {
    for (foreground, background, scheme_report) in [
        (
            "eeee/dddd/cccc",
            "1111/2222/3333",
            b"\x1b[?997;1n".as_slice(),
        ),
        (
            "1234/2345/3456",
            "dabc/ebcd/fcde",
            b"\x1b[?997;2n".as_slice(),
        ),
    ] {
        let mut harness = Harness::new(24, 80).expect("create eager-child harness");
        harness
            .start_capability_probes()
            .expect("start outer probes");
        harness
            .handle_pty_output(b"\x1b[18t\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
            .expect("handle child queries before outer replies");
        harness
            .flush_application_replies()
            .expect("defer child replies");
        assert_eq!(harness.application_input(), b"\x1b[8;24;80t");

        let outer =
            format!("\x1b]10;rgb:{foreground}\x1b\\\x1b]11;rgb:{background}\x1b\\\x1b[?64;22c");
        harness
            .handle_terminal_input(outer.as_bytes())
            .expect("consume outer color profile");
        harness
            .flush_application_replies()
            .expect("release reconciled child replies");

        let replies = harness.application_input();
        assert!(contains(
            replies,
            format!("\x1b]10;rgb:{foreground}\x1b\\").as_bytes()
        ));
        assert!(contains(
            replies,
            format!("\x1b]11;rgb:{background}\x1b\\").as_bytes()
        ));
        assert!(contains(replies, scheme_report));
    }
}

#[test]
fn outer_theme_negotiation_is_presentation_transparent() {
    let mut harness = Harness::new(24, 80).expect("create transparent-theme harness");
    harness
        .start_capability_probes()
        .expect("start outer probes");
    harness
        .handle_pty_output(b"visible content")
        .expect("render ordinary child content");
    let grid_before_queries = harness.active_view_contents();
    assert!(grid_before_queries.contains("visible content"));
    let presentation_before_queries = harness.terminal_output().to_vec();

    harness
        .handle_pty_output(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n")
        .expect("consume child theme queries");
    assert_eq!(
        harness.terminal_output(),
        presentation_before_queries,
        "child theme queries must not leak to or redraw the physical terminal"
    );
    assert!(harness.application_input().is_empty());

    harness
        .handle_terminal_input(
            b"\x1b]10;rgb:1111/2222/3333\x1b\\\x1b]11;rgb:dddd/eeee/ffff\x1b\\\x1b[?997;2n\x1b[?64;22c",
        )
        .expect("consume exact outer defaults and native theme reply");
    harness
        .flush_application_replies()
        .expect("release reconciled child theme replies");

    assert_eq!(
        harness.terminal_output(),
        presentation_before_queries,
        "outer probe replies and child query replies must remain off the presentation path"
    );
    assert_eq!(
        harness.active_view_contents(),
        grid_before_queries,
        "theme negotiation must not mutate the application grid"
    );
    assert!(contains(
        harness.application_input(),
        b"\x1b]10;rgb:1111/2222/3333\x1b\\"
    ));
    assert!(contains(
        harness.application_input(),
        b"\x1b]11;rgb:dddd/eeee/ffff\x1b\\"
    ));
    assert!(contains(harness.application_input(), b"\x1b[?997;2n"));
}

#[test]
fn continuous_unrelated_input_cannot_extend_the_child_color_wait() {
    let mut harness = Harness::new(24, 80).expect("create bounded-theme harness");
    harness
        .start_capability_probes()
        .expect("start outer probes");
    harness
        .handle_pty_output(b"\x1b]10;?\x1b\\\x1b[?996n")
        .expect("handle child theme queries");

    for _ in 0..5 {
        harness.advance_clock(10);
        harness
            .handle_terminal_input(b"x")
            .expect("forward unrelated input while probe remains active");
        harness
            .flush_application_replies()
            .expect("apply bounded color hold");
        assert!(!contains(harness.application_input(), b"\x1b]10;rgb:"));
    }

    harness.advance_clock(2);
    harness
        .handle_terminal_input(b"x")
        .expect("forward input beyond absolute color deadline");
    harness
        .flush_application_replies()
        .expect("release fallback at absolute deadline");
    assert!(contains(
        harness.application_input(),
        b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"
    ));
    assert!(contains(harness.application_input(), b"\x1b[?997;1n"));
}

#[test]
fn unbrokered_pixel_mouse_theme_notification_and_resize_modes_stay_unsupported() {
    let profile = VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark);
    let mut engine = GhosttyEngine::new_with_profile(24, 80, profile)
        .expect("create engine with Lector virtual profile");

    let initial = engine
        .advance(b"\x1b[?1015$p\x1b[?1016$p\x1b[?2031$p\x1b[?2048$p\x1b[?1006$p\x1b[?2026$p")
        .expect("query initial private modes")
        .pty_replies;
    assert_eq!(
        initial,
        b"\x1b[?1015;0$y\x1b[?1016;0$y\x1b[?2031;0$y\x1b[?2048;0$y\x1b[?1006;2$y\x1b[?2026;2$y"
    );

    let after_enable = engine
        .advance(
            b"\x1b[?1015h\x1b[?1016h\x1b[?2031h\x1b[?2048h\x1b[?1006h\x1b[?2026h\x1b[?1015$p\x1b[?1016$p\x1b[?2031$p\x1b[?2048$p\x1b[?1006$p\x1b[?2026$p",
        )
        .expect("enable and query private modes")
        .pty_replies;
    assert_eq!(
        after_enable,
        b"\x1b[?1015;0$y\x1b[?1016;0$y\x1b[?2031;0$y\x1b[?2048;0$y\x1b[?1006;1$y\x1b[?2026;1$y"
    );
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        lector::terminal::MouseEncoding::Sgr,
        "clearing pixel mouse must preserve the requested cell-coordinate SGR encoding"
    );

    TerminalEngine::resize_with_geometry(&mut engine, TerminalGeometry::new(30, 100, 10, 20));
    let after_resize = engine
        .advance(b"\x1b[?2048$p")
        .expect("query mode after resize")
        .pty_replies;
    assert_eq!(after_resize, b"\x1b[?2048;0$y");
    assert!(
        !contains(&after_resize, b"\x1b[48;"),
        "an unsupported in-band resize report escaped after resize"
    );
}

#[test]
fn fragmented_multi_mode_enable_is_corrected_only_after_its_complete_csi() {
    let mut engine = GhosttyEngine::new_with_profile(
        24,
        80,
        VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark),
    )
    .expect("create fragmented-mode engine");

    let prefix = engine
        .advance(b"\x1b[")
        .expect("retain incomplete CSI without injecting policy bytes");
    assert!(prefix.pty_replies.is_empty());
    let completed = engine
        .advance(b"?1006;1016;2031;2048hready\x1b[?1016$p\x1b[?2031$p\x1b[?2048$p\x1b[?1006$p")
        .expect("complete blind multi-mode enable")
        .pty_replies;
    assert_eq!(
        completed,
        b"\x1b[?1016;0$y\x1b[?2031;0$y\x1b[?2048;0$y\x1b[?1006;1$y"
    );
    assert!(engine.snapshot().contents().contains("ready"));
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        lector::terminal::MouseEncoding::Sgr
    );

    TerminalEngine::resize_with_geometry(&mut engine, TerminalGeometry::new(30, 100, 10, 20));
    assert!(
        engine
            .advance(b"")
            .expect("collect any resize side effects")
            .pty_replies
            .is_empty(),
        "blind mode 2048 must not survive long enough to emit an in-band resize"
    );
}

#[test]
fn terminal_reset_does_not_resurrect_a_previously_requested_mouse_encoding() {
    let mut engine = GhosttyEngine::new_with_profile(
        24,
        80,
        VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark),
    )
    .expect("create reset-mode engine");
    engine
        .advance(b"\x1b[?1006h")
        .expect("enable supported SGR mouse");
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        lector::terminal::MouseEncoding::Sgr
    );

    let reset_reply = engine
        .advance(b"\x1bc\x1b[?1006$p")
        .expect("reset terminal and query SGR mouse")
        .pty_replies;
    assert_eq!(reset_reply, b"\x1b[?1006;2$y");
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        lector::terminal::MouseEncoding::Default
    );
}

#[test]
fn unsupported_resize_mode_does_not_change_supported_mouse_encoding_order() {
    let mut engine = GhosttyEngine::new_with_profile(
        24,
        80,
        VirtualTerminalProfile::lector(geometry(), ColorScheme::Dark),
    )
    .expect("create mouse-order engine");

    engine
        .advance(b"\x1b[?1006h\x1b[?1005h")
        .expect("select supported mouse encodings in one order");
    let before = engine.snapshot().mouse_protocol_encoding();
    engine
        .advance(b"\x1b[?2048h")
        .expect("blindly enable resize mode");
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        before,
        "clearing 2048 must not change the pre-existing encoding snapshot"
    );

    engine
        .advance(b"\x1b[?1005h\x1b[?1006h")
        .expect("select supported mouse encodings in the other order");
    let before = engine.snapshot().mouse_protocol_encoding();
    engine
        .advance(b"\x1b[?2048h")
        .expect("blindly enable resize mode again");
    assert_eq!(
        engine.snapshot().mouse_protocol_encoding(),
        before,
        "clearing 2048 must preserve the other encoding snapshot too"
    );
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
fn late_kitty_probe_atomically_replaces_the_non_kitty_keyboard_fallback() {
    let mut harness = Harness::new(24, 80).expect("create probe harness");
    harness.configure_physical_terminal(Some(false));
    harness
        .activate_physical_terminal()
        .expect("activate conservative terminal profile");
    assert!(contains(harness.terminal_output(), b"\x1b[>4;2m"));
    harness
        .start_capability_probes()
        .expect("start outer probes");
    let before_reply = harness.terminal_output().len();

    harness
        .handle_terminal_input(b"\x1b[?0u\x1b[?64;22c")
        .expect("consume Kitty capability and startup fence");

    let transition = &harness.terminal_output()[before_reply..];
    assert!(transition.starts_with(b"\x1b[>4;0m\x1b[>5u"));
    assert!(contains(transition, b"\x1b[=5u"));
    assert!(harness.physical_profile().kitty_keyboard);
    assert!(harness.application_input().is_empty());
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

use super::*;

fn input(is_tty: bool, pairs: &[(&str, &str)]) -> TerminalCapabilityInput {
    TerminalCapabilityInput {
        is_tty,
        env: pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        probe: TerminalProbeEvidence::default(),
    }
}

#[test]
fn unknown_and_blocked_sets_cover_lookup_and_terminal_guards() {
    let unknown = TerminalGraphicsCapabilities::unknown();
    assert_eq!(unknown.protocols.len(), 3);
    assert_eq!(
        unknown.by_protocol(GraphicsProtocol::Kitty).unwrap().source,
        "missing"
    );

    for (case, risk) in [
        (input(false, &[]), "non_tty"),
        (input(true, &[("TERM", "linux")]), "linux_console"),
        (input(true, &[("TERM", "screen-256color")]), "screen"),
    ] {
        let caps = detect_terminal_capabilities(case);
        assert_eq!(caps.graphics.preferred, None);
        assert!(caps
            .graphics
            .protocols
            .iter()
            .all(|cap| cap.status == CapabilityStatus::Blocked));
        assert!(caps.graphics.protocols[0]
            .risks
            .iter()
            .any(|item| item == risk));
    }

    let from_env = TerminalCapabilityInput::from_env(false).with_probe(TerminalProbeEvidence {
        kitty_graphics: Some("OK".into()),
        ..Default::default()
    });
    assert!(from_env.probe.kitty_graphics.is_some());
}

#[test]
fn active_replies_win_and_preserve_session_risks() {
    let mut case = input(
        true,
        &[
            ("TERM", "xterm-256color"),
            ("TMUX", "1"),
            ("SSH_TTY", "tty"),
        ],
    );
    case.probe = TerminalProbeEvidence {
        sixel_xtsmgraphics: Some("\x1b[?1;0;256S".into()),
        sixel_da1: Some("not-six".into()),
        kitty_graphics: Some("\x1b_Gi=1;OK\x1b\\".into()),
        iterm2_capabilities: Some("Capabilities=File".into()),
    };
    let caps = detect_terminal_capabilities(case);
    assert_eq!(caps.graphics.preferred, Some(GraphicsProtocol::Sixel));
    for protocol in [
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2File,
    ] {
        let cap = caps.graphics.by_protocol(protocol).unwrap();
        assert_eq!(cap.status, CapabilityStatus::Supported);
        assert_eq!(cap.evidence, EvidenceStrength::Probe);
        assert_eq!(cap.risks, ["tmux", "ssh"]);
    }
    assert!(xtsmgraphics_reports_sixel("\x1b[?1;0;256S"));
    assert!(primary_da_reports_sixel("noise\x1b[?62;4;22c"));
}

#[test]
fn host_signals_cover_blocked_supported_and_unknown_fallbacks() {
    let alacritty = detect_terminal_capabilities(input(
        true,
        &[("TERM", "alacritty"), ("TERM_PROGRAM", "Alacritty")],
    ));
    assert_eq!(
        alacritty
            .graphics
            .by_protocol(GraphicsProtocol::Sixel)
            .unwrap()
            .status,
        CapabilityStatus::Blocked
    );

    let windows_terminal =
        detect_terminal_capabilities(input(true, &[("TERM", "xterm"), ("WT_SESSION", "session")]));
    let sixel = windows_terminal
        .graphics
        .by_protocol(GraphicsProtocol::Sixel)
        .unwrap();
    assert_eq!(sixel.status, CapabilityStatus::Supported);
    assert!(sixel
        .risks
        .iter()
        .any(|risk| risk == "requires_windows_terminal_1_22"));

    let wezterm = detect_terminal_capabilities(input(
        true,
        &[("TERM", "foot"), ("TERM_PROGRAM", "WezTerm")],
    ));
    assert_eq!(wezterm.graphics.preferred, Some(GraphicsProtocol::Sixel));
    assert!(wezterm
        .graphics
        .protocols
        .iter()
        .all(|cap| cap.status == CapabilityStatus::Supported));

    let missing = detect_terminal_capabilities(input(true, &[]));
    assert!(missing
        .graphics
        .protocols
        .iter()
        .all(|cap| cap.status == CapabilityStatus::Unknown));
    assert_eq!(first_source(&[("A", ""), ("B", "")]), "unknown");
    assert!(contains_any("GhostTY", &["ghostty"]));
    assert!(!contains_any("plain", &["kitty"]));
}

#[cfg(feature = "client")]
#[test]
fn protobuf_round_trip_and_invalid_values_cover_every_enum_variant() {
    let statuses = [
        CapabilityStatus::Supported,
        CapabilityStatus::Unsupported,
        CapabilityStatus::Unknown,
        CapabilityStatus::Blocked,
    ];
    let evidence = [
        EvidenceStrength::Probe,
        EvidenceStrength::StrongHostSignal,
        EvidenceStrength::Terminfo,
        EvidenceStrength::WeakEnv,
        EvidenceStrength::UserOverride,
    ];
    let protocols = [
        GraphicsProtocol::Sixel,
        GraphicsProtocol::Kitty,
        GraphicsProtocol::Iterm2File,
    ];
    let caps = TerminalGraphicsCapabilities {
        protocols: statuses
            .iter()
            .enumerate()
            .map(|(index, status)| GraphicsCapability {
                protocol: protocols[index % protocols.len()],
                status: *status,
                evidence: evidence[index % evidence.len()],
                source: format!("source-{index}"),
                risks: vec!["risk".into()],
            })
            .chain(
                evidence
                    .iter()
                    .skip(statuses.len())
                    .map(|item| GraphicsCapability {
                        protocol: GraphicsProtocol::Iterm2File,
                        status: CapabilityStatus::Supported,
                        evidence: *item,
                        source: "extra".into(),
                        risks: Vec::new(),
                    }),
            )
            .collect(),
        preferred: Some(GraphicsProtocol::Kitty),
    };
    let wire = terminal_graphics_capabilities_to_proto(&caps);
    let round_trip = terminal_graphics_capabilities_from_proto(&wire);
    assert_eq!(round_trip, caps);

    let invalid = crate::proto::daemon::TerminalGraphicsCapabilities {
        protocols: vec![crate::proto::daemon::TerminalGraphicsCapability {
            protocol: 999,
            status: 999,
            evidence: 999,
            source: "invalid".into(),
            risks: vec![],
        }],
        preferred: crate::proto::daemon::GraphicsProtocol::Unspecified as i32,
    };
    let decoded = terminal_graphics_capabilities_from_proto(&invalid);
    assert_eq!(decoded.preferred, None);
    assert_eq!(decoded.protocols[0].protocol, GraphicsProtocol::Sixel);
    assert_eq!(decoded.protocols[0].status, CapabilityStatus::Unknown);
    assert_eq!(decoded.protocols[0].evidence, EvidenceStrength::WeakEnv);
}

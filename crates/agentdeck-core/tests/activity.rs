use std::collections::HashSet;

use agentdeck_core::activity::{
    BackgroundKind, BackgroundWork, ScreenReadSchedule, parse_background, parse_phase,
    summarize_background,
};

#[test]
fn status_line_uses_the_last_structural_match() {
    let screen =
        "✻ Brewing… (1m · ↓ 2k tokens)\n✻ Incubating… (2m 20s · ↓ 6.1k tokens · thought for 17s)";
    let Some(phase) = parse_phase(screen) else {
        panic!("fixture has a status line");
    };
    assert_eq!(phase.verb, "Incubating");
    assert_eq!(phase.elapsed.as_deref(), Some("2m 20s"));
    assert_eq!(phase.tokens.as_deref(), Some("6.1k"));
    assert!(phase.thinking);
}

#[test]
fn status_line_rejects_ordinary_prose_and_handles_missing_parts() {
    assert!(parse_phase("Incubating for two minutes").is_none());
    let Some(phase) = parse_phase("● Working… (thought for 2s)") else {
        panic!("minimal structural status line");
    };
    assert_eq!(phase.elapsed, None);
    assert_eq!(phase.tokens, None);
    assert!(phase.thinking);
}

#[test]
fn claude_shell_hint_is_live_background_work() {
    let screen = "✻ Brewed for 2m 0s · 1 shell still running\n\n───\n❯\n───\n  ⏵⏵ bypass permissions on · 1 shell · ← for agents · ↓ to manage";
    let work = parse_background(screen);
    assert_eq!(
        work,
        vec![BackgroundWork {
            kind: BackgroundKind::Shell,
            count: 1,
        }]
    );
    assert_eq!(summarize_background(&work).as_deref(), Some("1 shell"));
}

#[test]
fn claude_reports_countless_and_counted_subagents_without_invention() {
    let countless = parse_background("⏵⏵ bypass · /tasks to see subagents · ← for agents");
    assert_eq!(
        countless,
        vec![BackgroundWork {
            kind: BackgroundKind::Subagent,
            count: 0,
        }]
    );
    assert_eq!(
        summarize_background(&countless).as_deref(),
        Some("subagents")
    );

    let counted = parse_background("✻ Waiting for 2 background agents to finish");
    assert_eq!(
        summarize_background(&counted).as_deref(),
        Some("2 subagents")
    );
}

#[test]
fn shell_and_subagent_order_matches_contract() {
    let work = parse_background("⏵⏵ bypass · 1 shell · /tasks to see subagents");
    assert_eq!(
        summarize_background(&work).as_deref(),
        Some("1 shell · subagents")
    );
}

#[test]
fn codex_terminals_are_normalized_to_shells() {
    let plural = parse_background("2 background terminals running · /ps to view · /stop to close");
    assert_eq!(summarize_background(&plural).as_deref(), Some("2 shells"));
    let singular = parse_background("1 background terminal running · /ps to view");
    assert_eq!(summarize_background(&singular).as_deref(), Some("1 shell"));
}

#[test]
fn keybindings_pi_and_scrollback_prose_do_not_claim_background_work() {
    assert!(parse_background("⏵⏵ bypass · esc to interrupt · ← for agents").is_empty());
    assert!(parse_background("Manual · GPT-5.6 Sol · Max · 15k / 272k (5%)").is_empty());
    assert!(
        parse_background("✻ Brewed for 9m · 2 shells still running\nRan 2 shell commands")
            .is_empty()
    );
    assert!(summarize_background(&[]).is_none());
}

#[test]
fn only_fourteen_tail_lines_are_live() {
    let screen = format!(
        "⏵⏵ bypass · 4 shells · ← for agents\n{}\n⏵⏵ bypass · esc to interrupt",
        "\n".repeat(18)
    );
    assert!(parse_background(&screen).is_empty());
}

#[test]
fn trailing_newline_counts_as_a_tail_component() {
    let screen = format!(
        "⏵⏵ bypass · 4 shells · ← for agents\n{}\n",
        (0..13)
            .map(|index| format!("fresh line {index}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(parse_background(&screen).is_empty());
}

#[test]
fn crlf_and_unicode_newlines_count_as_components() {
    for separator in ["\r\n", "\u{0085}", "\u{2028}", "\u{2029}"] {
        let screen = format!(
            "⏵⏵ bypass · 4 shells · ← for agents{separator}{}{separator}",
            (0..13)
                .map(|index| format!("fresh line {index}"))
                .collect::<Vec<_>>()
                .join(separator)
        );
        assert!(
            parse_background(&screen).is_empty(),
            "stale line admitted for {separator:?}"
        );
    }
}

#[test]
fn screen_schedule_enforces_working_and_idle_cadence() {
    let mut schedule = ScreenReadSchedule::default();
    assert!(schedule.admit("p1", "pi", true, 10_000));
    assert!(!schedule.admit("p1", "pi", true, 10_999));
    assert!(schedule.admit("p1", "pi", true, 11_000));

    assert!(schedule.admit("p2", "claude", false, 20_000));
    assert!(!schedule.admit("p2", "claude", false, 24_999));
    assert!(schedule.admit("p2", "claude", false, 25_000));

    assert!(schedule.admit("p3", "codex", false, 30_000));
    assert!(!schedule.admit("p4", "pi", false, 30_000));
    assert!(!schedule.admit("p5", "copilot", false, 30_000));
}

#[test]
fn failed_time_progress_cannot_bypass_throttle_and_dead_panes_are_pruned() {
    let mut schedule = ScreenReadSchedule::default();
    assert!(schedule.admit("live", "claude", true, 50));
    assert!(!schedule.admit("live", "claude", true, 40));
    assert!(schedule.admit("dead", "codex", true, 50));
    assert_eq!(schedule.len(), 2);

    schedule.retain(&HashSet::from(["live".to_owned()]));
    assert_eq!(schedule.len(), 1);
    assert!(!schedule.is_empty());
    schedule.retain(&HashSet::new());
    assert!(schedule.is_empty());
}

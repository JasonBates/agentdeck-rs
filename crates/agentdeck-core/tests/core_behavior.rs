use std::cell::Cell;

use std::collections::HashMap;

use agentdeck_core::{
    AgentEnrichment, AssemblyEnrichments, AssemblyFeeds, CapacityFeed, Clock, ContextUsage,
    HerdrAgent, HerdrSnapshot, HerdrTab, HerdrWorkspace, HerdrWorktree, HostFeed, Phase,
    ReadTracker, TitleSource, UNKNOWN_AGENT_KIND, assemble_deck, assemble_deck_enriched,
    clean_title, is_generic_title, normalize_agent_kind, normalize_cwd, quantize_reply_age,
};

#[derive(Default)]
struct TestClock(Cell<i64>);

impl TestClock {
    fn set(&self, seconds: i64) {
        self.0.set(seconds);
    }
}

impl Clock for TestClock {
    fn now_seconds(&self) -> i64 {
        self.0.get()
    }
}

fn feeds() -> AssemblyFeeds {
    AssemblyFeeds {
        herdr_detail: Some("herdr test".to_owned()),
        capacity: CapacityFeed {
            ok: false,
            reason: Some("unavailable".to_owned()),
            providers: Vec::new(),
        },
        host: HostFeed {
            ok: false,
            load1: 0.0,
            load5: 0.0,
            cores: 0,
            system: None,
        },
        local_model: None,
        capabilities: None,
    }
}

fn workspace(id: &str, number: i64, focused: bool) -> HerdrWorkspace {
    HerdrWorkspace {
        workspace_id: id.to_owned(),
        label: Some(format!("workspace-{id}")),
        number: Some(number),
        agent_status: "idle".to_owned(),
        focused,
        worktree: Some(HerdrWorktree {
            repo_key: Some("agentdeck".to_owned()),
            repo_name: Some("AgentDeck".to_owned()),
        }),
    }
}

fn agent(pane: &str, workspace_id: &str, focused: bool, status: &str) -> HerdrAgent {
    HerdrAgent {
        pane_id: pane.to_owned(),
        kind: "claude".to_owned(),
        agent_status: status.to_owned(),
        cwd: "/tmp/agentdeck-test".to_owned(),
        focused,
        tab_id: format!("tab-{pane}"),
        workspace_id: workspace_id.to_owned(),
        terminal_title_stripped: Some("Reviewing the pill state".to_owned()),
        session: None,
        reply_key: None,
        transcript_written_at: None,
    }
}

fn snapshot(
    workspaces: Vec<HerdrWorkspace>,
    agents: Vec<HerdrAgent>,
    focused: Option<&str>,
) -> HerdrSnapshot {
    HerdrSnapshot {
        focused_workspace_id: focused.map(ToOwned::to_owned),
        workspaces,
        tabs: Vec::new(),
        agents,
    }
}

#[test]
fn new_tab_targets_focused_group_member() {
    let input = snapshot(
        vec![workspace("w1", 1, false), workspace("w2", 2, true)],
        Vec::new(),
        Some("w2"),
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );

    assert_eq!(payload.workspaces.len(), 1);
    assert_eq!(payload.workspaces[0].new_tab_workspace_id, "w2");
}

#[test]
fn focused_workspace_id_outranks_stale_member_focused_flags_for_new_tabs() {
    let input = snapshot(
        vec![workspace("w1", 1, true), workspace("w2", 2, true)],
        Vec::new(),
        Some("w2"),
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );

    assert_eq!(payload.workspaces[0].new_tab_workspace_id, "w2");
}

#[test]
fn new_tab_targets_first_group_member() {
    let input = snapshot(
        vec![workspace("w2", 2, false), workspace("w1", 1, false)],
        Vec::new(),
        Some("elsewhere"),
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );

    assert_eq!(payload.workspaces.len(), 1);
    assert_eq!(payload.workspaces[0].new_tab_workspace_id, "w1");
}

#[test]
fn read_reply_clears_rollup() {
    let clock = TestClock::default();
    let mut tracker = ReadTracker::new();
    let pane = "pane-unread";
    let _ = tracker.update(&clock, pane, false, Some("1"), None);
    let _ = tracker.update(&clock, pane, false, Some("2"), None);
    let mut unseen_agent = agent(pane, "w1", false, "idle");
    unseen_agent.reply_key = Some("2".to_owned());
    let unseen = snapshot(
        vec![workspace("w1", 1, false)],
        vec![unseen_agent],
        Some("w1"),
    );
    let payload = assemble_deck(&unseen, &mut tracker, &clock, feeds());
    assert_eq!(payload.workspaces[0].unseen_done, 1);
    assert_eq!(payload.workspaces[0].unread, 1);

    let mut read_agent = agent(pane, "w1", true, "idle");
    read_agent.reply_key = Some("2".to_owned());
    let read = snapshot(vec![workspace("w1", 1, true)], vec![read_agent], Some("w1"));
    let payload = assemble_deck(&read, &mut tracker, &clock, feeds());
    assert_eq!(payload.workspaces[0].unseen_done, 0);
    assert_eq!(payload.workspaces[0].unread, 0);
}

#[test]
fn working_outranks_unseen_done() {
    let clock = TestClock::default();
    let mut tracker = ReadTracker::new();
    let pane = "pane-working";
    let _ = tracker.update(&clock, pane, false, Some("1"), None);
    let _ = tracker.update(&clock, pane, false, Some("2"), None);
    let input = snapshot(
        vec![workspace("w1", 1, false)],
        vec![agent(pane, "w1", false, "working")],
        Some("w1"),
    );
    let payload = assemble_deck(&input, &mut tracker, &clock, feeds());

    assert_eq!(payload.workspaces[0].working, 1);
    assert_eq!(payload.workspaces[0].unseen_done, 0);
}

#[test]
fn first_sighting_dates_the_reply_from_the_transcript() {
    let clock = TestClock(Cell::new(10_000));
    let status = ReadTracker::new().update(&clock, "p1", false, Some("7"), Some(6_400));
    assert_eq!(status.replied_seconds_ago, Some(3_600));
    assert!(!status.unread);
}

#[test]
fn first_sighting_without_a_transcript_reports_no_time() {
    let clock = TestClock(Cell::new(10_000));
    let status = ReadTracker::new().update(&clock, "p1", false, Some("7"), None);
    assert_eq!(status.replied_seconds_ago, None);
    assert!(!status.unread);
}

#[test]
fn future_transcript_mtime_never_invents_a_negative_reply_age() {
    let clock = TestClock(Cell::new(10_000));
    let status = ReadTracker::new().update(&clock, "p1", false, Some("7"), Some(10_001));
    assert_eq!(status.replied_seconds_ago, Some(0));
}

#[test]
fn a_reply_that_lands_while_watching_is_dated_now() {
    let clock = TestClock(Cell::new(10_000));
    let mut tracker = ReadTracker::new();
    let _ = tracker.update(&clock, "p1", false, Some("7"), Some(6_400));
    clock.set(12_000);
    let status = tracker.update(&clock, "p1", false, Some("8"), Some(6_400));
    assert_eq!(status.replied_seconds_ago, Some(0));
    assert!(status.unread);
}

#[test]
fn pane_seen_before_its_transcript_exists_still_counts_as_first_sighting() {
    let clock = TestClock(Cell::new(10_000));
    let mut tracker = ReadTracker::new();
    let _ = tracker.update(&clock, "p1", false, None, None);
    let status = tracker.update(&clock, "p1", false, Some("7"), Some(6_400));
    assert_eq!(status.replied_seconds_ago, Some(3_600));
    assert!(!status.unread);
}

#[test]
fn pane_identity_change_cannot_inherit_reply_state() {
    let clock = TestClock(Cell::new(10_000));
    let mut tracker = ReadTracker::new();
    let first = tracker.update_for_identity(
        &clock,
        "p1",
        "copilot",
        "/old",
        None,
        false,
        Some("reply-1"),
        Some(9_900),
    );
    assert!(!first.unread);
    let unread = tracker.update_for_identity(
        &clock,
        "p1",
        "copilot",
        "/old",
        None,
        false,
        Some("reply-2"),
        Some(9_950),
    );
    assert!(unread.unread);

    let reused = tracker.update_for_identity(
        &clock,
        "p1",
        "copilot",
        "/new",
        None,
        false,
        Some("reply-2"),
        Some(9_950),
    );
    assert!(!reused.unread);
    assert_eq!(reused.replied_seconds_ago, Some(50));
}

#[test]
fn missing_reply_clears_prior_reply_metadata() {
    let clock = TestClock(Cell::new(10_000));
    let mut tracker = ReadTracker::new();
    let _ = tracker.update(&clock, "p1", false, Some("reply-1"), Some(9_900));
    let _ = tracker.update(&clock, "p1", false, Some("reply-2"), Some(9_950));

    let missing = tracker.update(&clock, "p1", false, None, None);
    assert!(!missing.unread);
    assert_eq!(missing.replied_seconds_ago, None);
}

#[test]
fn focus_clears_unread_and_retain_prunes_dead_panes() {
    let clock = TestClock::default();
    let mut tracker = ReadTracker::new();
    let _ = tracker.update(&clock, "live", false, Some("1"), None);
    let unread = tracker.update(&clock, "live", false, Some("2"), None);
    assert!(unread.unread);
    let read = tracker.update(&clock, "live", true, Some("2"), None);
    assert!(!read.unread);
    let _ = tracker.update(&clock, "dead", false, Some("1"), None);
    tracker.retain(&["live".to_owned()].into_iter().collect());
    assert_eq!(tracker.len(), 1);
}

#[test]
fn reply_age_quantizes_to_thirty_second_steps() {
    assert_eq!(quantize_reply_age(0), 0);
    assert_eq!(quantize_reply_age(29), 0);
    assert_eq!(quantize_reply_age(30), 30);
    assert_eq!(quantize_reply_age(89), 60);
    assert_eq!(quantize_reply_age(-1), 0);
    assert_eq!(quantize_reply_age(-29), 0);
    assert_eq!(quantize_reply_age(-30), -30);
    assert_eq!(quantize_reply_age(-31), -30);
    assert_eq!(quantize_reply_age(i64::MIN), i64::MIN / 30 * 30);
    assert_eq!(quantize_reply_age(i64::MAX), i64::MAX / 30 * 30);
}

#[test]
fn clean_titles_strip_spinners_and_generic_titles_promote_tab_labels() {
    assert_eq!(
        clean_title(Some(" ◐ ◓ Review pull request ")),
        "Review pull request"
    );
    assert_eq!(clean_title(Some("***")), "—");
    assert!(is_generic_title("π - Example", "Example"));
    assert!(is_generic_title("example", "Example"));
    assert!(is_generic_title("STRASSE", "Straße"));
    assert!(is_generic_title("ΟΣ", "ος"));
    assert!(is_generic_title("Cafe\u{301}", "Café"));
    assert!(!is_generic_title("Review deck parity", "Example"));

    let mut generic = agent("p1", "w1", false, "idle");
    generic.cwd = "/workspace/Example".to_owned();
    generic.terminal_title_stripped = Some("π - Example".to_owned());
    generic.tab_id = "t1".to_owned();
    let mut input = snapshot(vec![workspace("w1", 1, false)], vec![generic], None);
    input.tabs.push(HerdrTab {
        tab_id: "t1".to_owned(),
        workspace_id: "w1".to_owned(),
        label: Some("Chapter nine".to_owned()),
    });
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.agents[0].title, "Chapter nine");
    assert_eq!(payload.agents[0].tab_label, "");
    assert_eq!(payload.agents[0].title_source, TitleSource::Herdr);
}

#[test]
fn model_off_assembly_has_no_provider_output_or_call_path() {
    let input = snapshot(
        vec![workspace("w1", 1, false)],
        vec![agent("p1", "w1", false, "idle")],
        None,
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.agents[0].title_source, TitleSource::Herdr);
    assert!(payload.agents[0].focus.is_none());
    assert!(payload.agents[0].activity.is_none());
}

#[test]
fn optional_enrichment_overlays_one_card_without_affecting_other_cards_or_order() {
    let input = snapshot(
        vec![workspace("w1", 1, false)],
        vec![
            agent("plain", "w1", false, "idle"),
            agent("rich", "w1", false, "working"),
        ],
        None,
    );
    let enrichments = AssemblyEnrichments {
        by_pane: HashMap::from([(
            "rich".to_owned(),
            AgentEnrichment {
                model_title: Some("Build portable bridge".to_owned()),
                focus: Some("Wire transcript context".to_owned()),
                state: Some("Core is complete".to_owned()),
                phase: Some(Phase {
                    verb: "Testing".to_owned(),
                    elapsed: Some("2m".to_owned()),
                    tokens: Some("1.2k".to_owned()),
                    thinking: false,
                }),
                background: Some("2 shells".to_owned()),
                activity: Some("Running workspace tests".to_owned()),
                context: Some(ContextUsage {
                    used: 42_000,
                    limit: 200_000,
                    percent: 21,
                    model: Some("gpt-5.6-sol".to_owned()),
                }),
            },
        )]),
    };
    let payload = assemble_deck_enriched(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
        &enrichments,
    );

    assert_eq!(
        payload
            .agents
            .iter()
            .map(|card| card.pane_id.as_str())
            .collect::<Vec<_>>(),
        vec!["plain", "rich"]
    );
    assert_eq!(payload.agents[0].title_source, TitleSource::Herdr);
    assert_eq!(payload.agents[0].phase, None);
    let rich = &payload.agents[1];
    assert_eq!(rich.title, "Build portable bridge");
    assert_eq!(rich.title_source, TitleSource::Model);
    assert_eq!(rich.focus.as_deref(), Some("Wire transcript context"));
    assert_eq!(rich.state.as_deref(), Some("Core is complete"));
    assert_eq!(
        rich.phase.as_ref().map(|phase| phase.verb.as_str()),
        Some("Testing")
    );
    assert_eq!(rich.background.as_deref(), Some("2 shells"));
    assert_eq!(rich.activity.as_deref(), Some("Running workspace tests"));
    assert_eq!(
        rich.context.as_ref().map(|context| context.percent),
        Some(21)
    );
}

#[test]
fn promoted_tab_is_hidden_only_when_it_duplicates_the_visible_title() {
    let mut generic = agent("p", "w1", false, "idle");
    generic.cwd = "/workspace/Example".to_owned();
    generic.terminal_title_stripped = Some("π - Example".to_owned());
    generic.tab_id = "tab".to_owned();
    let mut input = snapshot(vec![workspace("w1", 1, false)], vec![generic], None);
    input.tabs.push(HerdrTab {
        tab_id: "tab".to_owned(),
        workspace_id: "w1".to_owned(),
        label: Some("Chapter nine".to_owned()),
    });
    let enrichments = AssemblyEnrichments {
        by_pane: HashMap::from([(
            "p".to_owned(),
            AgentEnrichment {
                model_title: Some("Revise movement nine".to_owned()),
                ..AgentEnrichment::default()
            },
        )]),
    };
    let payload = assemble_deck_enriched(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
        &enrichments,
    );
    assert_eq!(payload.agents[0].title, "Revise movement nine");
    assert_eq!(payload.agents[0].tab_label, "Chapter nine");

    let mut repeated = enrichments;
    repeated
        .by_pane
        .get_mut("p")
        .unwrap_or_else(|| panic!("fixture pane"))
        .model_title = Some("Chapter nine".to_owned());
    let payload = assemble_deck_enriched(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
        &repeated,
    );
    assert_eq!(payload.agents[0].tab_label, "");

    repeated
        .by_pane
        .get_mut("p")
        .unwrap_or_else(|| panic!("fixture pane"))
        .model_title = Some("Review portable dashboard release".to_owned());
    input.tabs[0].label = Some("Portable dashboard release project".to_owned());
    let payload = assemble_deck_enriched(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
        &repeated,
    );
    assert_eq!(payload.agents[0].tab_label, "");
}

#[test]
fn normalized_missing_kind_and_cwd_are_deliberate_safe_fallbacks() {
    assert_eq!(normalize_agent_kind(None), UNKNOWN_AGENT_KIND);
    assert_eq!(normalize_agent_kind(Some("copilot")), "copilot");
    assert_eq!(normalize_cwd(None), "");
}

#[test]
fn unknown_and_copilot_kinds_are_preserved_without_enrichment() {
    let mut unknown = agent("unknown", "w1", false, "idle");
    unknown.kind = UNKNOWN_AGENT_KIND.to_owned();
    unknown.cwd = String::new();
    let mut copilot = agent("copilot", "w1", false, "working");
    copilot.kind = "copilot".to_owned();
    let input = snapshot(
        vec![workspace("w1", 1, false)],
        vec![unknown, copilot],
        None,
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.agents[0].kind, UNKNOWN_AGENT_KIND);
    assert_eq!(payload.agents[0].cwd, "");
    assert_eq!(payload.agents[1].kind, "copilot");
    assert_eq!(payload.workspaces[0].working, 1);
}

#[test]
fn worktree_grouping_uses_repo_key_and_cwd_then_workspace_label_fallbacks() {
    let mut first = workspace("parent", 10, false);
    first.worktree = Some(HerdrWorktree {
        repo_key: Some("repo".to_owned()),
        repo_name: None,
    });
    let mut second = workspace("worktree", 11, false);
    second.worktree = Some(HerdrWorktree {
        repo_key: Some("repo".to_owned()),
        repo_name: None,
    });
    let mut first_agent = agent("p1", "parent", false, "idle");
    first_agent.cwd = "/long/path/Über-long-项目".to_owned();
    let input = snapshot(vec![first, second], vec![first_agent], None);
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.workspaces.len(), 1);
    assert_eq!(payload.workspaces[0].id, "repo");
    assert_eq!(payload.workspaces[0].label, "Über-long-项目");
    assert_eq!(payload.workspaces[0].agent_count, 1);
}

#[test]
fn cwd_basename_fallback_handles_windows_and_root_paths() {
    let mut no_repo = workspace("w1", 1, false);
    no_repo.worktree = None;
    let mut windows = agent("windows", "w1", false, "idle");
    windows.cwd = r"C:\repo\Very Long 项目".to_owned();
    let payload = assemble_deck(
        &snapshot(vec![no_repo], vec![windows], None),
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.workspaces[0].label, "Very Long 项目");

    let mut root = agent("root", "gone", false, "idle");
    root.cwd = "/".to_owned();
    let payload = assemble_deck(
        &snapshot(Vec::new(), vec![root], None),
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(payload.workspaces[0].label, "gone");
}

#[test]
fn every_card_is_in_exactly_one_project_without_loss_or_duplication() {
    let mut orphan = agent("orphan", "gone", false, "idle");
    orphan.cwd = String::new();
    let input = snapshot(
        vec![workspace("w1", 1, false)],
        vec![agent("a", "w1", false, "idle"), orphan],
        None,
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    let counted = payload
        .workspaces
        .iter()
        .map(|item| item.agent_count)
        .sum::<i64>();
    assert_eq!(payload.agents.len() as i64, counted);
    for card in &payload.agents {
        assert_eq!(
            payload
                .workspaces
                .iter()
                .filter(|item| item.id == card.project_id)
                .count(),
            1
        );
    }
}

#[test]
fn rollups_equal_the_cards_and_agent_order_follows_snapshot() {
    let mut second_workspace = workspace("w2", 2, false);
    second_workspace.worktree = None;
    let input = snapshot(
        vec![workspace("w1", 1, false), second_workspace],
        vec![
            agent("last-in-first-out", "w2", false, "working"),
            agent("then-this", "w1", false, "idle"),
        ],
        None,
    );
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(
        payload
            .agents
            .iter()
            .map(|card| card.pane_id.as_str())
            .collect::<Vec<_>>(),
        ["last-in-first-out", "then-this"]
    );
    for project in &payload.workspaces {
        let cards = payload
            .agents
            .iter()
            .filter(|card| card.project_id == project.id)
            .collect::<Vec<_>>();
        assert_eq!(project.agent_count, cards.len() as i64);
        assert_eq!(
            project.working,
            cards.iter().filter(|card| card.status == "working").count() as i64
        );
        assert_eq!(
            project.unread,
            cards.iter().filter(|card| card.unread).count() as i64
        );
        assert_eq!(
            project.unseen_done,
            cards
                .iter()
                .filter(|card| card.unread && card.status != "working")
                .count() as i64
        );
    }
}

#[test]
fn project_order_uses_workspace_number_with_stable_ties_and_input_permutations_are_visible() {
    let mut alpha = workspace("alpha", 2, false);
    alpha.worktree = None;
    let mut beta = workspace("beta", 1, false);
    beta.worktree = None;
    let mut gamma = workspace("gamma", 2, false);
    gamma.worktree = None;
    let input = snapshot(vec![alpha.clone(), gamma, beta.clone()], Vec::new(), None);
    let payload = assemble_deck(
        &input,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(
        payload
            .workspaces
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["beta", "alpha", "gamma"]
    );

    let reordered = snapshot(vec![beta, alpha], Vec::new(), None);
    let payload = assemble_deck(
        &reordered,
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );
    assert_eq!(
        payload
            .workspaces
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        ["beta", "alpha"]
    );
}

#[test]
fn grouped_project_number_is_minimum_known_member_number_and_order_is_stable() {
    let mut missing_number = workspace("parent", 999, false);
    missing_number.number = None;
    missing_number.worktree = Some(HerdrWorktree {
        repo_key: Some("grouped".to_owned()),
        repo_name: Some("Grouped".to_owned()),
    });
    let mut numbered_member = workspace("worktree", 10, false);
    numbered_member.worktree = Some(HerdrWorktree {
        repo_key: Some("grouped".to_owned()),
        repo_name: Some("Grouped".to_owned()),
    });
    let mut later = workspace("later", 5, false);
    later.worktree = None;

    let payload = assemble_deck(
        &snapshot(
            vec![missing_number, later, numbered_member],
            Vec::new(),
            None,
        ),
        &mut ReadTracker::new(),
        &TestClock::default(),
        feeds(),
    );

    assert_eq!(
        payload
            .workspaces
            .iter()
            .map(|item| (item.id.as_str(), item.number))
            .collect::<Vec<_>>(),
        // Grouped's first member has no number and is encountered before `later`
        // after the stable `(number ?? 0)` sort. It must remain first even though
        // its emitted minimum known number is 10 and later's is 5.
        [("grouped", 10), ("later", 5)]
    );
}

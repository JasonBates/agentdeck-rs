use agentdeck::adapters::herdr::{SnapshotDto, SnapshotMappingError, normalize_snapshot};
use agentdeck_core::UNKNOWN_AGENT_KIND;
use serde_json::Value;

const PROTOCOL_19: &str = include_str!("../../../Tests/fixtures/herdr/protocol-19-snapshot.json");
const PROTOCOL_20: &str = include_str!("../../../Tests/fixtures/herdr/protocol-20-snapshot.json");

fn fixture_snapshot(fixture: &str) -> SnapshotDto {
    let envelope: Value = serde_json::from_str(fixture)
        .unwrap_or_else(|error| panic!("invalid Herdr fixture: {error}"));
    serde_json::from_value(envelope["result"]["snapshot"].clone())
        .unwrap_or_else(|error| panic!("fixture snapshot did not decode: {error}"))
}

#[test]
fn protocol_19_maps_worktree_focus_unknown_kinds_and_sessions_without_reordering() {
    let mapped = normalize_snapshot(&fixture_snapshot(PROTOCOL_19))
        .unwrap_or_else(|error| panic!("protocol 19 mapping failed: {error}"));

    assert_eq!(mapped.focused_workspace_id.as_deref(), Some("w1"));
    assert_eq!(
        mapped
            .workspaces
            .iter()
            .map(|workspace| workspace.workspace_id.as_str())
            .collect::<Vec<_>>(),
        ["w1"]
    );
    assert_eq!(mapped.workspaces[0].number, Some(1));
    assert_eq!(
        mapped.workspaces[0]
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.repo_key.as_deref()),
        Some("repo:example")
    );
    assert_eq!(
        mapped.workspaces[0]
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.repo_name.as_deref()),
        Some("example")
    );
    assert_eq!(
        mapped
            .tabs
            .iter()
            .map(|tab| tab.tab_id.as_str())
            .collect::<Vec<_>>(),
        ["w1:t1"]
    );
    assert_eq!(
        mapped
            .agents
            .iter()
            .map(|agent| agent.pane_id.as_str())
            .collect::<Vec<_>>(),
        ["w1:p1", "w1:p2"]
    );
    assert_eq!(mapped.agents[0].kind, "claude");
    assert_eq!(
        mapped.agents[0].terminal_title_stripped.as_deref(),
        Some("Example task")
    );
    let session = mapped.agents[0]
        .session
        .as_ref()
        .unwrap_or_else(|| panic!("Claude session locator was dropped"));
    assert_eq!(session.source, "hook");
    assert_eq!(session.agent, "claude");
    assert_eq!(session.kind, "id");
    assert_eq!(session.value, "session-example-1");
    assert_eq!(mapped.agents[1].kind, "future-agent");
    assert_eq!(mapped.agents[1].cwd, "");
    assert!(mapped.agents[1].session.is_none());
}

#[test]
fn protocol_20_preserves_copilot_and_normalizes_an_absent_kind() {
    let mapped = normalize_snapshot(&fixture_snapshot(PROTOCOL_20))
        .unwrap_or_else(|error| panic!("protocol 20 mapping failed: {error}"));

    assert_eq!(mapped.agents[0].kind, "copilot");
    assert_eq!(mapped.agents[0].cwd, r"C:\workspace\portable");
    assert_eq!(mapped.agents[1].kind, UNKNOWN_AGENT_KIND);
    assert_eq!(mapped.agents[1].cwd, "");
    assert_eq!(mapped.tabs[0].label.as_deref(), Some("agents"));
}

#[test]
fn blank_agent_kind_uses_the_same_explicit_unknown_fallback() {
    let mut raw = fixture_snapshot(PROTOCOL_20);
    raw.agents[0].agent = Some("  ".to_owned());
    let mapped = normalize_snapshot(&raw)
        .unwrap_or_else(|error| panic!("blank-kind mapping failed: {error}"));
    assert_eq!(mapped.agents[0].kind, UNKNOWN_AGENT_KIND);
}

#[cfg(target_pointer_width = "64")]
#[test]
fn unrepresentable_workspace_number_fails_instead_of_wrapping() {
    let mut raw = fixture_snapshot(PROTOCOL_20);
    raw.workspaces[0].number = usize::MAX;
    assert!(matches!(
        normalize_snapshot(&raw),
        Err(SnapshotMappingError::WorkspaceNumber {
            workspace_id,
            number: usize::MAX
        }) if workspace_id == "w2"
    ));
}

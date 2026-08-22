use agentdeck_core::{
    HerdrAgent, HerdrAgentSession, HerdrSnapshot, HerdrTab, HerdrWorkspace, HerdrWorktree,
    normalize_agent_kind, normalize_cwd,
};
use thiserror::Error;

use super::SnapshotDto;

/// A raw protocol value could not be represented by the stable domain shape.
/// Failing the snapshot is safer than wrapping or clamping an identity/order field.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SnapshotMappingError {
    #[error("Herdr workspace {workspace_id:?} number {number} exceeds AgentDeck's integer range")]
    WorkspaceNumber { workspace_id: String, number: usize },
}

/// Convert a fully validated Herdr wire snapshot into the small observation shape
/// consumed by deterministic deck policy. Unknown agent kinds and optional session
/// locators are preserved; absent kind/cwd values receive the documented core fallbacks.
pub fn normalize_snapshot(raw: &SnapshotDto) -> Result<HerdrSnapshot, SnapshotMappingError> {
    let workspaces = raw
        .workspaces
        .iter()
        .map(|workspace| {
            let number = i64::try_from(workspace.number).map_err(|_| {
                SnapshotMappingError::WorkspaceNumber {
                    workspace_id: workspace.workspace_id.clone(),
                    number: workspace.number,
                }
            })?;
            Ok(HerdrWorkspace {
                workspace_id: workspace.workspace_id.clone(),
                label: Some(workspace.label.clone()),
                number: Some(number),
                agent_status: workspace.agent_status.clone(),
                focused: workspace.focused,
                worktree: workspace.worktree.as_ref().map(|worktree| HerdrWorktree {
                    repo_key: Some(worktree.repo_key.clone()),
                    repo_name: Some(worktree.repo_name.clone()),
                }),
            })
        })
        .collect::<Result<Vec<_>, SnapshotMappingError>>()?;

    let tabs = raw
        .tabs
        .iter()
        .map(|tab| HerdrTab {
            tab_id: tab.tab_id.clone(),
            workspace_id: tab.workspace_id.clone(),
            label: Some(tab.label.clone()),
        })
        .collect();

    let agents = raw
        .agents
        .iter()
        .map(|agent| HerdrAgent {
            pane_id: agent.pane_id.clone(),
            kind: normalize_agent_kind(agent.agent.as_deref()),
            agent_status: agent.agent_status.clone(),
            cwd: normalize_cwd(agent.cwd.as_deref()),
            focused: agent.focused,
            tab_id: agent.tab_id.clone(),
            workspace_id: agent.workspace_id.clone(),
            terminal_title_stripped: agent.terminal_title_stripped.clone(),
            session: agent
                .agent_session
                .as_ref()
                .map(|session| HerdrAgentSession {
                    source: session.source.clone(),
                    agent: session.agent.clone(),
                    kind: session.kind.clone(),
                    value: session.value.clone(),
                }),
            // Transcript enrichment owns these observations and fills them after
            // resolving the preserved session locator.
            reply_key: None,
            transcript_written_at: None,
        })
        .collect();

    Ok(HerdrSnapshot {
        focused_workspace_id: raw.focused_workspace_id.clone(),
        workspaces,
        tabs,
        agents,
    })
}

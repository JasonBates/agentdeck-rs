//! Snapshot-to-payload assembly with no provider, process, or network dependency.

use std::collections::{HashMap, HashSet};

use crate::{
    CapacityFeed, Clock, ContextUsage, DeckAgent, DeckCapabilities, DeckPayload, DeckWorkspace,
    FeedStatus, HerdrSnapshot, HerdrWorkspace, HostFeed, LocalModelSnapshot, Phase, ReadTracker,
    TitleSource, clean_title, headings::distinct, is_generic_title, quantize_reply_age,
};

/// Non-Herdr feeds supplied by service adapters. Assembly only copies them, ensuring
/// the model-off path has no heading/provider interface to invoke.
#[derive(Clone, Debug, PartialEq)]
pub struct AssemblyFeeds {
    pub herdr_detail: Option<String>,
    pub capacity: CapacityFeed,
    pub host: HostFeed,
    pub local_model: Option<LocalModelSnapshot>,
    pub capabilities: Option<DeckCapabilities>,
}

/// Optional observations for one card, already filtered by adapter/config policy.
///
/// A missing entry means model-off/generic behavior. Keeping this separate from Herdr's
/// normalized snapshot prevents optional adapters from becoming required inputs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentEnrichment {
    pub model_title: Option<String>,
    pub focus: Option<String>,
    pub state: Option<String>,
    pub phase: Option<Phase>,
    pub background: Option<String>,
    pub activity: Option<String>,
    pub context: Option<ContextUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssemblyEnrichments {
    pub by_pane: HashMap<String, AgentEnrichment>,
}

/// Builds a v1 payload from normalized observations. Agent and tab order are retained
/// exactly; project order is workspace number order with stable ties.
#[must_use]
pub fn assemble_deck(
    snapshot: &HerdrSnapshot,
    tracker: &mut ReadTracker,
    clock: &impl Clock,
    feeds: AssemblyFeeds,
) -> DeckPayload {
    assemble_deck_enriched(
        snapshot,
        tracker,
        clock,
        feeds,
        &AssemblyEnrichments::default(),
    )
}

/// Builds a payload while overlaying independently obtained optional observations.
#[must_use]
pub fn assemble_deck_enriched(
    snapshot: &HerdrSnapshot,
    tracker: &mut ReadTracker,
    clock: &impl Clock,
    feeds: AssemblyFeeds,
    enrichments: &AssemblyEnrichments,
) -> DeckPayload {
    let mut cards = Vec::with_capacity(snapshot.agents.len());
    for agent in &snapshot.agents {
        let enrichment = enrichments.by_pane.get(&agent.pane_id);
        let workspace = workspace_for(snapshot, &agent.workspace_id);
        let cwd_name = basename(&agent.cwd);
        let project_id = repo_key(workspace)
            .unwrap_or(&agent.workspace_id)
            .to_owned();
        let project = project_label(workspace, &cwd_name, &agent.workspace_id);
        let mut title = clean_title(agent.terminal_title_stripped.as_deref());
        let mut tab_label = tab_label(snapshot, &agent.tab_id)
            .unwrap_or_default()
            .to_owned();
        let promoted_tab = is_generic_title(&title, &cwd_name) && !tab_label.is_empty();
        let model_title = enrichment
            .and_then(|item| item.model_title.as_deref())
            .filter(|title| !title.is_empty());
        if promoted_tab && model_title.is_none() {
            title = tab_label.clone();
            tab_label.clear();
        }
        if let Some(model_title) = model_title {
            title = model_title.to_owned();
            // Treat a semantically overlapping model title and tab label as the same
            // address component, while retaining genuinely distinct user tab labels.
            if !distinct(&tab_label, Some(model_title)) {
                tab_label.clear();
            }
        }
        let read = tracker.update_for_identity(
            clock,
            &agent.pane_id,
            &agent.kind,
            &agent.cwd,
            agent.session.as_ref(),
            agent.focused,
            agent.reply_key.as_deref(),
            agent.transcript_written_at,
        );

        cards.push(DeckAgent {
            pane_id: agent.pane_id.clone(),
            kind: agent.kind.clone(),
            status: agent.agent_status.clone(),
            focused: agent.focused,
            title,
            title_source: if model_title.is_some() {
                TitleSource::Model
            } else {
                TitleSource::Herdr
            },
            focus: enrichment.and_then(|item| item.focus.clone()),
            state: enrichment.and_then(|item| item.state.clone()),
            unread: read.unread,
            replied_ago: read.replied_seconds_ago.map(quantize_reply_age),
            project_id,
            project,
            cwd: agent.cwd.clone(),
            workspace_id: agent.workspace_id.clone(),
            workspace_label: workspace
                .and_then(|item| item.label.as_deref())
                .filter(|label| !label.is_empty())
                .unwrap_or(&agent.workspace_id)
                .to_owned(),
            tab_label,
            phase: enrichment.and_then(|item| item.phase.clone()),
            background: enrichment.and_then(|item| item.background.clone()),
            activity: enrichment.and_then(|item| item.activity.clone()),
            context: enrichment.and_then(|item| item.context.clone()),
        });
    }

    let panes = cards
        .iter()
        .map(|card| card.pane_id.clone())
        .collect::<HashSet<_>>();
    tracker.retain(&panes);

    let mut projects = project_seeds(snapshot);
    // A malformed/partial snapshot can contain an agent whose workspace was removed.
    // Keep it visible as a singleton project after known workspace projects.
    for card in &cards {
        if !projects.iter().any(|project| project.id == card.project_id) {
            projects.push(ProjectSeed {
                id: card.project_id.clone(),
                members: Vec::new(),
                label: card.project.clone(),
                number: None,
            });
        }
    }

    let workspaces = projects
        .iter()
        .enumerate()
        .map(|(offset, project)| project_rollup(project, &cards, snapshot, offset))
        .collect();

    DeckPayload {
        herdr: FeedStatus {
            ok: true,
            detail: feeds.herdr_detail,
        },
        workspaces,
        agents: cards,
        capacity: feeds.capacity,
        host: feeds.host,
        local_model: feeds.local_model,
        capabilities: feeds.capabilities,
    }
}

fn workspace_for<'a>(snapshot: &'a HerdrSnapshot, id: &str) -> Option<&'a HerdrWorkspace> {
    snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.workspace_id == id)
}

fn tab_label<'a>(snapshot: &'a HerdrSnapshot, tab_id: &str) -> Option<&'a str> {
    snapshot
        .tabs
        .iter()
        .find(|tab| tab.tab_id == tab_id)
        .and_then(|tab| tab.label.as_deref())
        .filter(|label| !label.is_empty())
}

fn repo_key(workspace: Option<&HerdrWorkspace>) -> Option<&str> {
    workspace
        .and_then(|workspace| workspace.worktree.as_ref())
        .and_then(|worktree| worktree.repo_key.as_deref())
        .filter(|key| !key.is_empty())
}

fn project_label(workspace: Option<&HerdrWorkspace>, cwd_name: &str, fallback: &str) -> String {
    workspace
        .and_then(|item| item.worktree.as_ref())
        .and_then(|worktree| worktree.repo_name.as_deref())
        .filter(|name| !name.is_empty())
        .or_else(|| (!cwd_name.is_empty()).then_some(cwd_name))
        .or_else(|| {
            workspace
                .and_then(|item| item.label.as_deref())
                .filter(|label| !label.is_empty())
        })
        .unwrap_or(fallback)
        .to_owned()
}

fn basename(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[derive(Clone, Debug)]
struct ProjectSeed<'a> {
    id: String,
    members: Vec<&'a HerdrWorkspace>,
    label: String,
    number: Option<i64>,
}

fn project_seeds(snapshot: &HerdrSnapshot) -> Vec<ProjectSeed<'_>> {
    let mut ordered = snapshot.workspaces.iter().enumerate().collect::<Vec<_>>();
    ordered.sort_by_key(|(index, workspace)| (workspace.number.unwrap_or(0), *index));
    let mut projects = Vec::new();
    for (_, workspace) in ordered {
        let id = repo_key(Some(workspace)).unwrap_or(&workspace.workspace_id);
        if let Some(project) = projects
            .iter_mut()
            .find(|project: &&mut ProjectSeed<'_>| project.id == id)
        {
            project.members.push(workspace);
            project.number = match (project.number, workspace.number) {
                (Some(left), Some(right)) => Some(left.min(right)),
                (None, some @ Some(_)) => some,
                (some @ Some(_), None) => some,
                (None, None) => None,
            };
            continue;
        }
        projects.push(ProjectSeed {
            id: id.to_owned(),
            members: vec![workspace],
            label: project_label(Some(workspace), "", &workspace.workspace_id),
            number: workspace.number,
        });
    }
    // Keep this encounter order: it is the first project encounter after the stable
    // individual-workspace `(number ?? 0)` sort. `number` below is only the
    // emitted group number, and must not re-sort projects after grouping.
    projects
}

fn project_rollup(
    project: &ProjectSeed<'_>,
    cards: &[DeckAgent],
    snapshot: &HerdrSnapshot,
    offset: usize,
) -> DeckWorkspace {
    let mine = cards
        .iter()
        .filter(|card| card.project_id == project.id)
        .collect::<Vec<_>>();
    // The snapshot-level focused workspace is authoritative. Workspace `focused`
    // remains a compatibility fallback for incomplete snapshots.
    let focused_member = project
        .members
        .iter()
        .find(|member| {
            snapshot.focused_workspace_id.as_deref() == Some(member.workspace_id.as_str())
        })
        .or_else(|| project.members.iter().find(|member| member.focused));
    let new_tab_workspace_id = focused_member
        .or_else(|| project.members.first())
        .map(|member| member.workspace_id.clone())
        .unwrap_or_else(|| project.id.clone());
    let working = mine.iter().filter(|card| card.status == "working").count() as i64;
    let unseen_done = mine
        .iter()
        .filter(|card| card.unread && card.status != "working")
        .count() as i64;
    let focused = project.members.iter().any(|member| {
        member.focused
            || snapshot.focused_workspace_id.as_deref() == Some(member.workspace_id.as_str())
    });
    let status = if working > 0 {
        "working".to_owned()
    } else if let Some(member) = project.members.first() {
        member.agent_status.clone()
    } else {
        mine.first()
            .map(|card| card.status.clone())
            .unwrap_or_else(|| "unknown".to_owned())
    };

    DeckWorkspace {
        id: project.id.clone(),
        label: project
            .members
            .first()
            .and_then(|member| member.worktree.as_ref())
            .and_then(|worktree| worktree.repo_name.as_deref())
            .filter(|name| !name.is_empty())
            .map_or_else(
                || {
                    mine.first()
                        .map(|card| card.project.clone())
                        .filter(|label| !label.is_empty())
                        .unwrap_or_else(|| project.label.clone())
                },
                ToOwned::to_owned,
            ),
        new_tab_workspace_id,
        number: project.number.unwrap_or((offset + 1) as i64),
        status,
        focused,
        agent_count: mine.len() as i64,
        working,
        unseen_done,
        unread: mine.iter().filter(|card| card.unread).count() as i64,
    }
}

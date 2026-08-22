//! Normalized observations consumed by the deck policy.
//!
//! These are intentionally not wire/protocol DTOs. The Herdr adapter owns decoding
//! and validation, then supplies this small, stable shape to the pure core.

/// Deliberate adapter-boundary fallback for an absent optional Herdr agent kind.
pub const UNKNOWN_AGENT_KIND: &str = "unknown";

/// Preserves every reported kind, while making an absent optional kind visible rather
/// than pretending it is Claude, Codex, or any other supported implementation.
#[must_use]
pub fn normalize_agent_kind(kind: Option<&str>) -> String {
    kind.filter(|value| !value.trim().is_empty())
        .unwrap_or(UNKNOWN_AGENT_KIND)
        .to_owned()
}

/// Deliberate adapter-boundary fallback for an absent optional working directory.
///
/// An empty cwd is preferable to inventing a path; project labels then fall back to
/// workspace identity and the card remains usable.
#[must_use]
pub fn normalize_cwd(cwd: Option<&str>) -> String {
    cwd.unwrap_or_default().to_owned()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrSnapshot {
    pub focused_workspace_id: Option<String>,
    pub workspaces: Vec<HerdrWorkspace>,
    pub tabs: Vec<HerdrTab>,
    pub agents: Vec<HerdrAgent>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrWorktree {
    pub repo_key: Option<String>,
    pub repo_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrWorkspace {
    pub workspace_id: String,
    pub label: Option<String>,
    pub number: Option<i64>,
    pub agent_status: String,
    pub focused: bool,
    pub worktree: Option<HerdrWorktree>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrTab {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: Option<String>,
}

/// Herdr's stable transcript/session locator. The adapter validates the raw DTO
/// before this value enters core policy; the transcript layer still decides
/// whether a particular agent/kind combination is supported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrAgentSession {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HerdrAgent {
    pub pane_id: String,
    pub kind: String,
    pub agent_status: String,
    pub cwd: String,
    pub focused: bool,
    pub tab_id: String,
    pub workspace_id: String,
    pub terminal_title_stripped: Option<String>,
    pub session: Option<HerdrAgentSession>,
    /// Adapter-derived stable completed-assistant-reply identity (normally a digest),
    /// if available. It is opaque rather than numeric so adapters never truncate a
    /// cryptographic key or inherit process-randomized numeric hash semantics.
    pub reply_key: Option<String>,
    /// Transcript mtime in Unix seconds; only used for the first sighting.
    pub transcript_written_at: Option<i64>,
}

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SchemaDto {
    pub protocol: u32,
    pub schema_version: u32,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SnapshotDto {
    pub version: String,
    pub protocol: u32,
    pub agents: Vec<AgentDto>,
    pub workspaces: Vec<WorkspaceDto>,
    pub tabs: Vec<TabDto>,
    #[serde(default)]
    pub focused_pane_id: Option<String>,
    #[serde(default)]
    pub focused_tab_id: Option<String>,
    #[serde(default)]
    pub focused_workspace_id: Option<String>,
    /// Required by protocols 19 and 20. AgentDeck intentionally does not model
    /// pane geometry at this boundary.
    pub panes: Vec<Value>,
    /// Required by protocols 19 and 20. AgentDeck intentionally does not model
    /// layout geometry at this boundary.
    pub layouts: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AgentSessionDto {
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AgentDto {
    pub terminal_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSessionDto>,
    pub agent_status: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub focused: bool,
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    pub state_change_seq: Option<u64>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorktreeDto {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    pub is_linked_worktree: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorkspaceDto {
    pub workspace_id: String,
    pub label: String,
    pub number: usize,
    pub agent_status: String,
    pub focused: bool,
    pub pane_count: usize,
    pub tab_count: usize,
    pub active_tab_id: String,
    #[serde(default)]
    pub worktree: Option<WorktreeDto>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TabDto {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
    pub number: usize,
    pub agent_status: String,
    pub focused: bool,
    pub pane_count: usize,
}

/// A successful Herdr mutation response. The adapter validates `result.type`
/// before returning this envelope and preserves command-specific fields for
/// later runtime consumers without weakening the snapshot DTO.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CommandEnvelopeDto {
    pub id: String,
    pub result: CommandResultDto,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct CommandResultDto {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotEnvelope {
    pub id: String,
    pub result: SnapshotResult,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotResult {
    #[serde(rename = "type")]
    pub kind: String,
    pub snapshot: SnapshotDto,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorEnvelope {
    #[serde(default)]
    pub id: Option<String>,
    pub error: ApiErrorBody,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

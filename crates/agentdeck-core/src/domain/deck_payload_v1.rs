use serde::{Deserialize, Serialize};

/// The complete browser payload emitted by the compatibility bridge.
///
/// There is deliberately no timestamp: canonical bytes are used to suppress
/// no-op broadcasts, while the browser derives staleness from frame arrival.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckPayload {
    pub herdr: FeedStatus,
    pub workspaces: Vec<DeckWorkspace>,
    pub agents: Vec<DeckAgent>,
    pub capacity: CapacityFeed,
    pub host: HostFeed,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_model: Option<LocalModelSnapshot>,
    /// Additive capability metadata. Baseline payload producers may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<DeckCapabilities>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckCapabilities {
    pub headings: CapabilityStatus,
    pub capacity: CapabilityStatus,
    pub host_telemetry: CapabilityStatus,
    pub local_model_telemetry: CapabilityStatus,
    pub tab_title_sync: CapabilityStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityStatus {
    pub state: CapabilityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<CapabilityBackend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<CapabilityLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<CapabilityReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_hint: Option<SetupHint>,
}

/// Closed provider names prevent payloads from accidentally carrying executable
/// paths, endpoints, credentials, or transcript-derived strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityBackend {
    Herdr,
    None,
    Ollama,
    Codexbar,
    Native,
    System,
}

/// Stable diagnostic codes only; human detail remains in typed health/log surfaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityReason {
    ProviderMissing,
    ModelMissing,
    ModelUnconfigured,
    ProviderDisabled,
    ProviderFailed,
    ConnectionFailed,
    Timeout,
    InvalidData,
    Unsupported,
    SamplerFailed,
    StateWriteFailed,
    NotRefreshed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityState {
    Available,
    Missing,
    Disabled,
    Unsupported,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CapabilityLevel {
    Basic,
    Detailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupHint {
    pub message: String,
    pub action_label: String,
    pub docs_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedStatus {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A repository-level group. The historic `workspaces` wire name is retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckWorkspace {
    pub id: String,
    pub label: String,
    pub new_tab_workspace_id: String,
    pub number: i64,
    pub status: String,
    pub focused: bool,
    pub agent_count: i64,
    pub working: i64,
    pub unseen_done: i64,
    pub unread: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckAgent {
    pub pane_id: String,
    pub kind: String,
    pub status: String,
    pub focused: bool,
    pub title: String,
    pub title_source: TitleSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub unread: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replied_ago: Option<i64>,
    pub project_id: String,
    pub project: String,
    pub cwd: String,
    pub workspace_id: String,
    pub workspace_label: String,
    pub tab_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextUsage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TitleSource {
    #[serde(rename = "model")]
    Model,
    #[serde(rename = "herdr")]
    Herdr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    pub used: i64,
    pub limit: i64,
    pub percent: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Phase {
    pub verb: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<String>,
    pub thinking: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityFeed {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub providers: Vec<CapacityProvider>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityProvider {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent_used: Option<f64>,
    pub label: String,
    pub windows: Vec<CapacityWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityWindow {
    pub span: String,
    pub used: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostFeed {
    pub ok: bool,
    pub load1: f64,
    pub load5: f64,
    pub cores: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemSnapshot {
    pub cpu_user: f64,
    pub cpu_sys: f64,
    pub cpu_busy: f64,
    pub cpu_history: Vec<i64>,
    #[serde(rename = "ramUsedGB")]
    pub ram_used_gb: f64,
    #[serde(rename = "ramTotalGB")]
    pub ram_total_gb: f64,
    pub ram_percent: i64,
    #[serde(rename = "compressorGB")]
    pub compressor_gb: f64,
    #[serde(rename = "swapUsedGB")]
    pub swap_used_gb: f64,
    #[serde(rename = "swapTotalGB")]
    pub swap_total_gb: f64,
    pub swap_percent: i64,
    pub agent_procs: i64,
    #[serde(rename = "agentCPU")]
    pub agent_cpu: f64,
    #[serde(rename = "agentRSSGB")]
    pub agent_rssgb: f64,
    pub load1: f64,
    pub cores: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelSnapshot {
    pub name: String,
    pub status: LocalModelStatus,
    #[serde(
        rename = "residentGB",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resident_gb: Option<f64>,
    pub context: i64,
    pub calls: Vec<LocalModelCall>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalModelStatus {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "busy")]
    Busy,
    #[serde(rename = "unloaded")]
    Unloaded,
    #[serde(rename = "offline")]
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelCall {
    pub at: i64,
    pub ms: i64,
    pub ok: bool,
}

//! Deterministic domain types and policy for AgentDeck.
//!
//! This crate deliberately has no process, network, clock, environment, or
//! platform-directory access. Those concerns belong to the `agentdeck` binary.

pub mod activity;
mod assemble;
pub mod context;
pub mod domain;
pub mod headings;
mod observations;
mod read_tracker;
pub mod tab_titles;
mod titles;
pub mod transcript;

pub use assemble::{
    AgentEnrichment, AssemblyEnrichments, AssemblyFeeds, assemble_deck, assemble_deck_enriched,
};
pub use domain::{
    CapabilityBackend, CapabilityLevel, CapabilityReason, CapabilityState, CapabilityStatus,
    CapacityFeed, CapacityProvider, CapacityWindow, ContextUsage, DeckAgent, DeckCapabilities,
    DeckPayload, DeckWorkspace, FeedStatus, HostFeed, LocalModelCall, LocalModelSnapshot,
    LocalModelStatus, Phase, SetupHint, SystemSnapshot, TitleSource,
};
pub use observations::{
    HerdrAgent, HerdrAgentSession, HerdrSnapshot, HerdrTab, HerdrWorkspace, HerdrWorktree,
    UNKNOWN_AGENT_KIND, normalize_agent_kind, normalize_cwd,
};
pub use read_tracker::{Clock, ReadStatus, ReadTracker, quantize_reply_age};
pub use titles::{clean_title, is_generic_title};

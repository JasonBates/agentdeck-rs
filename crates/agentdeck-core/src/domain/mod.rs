//! Versioned wire-domain types.

mod deck_payload_v1;

pub use deck_payload_v1::{
    CapabilityBackend, CapabilityLevel, CapabilityReason, CapabilityState, CapabilityStatus,
    CapacityFeed, CapacityProvider, CapacityWindow, ContextUsage, DeckAgent, DeckCapabilities,
    DeckPayload, DeckWorkspace, FeedStatus, HostFeed, LocalModelCall, LocalModelSnapshot,
    LocalModelStatus, Phase, SetupHint, SystemSnapshot, TitleSource,
};

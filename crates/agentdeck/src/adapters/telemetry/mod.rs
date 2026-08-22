//! Optional, bounded, local-only telemetry adapters.
//!
//! These adapters deliberately do not own the deck runtime.  They return closed
//! capability DTOs plus optional wire feeds; the future state owner decides when
//! to merge them into a payload.

pub mod capacity;
pub mod host;
pub mod ollama;

use agentdeck_core::{
    CapabilityBackend, CapabilityReason, CapabilityState, CapabilityStatus, SetupHint,
};

pub(crate) fn capability(
    state: CapabilityState,
    backend: Option<CapabilityBackend>,
    reason: Option<CapabilityReason>,
    setup_hint: Option<SetupHint>,
) -> CapabilityStatus {
    CapabilityStatus {
        state,
        backend,
        level: None,
        reason,
        setup_hint,
    }
}

pub(crate) fn codexbar_setup_hint() -> SetupHint {
    SetupHint {
        message: "Install CodexBar to show Claude and Codex quota.".to_owned(),
        action_label: "Learn more".to_owned(),
        docs_path: "docs/setup.html#claude-and-codex-capacity".to_owned(),
        command: None,
    }
}

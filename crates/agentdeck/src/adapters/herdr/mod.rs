//! Bounded Herdr CLI adapter.
//!
//! Ordinary reads and mutations use Herdr's version-matched CLI. Only the
//! long-lived invalidation subscription uses Herdr's raw local-socket API.

mod client;
mod dto;
mod error;
mod events;
mod mapper;
mod process;
mod protocol;
mod routing;

pub use client::{HerdrClient, VisibleLines};
pub use dto::{
    AgentDto, AgentSessionDto, CommandEnvelopeDto, CommandResultDto, SchemaDto, SnapshotDto,
    TabDto, WorkspaceDto, WorktreeDto,
};
pub use error::{HerdrError, OutputStream, ProcessError};
pub use events::{
    EVENT_SUBSCRIPTIONS, EVENT_WIRE_NAMES, EventEndpoint, EventError, EventFrameDecoder,
    EventLoopOptions, FrameAction, ReconnectBackoff, SUBSCRIPTION_ID, connect_event_endpoint,
    event_subscription_request, herdr_config_dir_with, resolve_event_endpoint_with,
    run_event_subscription, run_event_subscription_with_jitter,
};
pub use mapper::{SnapshotMappingError, normalize_snapshot};
pub use process::{CommandLimits, CommandOutput, CommandSpec, ProcessRunner, TokioProcessRunner};
pub use protocol::{ProtocolSupport, assess_protocol};
pub use routing::{HerdrTarget, resolve_herdr_binary, resolve_herdr_binary_with};

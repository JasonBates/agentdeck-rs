//! In-memory unread and reply-age policy. Time is injected by the caller.

use std::collections::{HashMap, HashSet};

use crate::HerdrAgentSession;

/// Supplies time to the pure tracker. Production can adapt a system clock; tests use
/// a fixed clock, so no policy test depends on wall-clock time.
pub trait Clock {
    fn now_seconds(&self) -> i64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadStatus {
    pub unread: bool,
    pub replied_seconds_ago: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct ReadTracker {
    entries: HashMap<String, Entry>,
}

#[derive(Clone, Debug, Default)]
struct Entry {
    identity: Option<AgentIdentity>,
    reply_key: Option<String>,
    replied_at: Option<i64>,
    seen_reply_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentIdentity {
    kind: String,
    cwd: String,
    session: Option<HerdrAgentSession>,
}

impl ReadTracker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates one pane once per observation cycle.
    pub fn update(
        &mut self,
        clock: &impl Clock,
        pane: &str,
        focused: bool,
        reply_key: Option<&str>,
        written_at: Option<i64>,
    ) -> ReadStatus {
        self.update_inner(clock, pane, None, focused, reply_key, written_at)
    }

    /// Updates one pane while binding its read state to the current agent identity.
    /// A reused pane ID must never inherit read state from a prior process/session.
    #[allow(clippy::too_many_arguments)]
    pub fn update_for_identity(
        &mut self,
        clock: &impl Clock,
        pane: &str,
        kind: &str,
        cwd: &str,
        session: Option<&HerdrAgentSession>,
        focused: bool,
        reply_key: Option<&str>,
        written_at: Option<i64>,
    ) -> ReadStatus {
        let identity = AgentIdentity {
            kind: kind.to_owned(),
            cwd: cwd.to_owned(),
            session: session.cloned(),
        };
        self.update_inner(clock, pane, Some(identity), focused, reply_key, written_at)
    }

    fn update_inner(
        &mut self,
        clock: &impl Clock,
        pane: &str,
        identity: Option<AgentIdentity>,
        focused: bool,
        reply_key: Option<&str>,
        written_at: Option<i64>,
    ) -> ReadStatus {
        let now = clock.now_seconds();
        let entry = self.entries.entry(pane.to_owned()).or_default();
        if let Some(identity) = identity {
            if entry.identity.as_ref().is_some_and(|old| old != &identity) {
                *entry = Entry::default();
            }
            entry.identity = Some(identity);
        }

        if let Some(key) = reply_key.filter(|key| !key.is_empty()) {
            if entry.reply_key.as_deref() != Some(key) {
                let first_sighting = entry.reply_key.is_none();
                entry.reply_key = Some(key.to_owned());
                entry.replied_at = if first_sighting {
                    written_at
                } else {
                    Some(now)
                };
                if first_sighting {
                    entry.seen_reply_key = Some(key.to_owned());
                }
            }
        } else {
            entry.reply_key = None;
            entry.replied_at = None;
            entry.seen_reply_key = None;
        }

        if focused {
            entry.seen_reply_key = entry.reply_key.clone();
        }

        ReadStatus {
            unread: entry.reply_key.is_some() && entry.reply_key != entry.seen_reply_key,
            replied_seconds_ago: entry.replied_at.map(|at| now.saturating_sub(at).max(0)),
        }
    }

    /// Drops state for panes no longer present in the authoritative snapshot.
    pub fn retain(&mut self, panes: &HashSet<String>) {
        self.entries.retain(|pane, _| panes.contains(pane));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Floors reply ages to the 30-second wire cadence used to suppress no-op updates.
#[must_use]
pub const fn quantize_reply_age(seconds: i64) -> i64 {
    // Signed integer division truncates toward zero. Division
    // happens before multiplication, so both i64 extremes remain safe. ReadTracker
    // itself still clamps its produced ages to nonnegative before calling this helper.
    (seconds / 30) * 30
}

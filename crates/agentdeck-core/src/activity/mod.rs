//! Pure parsing and scheduling policy for terminal-screen enrichment.
//!
//! Screen acquisition belongs to the executable adapter. This module accepts bounded
//! text and explicit timestamps so it remains deterministic and cannot perform I/O.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::domain::Phase;

const DEFAULT_BACKGROUND_TAIL_LINES: usize = 14;
const WORKING_READ_INTERVAL_MS: u64 = 1_000;
const IDLE_READ_INTERVAL_MS: u64 = 5_000;

static STATUS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^\s\w]\s*([A-Z][a-z]+)\u{2026}\s*\(([^)]*)\)")
        .unwrap_or_else(|error| panic!("static status-line regex must compile: {error}"))
});
static SHELL_SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+)\s+shells?$")
        .unwrap_or_else(|error| panic!("static shell regex must compile: {error}"))
});
static TERMINAL_SEGMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d+)\s+background terminals?\s+running$")
        .unwrap_or_else(|error| panic!("static terminal regex must compile: {error}"))
});
static WAITING_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Waiting for (\d+) background agents?\s+to finish")
        .unwrap_or_else(|error| panic!("static waiting regex must compile: {error}"))
});

/// Background work that a terminal client explicitly reports.
///
/// A zero count means presence was reported without a count. It must not be promoted
/// to one: doing so would make AgentDeck claim evidence the agent never supplied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundWork {
    pub kind: BackgroundKind,
    pub count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundKind {
    Shell,
    Subagent,
}

impl BackgroundKind {
    const fn singular(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Subagent => "subagent",
        }
    }
}

/// Parse the last matching Claude-style status line from a bounded screen tail.
#[must_use]
pub fn parse_phase(screen: &str) -> Option<Phase> {
    let captures = STATUS_LINE.captures_iter(screen).last()?;
    let verb = captures.get(1)?.as_str().to_owned();
    let inner = captures.get(2)?.as_str();
    let mut elapsed = None;
    let mut tokens = None;

    for part in inner.split('·').map(str::trim) {
        if part.contains("token") {
            let value = part
                .replace('↓', "")
                .replace("tokens", "")
                .replace("token", "")
                .trim()
                .to_owned();
            if !value.is_empty() {
                tokens = Some(value);
            }
        } else if elapsed.is_none()
            && part
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
        {
            elapsed = Some(part.to_owned());
        }
    }

    Some(Phase {
        verb,
        elapsed,
        tokens,
        thinking: inner.contains("thought for"),
    })
}

/// Parse live background-work indicators from only the redrawable screen tail.
#[must_use]
pub fn parse_background(screen: &str) -> Vec<BackgroundWork> {
    parse_background_tail(screen, DEFAULT_BACKGROUND_TAIL_LINES)
}

#[must_use]
pub fn parse_background_tail(screen: &str, tail_lines: usize) -> Vec<BackgroundWork> {
    // Split on the recognized newline set and retain empty/final components. Those
    // components count toward the 14-line live tail; `str::lines()` would admit stale
    // scrollback around LF/CRLF.
    let lines = screen.split(is_contract_newline).collect::<Vec<_>>();
    let start = lines.len().saturating_sub(tail_lines);
    let mut shells = 0_u64;
    let mut subagents = 0_u64;
    let mut subagents_running = false;

    for line in &lines[start..] {
        if line.contains("/tasks to see subagents") {
            subagents_running = true;
        }
        if let Some(count) = capture_count(&WAITING_LINE, line).filter(|count| *count > 0) {
            subagents_running = true;
            subagents = subagents.max(count);
        }

        for candidate in std::iter::once(line.trim()).chain(line.split('·').map(str::trim)) {
            if let Some(count) = capture_count(&SHELL_SEGMENT, candidate) {
                shells = shells.max(count);
            }
            if let Some(count) = capture_count(&TERMINAL_SEGMENT, candidate) {
                shells = shells.max(count);
            }
        }
    }

    let mut work = Vec::with_capacity(2);
    if shells > 0 {
        work.push(BackgroundWork {
            kind: BackgroundKind::Shell,
            count: shells,
        });
    }
    if subagents_running {
        work.push(BackgroundWork {
            kind: BackgroundKind::Subagent,
            count: subagents,
        });
    }
    work
}

fn is_contract_newline(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

fn capture_count(pattern: &Regex, text: &str) -> Option<u64> {
    pattern.captures(text)?.get(1)?.as_str().parse::<u64>().ok()
}

/// Render background observations exactly once at the domain boundary.
#[must_use]
pub fn summarize_background(work: &[BackgroundWork]) -> Option<String> {
    if work.is_empty() {
        return None;
    }

    Some(
        work.iter()
            .map(|item| {
                let noun = item.kind.singular();
                if item.count == 0 {
                    format!("{noun}s")
                } else if item.count == 1 {
                    format!("1 {noun}")
                } else {
                    format!("{} {noun}s", item.count)
                }
            })
            .collect::<Vec<_>>()
            .join(" · "),
    )
}

/// Deterministic admission policy for screen reads.
///
/// The adapter still applies a timeout and a bounded line count. This policy only
/// decides whether a read is due, and records an admitted attempt even if it fails so
/// an unavailable pane cannot be hammered on every reconciliation tick.
#[derive(Clone, Debug, Default)]
pub struct ScreenReadSchedule {
    last_attempt_ms: HashMap<String, u64>,
}

impl ScreenReadSchedule {
    #[must_use]
    pub fn admit(&mut self, pane_id: &str, agent_kind: &str, working: bool, now_ms: u64) -> bool {
        let interval = if working {
            WORKING_READ_INTERVAL_MS
        } else if matches!(agent_kind, "claude" | "codex") {
            IDLE_READ_INTERVAL_MS
        } else {
            return false;
        };

        if self
            .last_attempt_ms
            .get(pane_id)
            .is_some_and(|previous| now_ms.saturating_sub(*previous) < interval)
        {
            return false;
        }
        self.last_attempt_ms.insert(pane_id.to_owned(), now_ms);
        true
    }

    pub fn retain(&mut self, live_panes: &HashSet<String>) {
        self.last_attempt_ms
            .retain(|pane_id, _| live_panes.contains(pane_id));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.last_attempt_ms.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.last_attempt_ms.is_empty()
    }
}

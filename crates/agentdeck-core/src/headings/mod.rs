//! Deterministic heading prompts, output cleanup, and quality gates.
//!
//! Provider discovery, HTTP, timeouts, and concurrency belong to executable adapters.

use std::collections::{HashMap, HashSet};

use unicode_segmentation::UnicodeSegmentation;

use crate::transcript::TranscriptDigest;

pub const TITLE_MAX_TOKENS: u32 = 40;
pub const SUBTITLE_MAX_TOKENS: u32 = 32;
pub const OUTCOME_MAX_TOKENS: u32 = 110;
pub const ACTIVITY_MAX_TOKENS: u32 = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadingKind {
    Title,
    Subtitle,
    Outcome,
    Activity,
}

impl HeadingKind {
    #[must_use]
    pub const fn max_tokens(self) -> u32 {
        match self {
            Self::Title => TITLE_MAX_TOKENS,
            Self::Subtitle => SUBTITLE_MAX_TOKENS,
            Self::Outcome => OUTCOME_MAX_TOKENS,
            Self::Activity => ACTIVITY_MAX_TOKENS,
        }
    }

    #[must_use]
    pub const fn max_characters(self) -> usize {
        match self {
            Self::Title | Self::Activity => 90,
            Self::Subtitle => 130,
            Self::Outcome => 380,
        }
    }

    #[must_use]
    pub const fn multiline(self) -> bool {
        matches!(self, Self::Outcome)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadingJob {
    pub kind: HeadingKind,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadingRejection {
    EmptyOrTooShort,
    TooLong,
    AssistantAction,
    TooCloseToTitle,
}

const FAILURE_COOLDOWN_MS: u64 = 20_000;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcceptedHeadings {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub outcome: Option<String>,
    pub activity: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptHeadingPlan {
    pub prompts_seen: usize,
    pub title: bool,
    pub subtitle: bool,
    pub outcome: bool,
    prompt_key: Option<String>,
    reply_key: Option<String>,
    attempted_at_ms: u64,
}

impl TranscriptHeadingPlan {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.title && !self.subtitle && !self.outcome
    }
}

#[derive(Clone, Debug, Default)]
struct PaneHeadings {
    accepted: AcceptedHeadings,
    last_prompt_key: Option<String>,
    subtitle_attempt_key: Option<String>,
    subtitle_accepted_key: Option<String>,
    subtitle_attempt_ms: Option<u64>,
    last_reply_key: Option<String>,
    last_screen_key: Option<String>,
    prompts_seen: usize,
    title_at_prompt: usize,
    title_attempt_ms: Option<u64>,
    activity_attempt_ms: Option<u64>,
}

/// Pure transition policy for heading cadence and cached accepted values.
///
/// The async adapter owns the single global generation lane. It calls these methods
/// only after acquiring that lane, so a prompt observed while busy remains new on the
/// next pass, preserving the established scheduling behavior.
#[derive(Clone, Debug, Default)]
pub struct HeadingStore {
    panes: HashMap<String, PaneHeadings>,
}

impl HeadingStore {
    /// Decide which transcript jobs are due and atomically record their attempts.
    #[must_use]
    pub fn plan_transcript(
        &mut self,
        pane_id: &str,
        digest: &TranscriptDigest,
        now_ms: u64,
    ) -> TranscriptHeadingPlan {
        let pane = self.panes.entry(pane_id.to_owned()).or_default();
        let is_new_prompt = pane.last_prompt_key != digest.last_prompt_key;
        if is_new_prompt {
            pane.prompts_seen = pane.prompts_seen.saturating_add(1);
            pane.last_prompt_key = digest.last_prompt_key.clone();
            pane.accepted.outcome = None;
        }

        let missing_title = pane.accepted.title.is_none();
        let cooldown_elapsed = pane
            .title_attempt_ms
            .is_none_or(|attempt| now_ms.saturating_sub(attempt) >= FAILURE_COOLDOWN_MS);
        let title_due = if missing_title {
            cooldown_elapsed
        } else {
            pane.prompts_seen.saturating_sub(pane.title_at_prompt)
                >= title_interval(pane.prompts_seen)
        };
        let subtitle_was_accepted = pane.subtitle_accepted_key == digest.last_prompt_key;
        let subtitle_cooldown_elapsed = pane
            .subtitle_attempt_ms
            .is_none_or(|attempt| now_ms.saturating_sub(attempt) >= FAILURE_COOLDOWN_MS);
        let subtitle_due = pane.subtitle_attempt_key != digest.last_prompt_key
            || (!subtitle_was_accepted && subtitle_cooldown_elapsed);
        let outcome_due =
            !digest.last_reply.is_empty() && pane.last_reply_key != digest.last_reply_key;

        if title_due {
            pane.title_at_prompt = pane.prompts_seen;
            pane.title_attempt_ms = Some(now_ms);
        }
        TranscriptHeadingPlan {
            prompts_seen: pane.prompts_seen,
            title: title_due,
            subtitle: subtitle_due,
            outcome: outcome_due,
            prompt_key: digest.last_prompt_key.clone(),
            reply_key: digest.last_reply_key.clone(),
            attempted_at_ms: now_ms,
        }
    }

    /// Merge one completed transcript bundle. `None` retains any prior accepted value.
    pub fn complete_transcript(
        &mut self,
        pane_id: &str,
        plan: &TranscriptHeadingPlan,
        title: Option<String>,
        subtitle: Option<String>,
        outcome: Option<String>,
    ) {
        self.complete_title(pane_id, plan, title);
        self.complete_subtitle(pane_id, plan, subtitle);
        self.complete_outcome(pane_id, plan, outcome);
    }

    /// Record only a completed title attempt from a larger transcript plan.
    ///
    /// Keeping job completion granular lets an async worker preserve a finished title
    /// when its observation becomes stale without also suppressing a subtitle or outcome
    /// that never ran.
    pub fn complete_title(
        &mut self,
        pane_id: &str,
        plan: &TranscriptHeadingPlan,
        title: Option<String>,
    ) {
        if plan.title {
            if let Some(title) = title {
                self.panes
                    .entry(pane_id.to_owned())
                    .or_default()
                    .accepted
                    .title = Some(title);
            }
        }
    }

    /// Record one completed subtitle attempt, including a rejected/empty result.
    pub fn complete_subtitle(
        &mut self,
        pane_id: &str,
        plan: &TranscriptHeadingPlan,
        subtitle: Option<String>,
    ) {
        if !plan.subtitle {
            return;
        }
        let pane = self.panes.entry(pane_id.to_owned()).or_default();
        pane.subtitle_attempt_key = plan.prompt_key.clone();
        pane.subtitle_attempt_ms = Some(plan.attempted_at_ms);
        if let Some(subtitle) = subtitle {
            pane.accepted.subtitle = Some(subtitle);
            pane.subtitle_accepted_key = plan.prompt_key.clone();
        }
    }

    /// Record one completed outcome attempt, including a rejected/empty result.
    pub fn complete_outcome(
        &mut self,
        pane_id: &str,
        plan: &TranscriptHeadingPlan,
        outcome: Option<String>,
    ) {
        if !plan.outcome {
            return;
        }
        let pane = self.panes.entry(pane_id.to_owned()).or_default();
        pane.last_reply_key = plan.reply_key.clone();
        if let Some(outcome) = outcome {
            pane.accepted.outcome = Some(outcome);
        }
    }

    /// Admit an activity job at most once per changed screen and cooldown interval.
    #[must_use]
    pub fn plan_activity(&mut self, pane_id: &str, screen_key: &str, now_ms: u64) -> bool {
        let pane = self.panes.entry(pane_id.to_owned()).or_default();
        if pane.last_screen_key.as_deref() == Some(screen_key) {
            return false;
        }
        if pane
            .activity_attempt_ms
            .is_some_and(|attempt| now_ms.saturating_sub(attempt) < FAILURE_COOLDOWN_MS)
        {
            return false;
        }
        pane.last_screen_key = Some(screen_key.to_owned());
        pane.activity_attempt_ms = Some(now_ms);
        true
    }

    /// Retain the previous accepted activity label when a provider attempt fails.
    pub fn complete_activity(&mut self, pane_id: &str, activity: Option<String>) {
        if let Some(activity) = activity {
            self.panes
                .entry(pane_id.to_owned())
                .or_default()
                .accepted
                .activity = Some(activity);
        }
    }

    #[must_use]
    pub fn accepted(&self, pane_id: &str) -> Option<&AcceptedHeadings> {
        self.panes.get(pane_id).map(|pane| &pane.accepted)
    }

    pub fn retain(&mut self, live_panes: &HashSet<String>) {
        self.panes.retain(|pane_id, _| live_panes.contains(pane_id));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.panes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }
}

/// The adaptive title cadence used by the heading scheduler.
#[must_use]
pub const fn title_interval(prompts_seen: usize) -> usize {
    if prompts_seen < 4 {
        1
    } else if prompts_seen < 12 {
        3
    } else {
        8
    }
}

#[must_use]
pub fn title_job(digest: &TranscriptDigest) -> HeadingJob {
    let opening = take_graphemes(&digest.opening, 400);
    let requests = take_graphemes(&digest.requests, 1200);
    HeadingJob {
        kind: HeadingKind::Title,
        prompt: format!(
            "A user opened a working session with one request, then continued.\n\n\
             FIRST REQUEST:\n{opening}\n\n\
             LATER REQUESTS:\n{requests}\n\n\
             Decide which of these two is true:\n\
             (a) The later requests are follow-ups that serve the FIRST request — the session\n\
                 is still about the thing it opened with.\n\
             (b) The first request was answered or set aside early, and most later requests\n\
                 are about a DIFFERENT problem the session then settled on.\n\n\
             If (a), name the first request's goal. If (b), name that different problem.\n\
             Do not name whatever the most recent single request happens to be.\n\n\
             If there are only one or two later requests, there is not yet enough evidence\n\
             that the session has moved on — name the first request's goal.\n\n\
             Name the subject the user is working on, not the assistant's response to it.\n\
             Never describe replying, greeting, acknowledging, helping or answering.\n\n\
             Answer with only a 3 to 6 word imperative task name. No quotes. No trailing period.\n\n\
             NAME:"
        ),
    }
}

#[must_use]
pub fn subtitle_job(digest: &TranscriptDigest, title: Option<&str>) -> HeadingJob {
    let goal = title.unwrap_or("the session's larger goal");
    let context = if digest.last_reply.is_empty() {
        String::new()
    } else {
        format!(
            "\nWHAT THE AGENT SAID JUST BEFORE THIS REQUEST (context only — do not\n\
             describe the agent's reply itself, only the work it is about):\n{}\n",
            take_graphemes(&digest.last_reply, 900)
        )
    };
    let source = if digest.last_prompt.is_empty() {
        format!("RECENT TURNS:\n{}", take_graphemes(&digest.recent, 1200))
    } else {
        format!(
            "LATEST REQUEST:\n{}",
            take_graphemes(&digest.last_prompt, 700)
        )
    };
    HeadingJob {
        kind: HeadingKind::Subtitle,
        prompt: format!(
            "A working session is moving through a series of requests toward one larger goal.\n\
             Name the single concrete step now underway in service of that goal.\n\n\
             Two examples of the right size and shape:\n\n\
               GOAL: Migrate the photo library to Postgres\n\
               REQUEST: \"the thumbnails are all coming out rotated 90 degrees\"\n\
               STEP: Fix rotated thumbnails in the importer\n\n\
               GOAL: Tune the espresso grinder settings\n\
               CONTEXT: the agent had just asked for a shot pulled at a finer grind\n\
               REQUEST: \"ok pulled it\"\n\
               STEP: Read the shot time at the finer grind\n\n\
             Notice the second one: a short answer names no work by itself, so the step comes\n\
             from what the answer sets in motion.\n\n\
             THE LARGER GOAL: {goal}\n{context}\n{source}\n\n\
             One action on one thing, at most 8 words. Never restate the goal. Write\n\
             impersonally, never \"you\" or \"I\". Name the subject of the work, not the\n\
             assistant's response to it. No quotes, no trailing period.\n\n\
             STEP:"
        ),
    }
}

#[must_use]
pub fn outcome_job(digest: &TranscriptDigest) -> HeadingJob {
    HeadingJob {
        kind: HeadingKind::Outcome,
        prompt: format!(
            "Below is the latest reply from a coding agent to its user.\n\
             Summarise it in at most 3 short sentences: what was done, what happens next,\n\
             and any decision or answer it is waiting on. Lead with whichever matters most.\n\
             Write impersonally — never \"you\" or \"I\". Be concrete and specific.\n\
             No quotes. No preamble.\n\n\
             LATEST REPLY:\n{}\n\n\
             STATE:",
            take_graphemes(&digest.last_reply, 1400)
        ),
    }
}

#[must_use]
pub fn activity_job(screen: &str) -> HeadingJob {
    HeadingJob {
        kind: HeadingKind::Activity,
        prompt: format!(
            "You are labelling a coding agent's terminal for a status dashboard.\n\
             Reply with ONE clause, max 8 words, describing what the agent is doing right now.\n\
             No punctuation at the end. No quotes. Present participle.\n\n\
             TERMINAL:\n{}\n\n\
             LABEL:",
            take_last_graphemes(screen, 2000)
        ),
    }
}

/// Apply the output cleanup shared by all provider adapters.
pub fn tidy(raw: &str, kind: HeadingKind) -> Result<String, HeadingRejection> {
    let mut text = raw.trim().to_owned();
    // Small instruction-following models sometimes repeat the final subtitle cue.
    // This is deliberately a leading, exact cue only: a real heading may contain
    // the word "step" elsewhere, and the other heading prompts do not use it.
    if kind == HeadingKind::Subtitle {
        if let Some(without_marker) = strip_leading_ascii_case_insensitive(&text, "STEP:") {
            text = without_marker.trim_start().to_owned();
        }
    }
    for marker in ["LABEL:", "NAME:", "FOCUS:", "STATE:"] {
        if let Some(index) = find_ascii_case_insensitive(&text, marker) {
            text = text[index + marker.len()..].trim_start().to_owned();
        }
    }
    text = if kind.multiline() {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        text.lines().next().unwrap_or_default().to_owned()
    };
    text = text
        .trim_matches(|character| matches!(character, '"' | '\'' | '`' | '“' | '”' | ' '))
        .to_owned();
    if !kind.multiline() {
        text = text
            .trim_matches(|character| matches!(character, '.' | ' '))
            .to_owned();
    }

    let count = text.graphemes(true).count();
    if count <= 3 {
        return Err(HeadingRejection::EmptyOrTooShort);
    }
    if count > kind.max_characters() {
        if kind.multiline() {
            if let Some(shorter) = last_complete_sentence(&text, kind.max_characters()) {
                return Ok(shorter);
            }
        }
        return Err(HeadingRejection::TooLong);
    }
    Ok(text)
}

fn strip_leading_ascii_case_insensitive<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.get(..marker.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(marker))
        .map(|_| &text[marker.len()..])
}

/// Apply job-specific semantic rejection after syntactic cleanup.
pub fn validate(
    candidate: String,
    kind: HeadingKind,
    title: Option<&str>,
) -> Result<String, HeadingRejection> {
    if matches!(kind, HeadingKind::Title | HeadingKind::Subtitle) && !names_work(&candidate) {
        return Err(HeadingRejection::AssistantAction);
    }
    if kind == HeadingKind::Subtitle && !distinct(&candidate, title) {
        return Err(HeadingRejection::TooCloseToTitle);
    }
    if kind == HeadingKind::Outcome && candidate.graphemes(true).count() <= 12 {
        return Err(HeadingRejection::EmptyOrTooShort);
    }
    Ok(candidate)
}

#[must_use]
pub fn names_work(heading: &str) -> bool {
    let first = heading
        .split(|character: char| !character.is_alphanumeric())
        .find(|word| !word.is_empty())
        .unwrap_or_default()
        .to_lowercase();
    !matches!(
        first.as_str(),
        "greet"
            | "greeting"
            | "greets"
            | "acknowledge"
            | "acknowledging"
            | "acknowledges"
            | "respond"
            | "responding"
            | "reply"
            | "replying"
            | "answer"
            | "answering"
            | "assist"
            | "assisting"
            | "help"
            | "helping"
            | "welcome"
            | "welcoming"
            | "thank"
            | "thanking"
            | "apologize"
            | "apologise"
            | "chat"
            | "chatting"
            | "converse"
            | "conversing"
            | "engage"
            | "engaging"
            | "introduce"
            | "introducing"
    )
}

#[must_use]
pub fn distinct(subtitle: &str, title: Option<&str>) -> bool {
    let Some(title) = title else {
        return true;
    };
    let title_words = significant_words(title);
    if title_words.is_empty() {
        return true;
    }
    let subtitle_words = significant_words(subtitle);
    let shared = title_words.intersection(&subtitle_words).count();
    shared as f64 / (title_words.len() as f64) < 0.6
}

fn significant_words(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.chars().count() > 3)
        .map(str::to_lowercase)
        .collect()
}

fn last_complete_sentence(text: &str, max_characters: usize) -> Option<String> {
    let head = take_graphemes(text, max_characters);
    let end = head.rfind(['.', '!', '?'])?;
    let candidate = head[..=end].trim();
    (candidate.graphemes(true).count() > 20).then(|| candidate.to_owned())
}

fn find_ascii_case_insensitive(text: &str, needle: &str) -> Option<usize> {
    text.as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn take_graphemes(text: &str, limit: usize) -> String {
    text.graphemes(true).take(limit).collect()
}

fn take_last_graphemes(text: &str, limit: usize) -> String {
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    graphemes[graphemes.len().saturating_sub(limit)..].concat()
}

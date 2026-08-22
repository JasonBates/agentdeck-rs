use std::collections::HashSet;

use agentdeck_core::headings::{
    HeadingKind, HeadingRejection, HeadingStore, activity_job, distinct, names_work, outcome_job,
    subtitle_job, tidy, title_interval, title_job, validate,
};
use agentdeck_core::transcript::TranscriptDigest;

fn digest() -> TranscriptDigest {
    TranscriptDigest {
        opening: "Build the portable AgentDeck service".to_owned(),
        requests: "- Build it\n- Add portable providers".to_owned(),
        recent: "USER: Add portable providers\nASSISTANT: Choose explicit backends".to_owned(),
        last_prompt: "Add portable providers".to_owned(),
        last_prompt_key: Some("prompt-key".to_owned()),
        last_reply: "The service core is complete. Next, wire the provider adapter.".to_owned(),
        last_reply_key: Some("reply-key".to_owned()),
        written_at: 1,
    }
}

#[test]
fn title_cadence_has_frozen_boundaries() {
    assert_eq!(title_interval(0), 1);
    assert_eq!(title_interval(3), 1);
    assert_eq!(title_interval(4), 3);
    assert_eq!(title_interval(11), 3);
    assert_eq!(title_interval(12), 8);
    assert_eq!(title_interval(99), 8);
}

#[test]
fn jobs_have_exact_kinds_limits_and_bounded_private_inputs() {
    let mut input = digest();
    input.opening = "o".repeat(401);
    input.requests = "r".repeat(1201);
    input.last_prompt = "p".repeat(701);
    input.last_reply = "a".repeat(1401);
    let title = title_job(&input);
    assert_eq!(title.kind, HeadingKind::Title);
    assert!(title.prompt.contains(&"o".repeat(400)));
    assert!(!title.prompt.contains(&"o".repeat(401)));
    assert_eq!(title.kind.max_tokens(), 40);

    let subtitle = subtitle_job(&input, Some("Portable AgentDeck service"));
    assert!(
        subtitle
            .prompt
            .contains("THE LARGER GOAL: Portable AgentDeck service")
    );
    assert!(subtitle.prompt.contains(&"p".repeat(700)));
    assert!(!subtitle.prompt.contains(&"p".repeat(701)));
    assert_eq!(subtitle.kind.max_tokens(), 32);

    let outcome = outcome_job(&input);
    assert!(outcome.prompt.contains(&"a".repeat(1400)));
    assert!(!outcome.prompt.contains(&"a".repeat(1401)));
    assert_eq!(outcome.kind.max_tokens(), 110);

    let activity = activity_job(&format!("{}TAIL", "x".repeat(2100)));
    assert!(!activity.prompt.contains(&"x".repeat(2100)));
    assert!(activity.prompt.contains("TAIL"));
    assert_eq!(activity.kind.max_tokens(), 24);
}

#[test]
fn subtitle_uses_preceding_reply_for_thin_prompts() {
    let mut input = digest();
    input.last_prompt = "ok run that".to_owned();
    let job = subtitle_job(&input, Some("Build portable service"));
    assert!(job.prompt.contains("WHAT THE AGENT SAID JUST BEFORE"));
    assert!(job.prompt.contains("Next, wire the provider adapter"));
    assert!(job.prompt.contains("LATEST REQUEST:\nok run that"));
}

#[test]
fn tidy_strips_markers_quotes_and_single_line_noise() {
    assert_eq!(
        tidy(
            "  preamble NAME: “Build portable bridge.”\nignored",
            HeadingKind::Title
        ),
        Ok("Build portable bridge".to_owned())
    );
    assert_eq!(
        tidy(
            "STATE: First sentence.\nSecond sentence.",
            HeadingKind::Outcome
        ),
        Ok("First sentence. Second sentence.".to_owned())
    );
    assert_eq!(
        tidy("  sTeP: Keep this cue intact", HeadingKind::Subtitle),
        Ok("Keep this cue intact".to_owned())
    );
}

#[test]
fn tidy_strips_only_a_leading_subtitle_step_cue() {
    assert_eq!(
        tidy("Plan the next step: review markers", HeadingKind::Subtitle),
        Ok("Plan the next step: review markers".to_owned())
    );
    assert_eq!(
        tidy("Complete the migration STEP:", HeadingKind::Subtitle),
        Ok("Complete the migration STEP:".to_owned())
    );
    assert_eq!(
        tidy("STEP: Build portable bridge", HeadingKind::Title),
        Ok("STEP: Build portable bridge".to_owned())
    );
    assert_eq!(
        tidy("STEP: Summarise the change", HeadingKind::Outcome),
        Ok("STEP: Summarise the change".to_owned())
    );
}

#[test]
fn tidy_rejects_short_and_overlong_single_line_outputs() {
    assert_eq!(
        tidy("ok", HeadingKind::Title),
        Err(HeadingRejection::EmptyOrTooShort)
    );
    assert_eq!(
        tidy(&"x".repeat(91), HeadingKind::Title),
        Err(HeadingRejection::TooLong)
    );
    assert_eq!(
        tidy(&"x".repeat(131), HeadingKind::Subtitle),
        Err(HeadingRejection::TooLong)
    );
}

#[test]
fn multiline_overrun_trims_only_to_a_useful_complete_sentence() {
    let first = format!("{}.", "a".repeat(100));
    let second = format!("{}.", "b".repeat(200));
    let overflow = format!("{first} {second} {}", "c".repeat(200));
    assert_eq!(
        tidy(&overflow, HeadingKind::Outcome),
        Ok(format!("{first} {second}"))
    );
    assert_eq!(
        tidy(&"x".repeat(381), HeadingKind::Outcome),
        Err(HeadingRejection::TooLong)
    );
}

#[test]
fn headings_that_describe_the_assistant_are_rejected() {
    for bad in [
        "Greet user and start conversation",
        "Acknowledge user update",
        "Answer deployment question",
        "Helping with portable service",
    ] {
        assert!(!names_work(bad), "accepted {bad:?}");
        assert_eq!(
            validate(bad.to_owned(), HeadingKind::Title, None),
            Err(HeadingRejection::AssistantAction)
        );
    }
    for good in [
        "Discuss chapter structure",
        "Confirm service protocol",
        "Build the portable service",
    ] {
        assert!(names_work(good), "rejected {good:?}");
    }
}

#[test]
fn subtitle_overlap_gate_matches_contract_threshold() {
    assert!(!distinct(
        "Develop portable service deployment now",
        Some("Develop portable service deployment")
    ));
    assert!(distinct(
        "Test provider fallback behavior",
        Some("Develop portable service deployment")
    ));
    assert!(distinct("Anything useful", None));
    assert_eq!(
        validate(
            "Develop portable service deployment now".to_owned(),
            HeadingKind::Subtitle,
            Some("Develop portable service deployment")
        ),
        Err(HeadingRejection::TooCloseToTitle)
    );
}

#[test]
fn outcome_requires_more_than_twelve_graphemes() {
    assert_eq!(
        validate("Work is done".to_owned(), HeadingKind::Outcome, None),
        Err(HeadingRejection::EmptyOrTooShort)
    );
    assert_eq!(
        validate(
            "Bridge work is complete".to_owned(),
            HeadingKind::Outcome,
            None
        ),
        Ok("Bridge work is complete".to_owned())
    );
}

#[test]
fn character_limits_count_graphemes() {
    let grapheme = "e\u{301}";
    assert_eq!(
        tidy(&grapheme.repeat(90), HeadingKind::Title),
        Ok(grapheme.repeat(90))
    );
    assert_eq!(
        tidy(&grapheme.repeat(91), HeadingKind::Title),
        Err(HeadingRejection::TooLong)
    );
}

#[test]
fn sanitized_whole_prompt_goldens_freeze_provider_inputs() {
    let input = digest();
    assert_eq!(
        title_job(&input).prompt,
        include_str!("fixtures/headings/title-prompt.txt").trim_end_matches('\n')
    );
    assert_eq!(
        subtitle_job(&input, Some("Build portable AgentDeck service")).prompt,
        include_str!("fixtures/headings/subtitle-prompt.txt").trim_end_matches('\n')
    );
    assert_eq!(
        outcome_job(&input).prompt,
        include_str!("fixtures/headings/outcome-prompt.txt").trim_end_matches('\n')
    );
    assert_eq!(
        activity_job("Run heading fixture tests").prompt,
        include_str!("fixtures/headings/activity-prompt.txt").trim_end_matches('\n')
    );
}

#[test]
fn transcript_schedule_clears_outcome_and_follows_adaptive_title_cadence() {
    let mut store = HeadingStore::default();
    let mut input = digest();

    let first = store.plan_transcript("p", &input, 0);
    assert_eq!(first.prompts_seen, 1);
    assert!(first.title && first.subtitle && first.outcome);
    store.complete_transcript(
        "p",
        &first,
        Some("Build portable service".to_owned()),
        Some("Implement transcript adapter".to_owned()),
        Some("Transcript adapter is complete".to_owned()),
    );
    assert_eq!(
        store
            .accepted("p")
            .and_then(|value| value.outcome.as_deref()),
        Some("Transcript adapter is complete")
    );

    let same = store.plan_transcript("p", &input, 1);
    assert!(same.is_empty());

    input.last_prompt_key = Some("prompt-2".to_owned());
    input.last_reply.clear();
    input.last_reply_key = None;
    let second = store.plan_transcript("p", &input, 2);
    assert_eq!(second.prompts_seen, 2);
    assert!(second.title && second.subtitle && !second.outcome);
    assert_eq!(
        store
            .accepted("p")
            .and_then(|value| value.outcome.as_deref()),
        None
    );

    store.complete_transcript(
        "p",
        &second,
        Some("Build portable service".to_owned()),
        None,
        None,
    );
    input.last_prompt_key = Some("prompt-3".to_owned());
    let third = store.plan_transcript("p", &input, 3);
    assert!(third.title);
    store.complete_transcript(
        "p",
        &third,
        Some("Build portable service".to_owned()),
        None,
        None,
    );

    input.last_prompt_key = Some("prompt-4".to_owned());
    let fourth = store.plan_transcript("p", &input, 4);
    assert!(!fourth.title, "4 prompts switches to every-three cadence");
    input.last_prompt_key = Some("prompt-5".to_owned());
    assert!(!store.plan_transcript("p", &input, 5).title);
    input.last_prompt_key = Some("prompt-6".to_owned());
    assert!(store.plan_transcript("p", &input, 6).title);
}

#[test]
fn failed_title_and_activity_attempts_cool_down_and_retain_previous_values() {
    let mut store = HeadingStore::default();
    let mut input = digest();
    let first = store.plan_transcript("p", &input, 100);
    store.complete_transcript(
        "p",
        &first,
        None,
        Some("Accepted subtitle".to_owned()),
        Some("Accepted outcome".to_owned()),
    );

    let cooling = store.plan_transcript("p", &input, 20_099);
    assert!(!cooling.title);
    let retry = store.plan_transcript("p", &input, 20_100);
    assert!(retry.title);
    store.complete_transcript("p", &retry, Some("Accepted title".to_owned()), None, None);

    input.last_prompt_key = Some("next-prompt".to_owned());
    let next = store.plan_transcript("p", &input, 20_101);
    store.complete_transcript("p", &next, None, None, None);
    let accepted = store.accepted("p").unwrap_or_else(|| panic!("pane exists"));
    assert_eq!(accepted.title.as_deref(), Some("Accepted title"));
    assert_eq!(accepted.subtitle.as_deref(), Some("Accepted subtitle"));
    assert_eq!(accepted.outcome, None, "new prompt clears stale outcome");

    assert!(store.plan_activity("p", "screen-1", 30_000));
    store.complete_activity("p", Some("Running tests".to_owned()));
    assert!(!store.plan_activity("p", "screen-1", 60_000));
    assert!(!store.plan_activity("p", "screen-2", 49_999));
    assert!(store.plan_activity("p", "screen-2", 50_000));
    store.complete_activity("p", None);
    assert_eq!(
        store
            .accepted("p")
            .and_then(|value| value.activity.as_deref()),
        Some("Running tests")
    );
}

#[test]
fn partial_transcript_completion_resumes_only_unfinished_jobs() {
    let mut store = HeadingStore::default();
    let input = digest();
    let first = store.plan_transcript("p", &input, 100);
    assert!(first.title && first.subtitle && first.outcome);

    store.complete_title("p", &first, Some("Build portable service".to_owned()));
    let resumed = store.plan_transcript("p", &input, 101);
    assert!(!resumed.title, "the completed title is retained");
    assert!(resumed.subtitle, "the unattempted subtitle remains due");
    assert!(resumed.outcome, "the unattempted outcome remains due");

    store.complete_subtitle("p", &resumed, None);
    store.complete_outcome("p", &resumed, Some("Service is ready".to_owned()));
    let complete = store.plan_transcript("p", &input, 102);
    assert!(
        complete.is_empty(),
        "completed or rejected jobs do not storm"
    );
    let subtitle_retry = store.plan_transcript("p", &input, 20_101);
    assert!(!subtitle_retry.title && subtitle_retry.subtitle && !subtitle_retry.outcome);
    store.complete_subtitle(
        "p",
        &subtitle_retry,
        Some("Retry rejected subtitle".to_owned()),
    );
    assert!(store.plan_transcript("p", &input, 20_102).is_empty());
    let accepted = store.accepted("p").unwrap_or_else(|| panic!("pane exists"));
    assert_eq!(accepted.title.as_deref(), Some("Build portable service"));
    assert_eq!(
        accepted.subtitle.as_deref(),
        Some("Retry rejected subtitle")
    );
    assert_eq!(accepted.outcome.as_deref(), Some("Service is ready"));
}

#[test]
fn heading_store_prunes_dead_panes() {
    let mut store = HeadingStore::default();
    let _ = store.plan_activity("live", "one", 0);
    let _ = store.plan_activity("dead", "two", 0);
    assert_eq!(store.len(), 2);
    store.retain(&HashSet::from(["live".to_owned()]));
    assert_eq!(store.len(), 1);
    store.retain(&HashSet::new());
    assert!(store.is_empty());
}

use std::collections::BTreeMap;

use agentdeck_core::tab_titles::{TabTitleObservation, TabTitleOwnership};

fn tab(id: &str, label: &str, title: Option<&str>, agent_count: usize) -> TabTitleObservation {
    TabTitleObservation {
        tab_id: id.to_owned(),
        current_label: label.to_owned(),
        model_title: title.map(ToOwned::to_owned),
        agent_count,
    }
}

#[test]
fn claims_default_numbered_tab_and_follows_improvements() {
    let mut state = TabTitleOwnership::default();
    let first = state.plan(&[tab("w1:t1", "1", Some("Connect AgentDeck titles"), 1)]);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].expected_current_label, "1");
    state.rename_succeeded(&first[0]);

    let next = state.plan(&[tab(
        "w1:t1",
        "Connect AgentDeck titles",
        Some("Sync AgentDeck and Herdr titles"),
        1,
    )]);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].title, "Sync AgentDeck and Herdr titles");
    state.rename_succeeded(&next[0]);
    assert_eq!(
        state.managed().get("w1:t1").map(String::as_str),
        Some("Sync AgentDeck and Herdr titles")
    );
}

#[test]
fn manual_rename_releases_ownership_and_is_not_reclaimed() {
    let mut state = TabTitleOwnership::from_managed(BTreeMap::from([(
        "w1:t1".to_owned(),
        "Generated title".to_owned(),
    )]));
    let renames = state.plan(&[tab(
        "w1:t1",
        "Manual title",
        Some("Improved generated title"),
        1,
    )]);
    assert!(renames.is_empty());
    assert!(!state.managed().contains_key("w1:t1"));
    assert!(state.is_dirty());

    assert!(
        state
            .plan(&[tab(
                "w1:t1",
                "Manual title",
                Some("Another generated title"),
                1,
            )])
            .is_empty()
    );
}

#[test]
fn exact_existing_generated_title_recovers_lost_state() {
    let mut state = TabTitleOwnership::default();
    let renames = state.plan(&[tab("w1:t1", "Initial title", Some("Initial title"), 1)]);
    assert!(renames.is_empty());
    assert_eq!(
        state.managed().get("w1:t1").map(String::as_str),
        Some("Initial title")
    );
    let improved = state.plan(&[tab("w1:t1", "Initial title", Some("Settled title"), 1)]);
    assert_eq!(improved.len(), 1);
}

#[test]
fn named_multi_agent_and_invalid_titles_are_not_claimed() {
    let mut state = TabTitleOwnership::default();
    let renames = state.plan(&[
        tab("named", "Manual", Some("Generated"), 1),
        tab("many", "", Some("Ambiguous"), 2),
        tab("empty", "", Some("  "), 1),
        tab("dash", "", Some("—"), 1),
    ]);
    assert!(renames.is_empty());
    assert!(state.managed().is_empty());
}

#[test]
fn failed_rename_retries_without_taking_ownership() {
    let mut state = TabTitleOwnership::default();
    let observation = tab("w1:t1", "", Some("Generated title"), 1);
    let first = state.plan(std::slice::from_ref(&observation));
    assert_eq!(first.len(), 1);
    state.rename_failed(&first[0]);
    assert!(!state.managed().contains_key("w1:t1"));

    let retry = state.plan(&[observation]);
    assert_eq!(retry, first);
}

#[test]
fn dead_tabs_are_pruned_and_persistence_dirty_bit_is_explicit() {
    let mut state = TabTitleOwnership::from_managed(BTreeMap::from([
        ("live".to_owned(), "A".to_owned()),
        ("dead".to_owned(), "B".to_owned()),
    ]));
    let _ = state.plan(&[tab("live", "A", Some("A"), 1)]);
    assert_eq!(state.managed().len(), 1);
    assert!(state.is_dirty());
    state.mark_persisted();
    assert!(!state.is_dirty());
}

#[test]
fn manual_label_releases_ownership_even_if_model_is_unavailable_or_ambiguous() {
    let mut state = TabTitleOwnership::from_managed(BTreeMap::from([(
        "tab".to_owned(),
        "Generated".to_owned(),
    )]));
    assert!(
        state
            .plan(&[tab("tab", "Manual for now", None, 1)])
            .is_empty()
    );
    assert!(!state.managed().contains_key("tab"));

    let mut state = TabTitleOwnership::from_managed(BTreeMap::from([(
        "tab".to_owned(),
        "Generated".to_owned(),
    )]));
    assert!(
        state
            .plan(&[tab("tab", "Manual for now", Some("Other"), 2)])
            .is_empty()
    );
    assert!(!state.managed().contains_key("tab"));
}

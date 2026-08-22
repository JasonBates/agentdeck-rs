//! Deterministic Herdr-derived card-title policy for model-off operation.

use caseless::Caseless;

/// Removes a leading spinner/marker run and normalizes an empty title to the
/// compatibility placeholder. This uses Unicode character classification just as the
/// Unicode letters such as `π` remain meaningful during normalization.
#[must_use]
pub fn clean_title(raw: Option<&str>) -> String {
    let trimmed_prefix = raw
        .unwrap_or_default()
        .trim_start_matches(|character: char| !character.is_alphanumeric());
    let title = trimmed_prefix.trim();
    if title.is_empty() {
        "—".to_owned()
    } else {
        title.to_owned()
    }
}

/// Whether a terminal title says no more than the directory/project chip does.
#[must_use]
pub fn is_generic_title(title: &str, project: &str) -> bool {
    if title.is_empty() || title == "—" {
        return true;
    }
    if case_insensitive_equal(title, project) {
        return true;
    }
    title
        .rsplit('-')
        .next()
        .is_some_and(|tail| case_insensitive_equal(tail.trim(), project))
}

fn case_insensitive_equal(left: &str, right: &str) -> bool {
    // Full Unicode Default Case Folding handles cases `to_lowercase` does not,
    // including German ß and Greek final sigma. Canonical matching adds the NFD
    // normalization required for equivalent composed/decomposed accents without
    // compatibility-folding unrelated title characters.
    left.chars().canonical_caseless_match(right.chars())
}

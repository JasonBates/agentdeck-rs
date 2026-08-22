//! Pure ownership policy for synchronizing accepted card titles to Herdr tabs.
//!
//! Persistence and Herdr mutations are two-phase adapter concerns. A failed rename is
//! never recorded as ownership, so the same observation is naturally retried.

use std::collections::{BTreeMap, HashSet};

pub const TAB_TITLE_STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabTitleObservation {
    pub tab_id: String,
    pub current_label: String,
    pub model_title: Option<String>,
    pub agent_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabRename {
    pub tab_id: String,
    pub expected_current_label: String,
    pub title: String,
}

/// Version-one decoded state. Adapters reject other versions before constructing it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TabTitleOwnership {
    managed: BTreeMap<String, String>,
    dirty: bool,
}

impl TabTitleOwnership {
    #[must_use]
    pub fn from_managed(managed: BTreeMap<String, String>) -> Self {
        Self {
            managed,
            dirty: false,
        }
    }

    #[must_use]
    pub fn managed(&self) -> &BTreeMap<String, String> {
        &self.managed
    }

    /// Prune/release ownership and return safe compare-before-set rename intents.
    #[must_use]
    pub fn plan(&mut self, observations: &[TabTitleObservation]) -> Vec<TabRename> {
        let live_tabs = observations
            .iter()
            .map(|observation| observation.tab_id.clone())
            .collect::<Vec<_>>();
        self.plan_with_live_tabs(observations, Some(&live_tabs))
    }

    /// Plan from a bounded observation sample while pruning only from a complete,
    /// independently supplied live-tab set. Passing `None` deliberately avoids
    /// pruning: a caller that had to cap or truncate its snapshot must not turn
    /// missing observations into ownership loss.
    #[must_use]
    pub fn plan_with_live_tabs(
        &mut self,
        observations: &[TabTitleObservation],
        live_tabs: Option<&[String]>,
    ) -> Vec<TabRename> {
        if let Some(live_tabs) = live_tabs {
            let live_tabs = live_tabs.iter().map(String::as_str).collect::<HashSet<_>>();
            let before = self.managed.len();
            self.managed
                .retain(|tab_id, _| live_tabs.contains(tab_id.as_str()));
            self.dirty |= self.managed.len() != before;
        }

        let mut renames = Vec::new();
        for observation in observations {
            // A label that no longer matches the last value we wrote is a manual
            // edit. Release it before considering model availability or pane
            // cardinality so an unavailable model cannot retain stale ownership.
            if let Some(last_written) = self.managed.get(&observation.tab_id) {
                if observation.current_label != *last_written {
                    self.managed.remove(&observation.tab_id);
                    self.dirty = true;
                    continue;
                }
            }
            if observation.agent_count != 1 {
                continue;
            }
            let Some(raw_title) = observation.model_title.as_deref() else {
                continue;
            };
            let title = raw_title.trim();
            if title.is_empty() || title == "—" {
                continue;
            }

            if let Some(last_written) = self.managed.get(&observation.tab_id) {
                if title == last_written {
                    continue;
                }
            } else {
                let default_label = observation.current_label.is_empty()
                    || observation
                        .current_label
                        .chars()
                        .all(|character| character.is_numeric());
                if !default_label && observation.current_label != title {
                    continue;
                }
                if observation.current_label == title {
                    self.managed
                        .insert(observation.tab_id.clone(), title.to_owned());
                    self.dirty = true;
                    continue;
                }
            }

            renames.push(TabRename {
                tab_id: observation.tab_id.clone(),
                expected_current_label: observation.current_label.clone(),
                title: title.to_owned(),
            });
        }
        renames
    }

    /// Record ownership only after a compare-before-set Herdr rename succeeds.
    pub fn rename_succeeded(&mut self, rename: &TabRename) {
        self.managed
            .insert(rename.tab_id.clone(), rename.title.clone());
        self.dirty = true;
    }

    /// A failed action deliberately changes nothing and is retried on the next plan.
    pub const fn rename_failed(&mut self, _rename: &TabRename) {}

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Persistence adapters call this only after an atomic durable write succeeds.
    pub fn mark_persisted(&mut self) {
        self.dirty = false;
    }
}

//! Reasoning effort: supported levels per upstream model and the level for
//! free accounts.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Known reasoning effort levels, lowest first. The order caps and maps
/// levels between models.
pub const EFFORT_LEVELS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Position of `level` in [`EFFORT_LEVELS`].
pub fn effort_rank(level: &str) -> Option<usize> {
    EFFORT_LEVELS.iter().position(|l| *l == level)
}

/// Reasoning controls of one upstream model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningModel {
    /// Levels the model accepts. Empty means it has no reasoning control:
    /// the parameter is removed before the request is forwarded.
    pub efforts: Vec<String>,
    /// Level applied when the request names none. Empty leaves the
    /// provider's default.
    pub default: String,
}

/// `reasoning_effort` handling.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reasoning {
    /// Highest level for free accounts (only trial credit, no subscription
    /// or paid credit). Higher requested levels are lowered to it, and
    /// requests to a configured reasoning model without a level get it.
    /// Empty disables the cap.
    pub free_effort: String,
    /// Per upstream model name (case-insensitive). Models not listed pass
    /// the client's value through unchanged (apart from the free cap).
    pub models: BTreeMap<String, ReasoningModel>,
}

impl Default for Reasoning {
    fn default() -> Self {
        Self {
            free_effort: "low".into(),
            models: BTreeMap::new(),
        }
    }
}

impl Reasoning {
    /// The controls configured for `model`, if any.
    pub fn model(&self, model: &str) -> Option<&ReasoningModel> {
        self.models
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(model))
            .map(|(_, m)| m)
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        let known = |v: &str| effort_rank(v).is_some();
        anyhow::ensure!(
            self.free_effort.is_empty() || known(&self.free_effort),
            "reasoning.free_effort must be one of {}",
            EFFORT_LEVELS.join(", ")
        );
        anyhow::ensure!(
            self.models.len() <= 500,
            "reasoning.models allows at most 500 entries"
        );
        for (name, model) in &self.models {
            for level in &model.efforts {
                anyhow::ensure!(
                    known(level),
                    "reasoning.models.{name:?}.efforts: unknown level {level:?} (use {})",
                    EFFORT_LEVELS.join(", ")
                );
            }
            anyhow::ensure!(
                model.default.is_empty() || model.efforts.contains(&model.default),
                "reasoning.models.{name:?}.default must be one of its efforts"
            );
        }
        Ok(())
    }
}

//! The public model catalog: which models customers see and what each public
//! name resolves to upstream.
use super::Config;
use crate::keys::Limits;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet};

/// Most catalog entries.
const MAX_MODELS: usize = 200;
/// Most aliases of one entry.
const MAX_ALIASES: usize = 10;
/// Longest public model name, in characters.
const MAX_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 1000;
const MAX_MODALITIES: usize = 8;
const MAX_MODALITY_CHARS: usize = 32;
/// Largest accepted `context_length`.
const MAX_CONTEXT_LENGTH: u64 = 1_000_000_000;

/// One public model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Model {
    /// Public model id, unique case-insensitively across ids and aliases.
    pub id: String,
    /// Model name sent upstream. Empty means `id`.
    pub upstream: String,
    /// Extra public names that resolve to this entry. They are not listed.
    pub aliases: Vec<String>,
    /// Disabled entries are hidden and refused.
    pub enabled: bool,
    /// Display name.
    pub name: String,
    pub description: String,
    /// Owner shown in the list. Empty uses `branding.name`.
    pub owned_by: String,
    /// Creation time shown in the list, in Unix seconds.
    pub created: u64,
    /// Context window in tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    /// Input modalities. Empty derives them from the upstream the entry
    /// serves, see [`Model::resolved`].
    pub input_modalities: Vec<String>,
    /// Output modalities. Empty derives them like `input_modalities`.
    pub output_modalities: Vec<String>,
    /// List the price, with the caller's markup applied.
    pub show_pricing: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            upstream: String::new(),
            aliases: Vec::new(),
            enabled: true,
            name: String::new(),
            description: String::new(),
            owned_by: String::new(),
            created: 0,
            context_length: None,
            input_modalities: Vec::new(),
            output_modalities: Vec::new(),
            show_pricing: false,
        }
    }
}

impl Model {
    /// The model name sent upstream.
    pub fn upstream_name(&self) -> &str {
        if self.upstream.is_empty() {
            &self.id
        } else {
            &self.upstream
        }
    }

    /// Whether `name` is this entry's id or one of its aliases.
    pub fn answers_to(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    }

    /// A minimal enabled entry for a configured route model. Its modalities
    /// are derived from the upstream it serves.
    fn route(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            ..Self::default()
        }
    }

    /// The capability, input and output modalities of this entry. Explicit
    /// modalities win. Empty ones come from the upstream route that serves
    /// the entry's upstream model, and stay empty when no route does.
    pub fn resolved(&self, config: &Config) -> Resolved {
        let capability = config.capability_of(self.upstream_name());
        let pick = |explicit: &[String], derived: Option<&[&str]>| -> Vec<String> {
            if !explicit.is_empty() {
                explicit.to_vec()
            } else {
                derived
                    .map(|d| d.iter().map(|m| (*m).to_owned()).collect())
                    .unwrap_or_default()
            }
        };
        Resolved {
            capability,
            input: pick(&self.input_modalities, capability.map(|c| c.input())),
            output: pick(&self.output_modalities, capability.map(|c| c.output())),
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        public_name("id", &self.id)?;
        if !self.upstream.is_empty() {
            public_name("upstream", &self.upstream)?;
        }
        anyhow::ensure!(
            self.aliases.len() <= MAX_ALIASES,
            "models.{}: at most {MAX_ALIASES} aliases",
            self.id
        );
        for alias in &self.aliases {
            public_name("alias", alias)?;
        }
        for (field, value, limit) in [
            ("name", &self.name, MAX_NAME_CHARS),
            ("owned_by", &self.owned_by, MAX_NAME_CHARS),
            ("description", &self.description, MAX_DESCRIPTION_CHARS),
        ] {
            anyhow::ensure!(
                value.chars().count() <= limit && !value.chars().any(char::is_control),
                "models.{}: {field} must be at most {limit} characters without control characters",
                self.id
            );
        }
        anyhow::ensure!(
            self.context_length
                .is_none_or(|n| (1..=MAX_CONTEXT_LENGTH).contains(&n)),
            "models.{}: context_length must be 1-{MAX_CONTEXT_LENGTH}",
            self.id
        );
        for (field, list) in [
            ("input_modalities", &self.input_modalities),
            ("output_modalities", &self.output_modalities),
        ] {
            anyhow::ensure!(
                list.len() <= MAX_MODALITIES
                    && list.iter().all(|m| {
                        (1..=MAX_MODALITY_CHARS).contains(&m.len())
                            && m.bytes()
                                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    }),
                "models.{}: {field} takes at most {MAX_MODALITIES} lowercase names such as \"text\"",
                self.id
            );
        }
        Ok(())
    }
}

/// What a model does, decided by the upstream route it is configured on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Chat,
    Speech,
    Transcription,
    Image,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Speech => "speech",
            Self::Transcription => "transcription",
            Self::Image => "image",
        }
    }

    fn input(self) -> &'static [&'static str] {
        match self {
            Self::Chat | Self::Speech | Self::Image => &["text"],
            Self::Transcription => &["audio"],
        }
    }

    fn output(self) -> &'static [&'static str] {
        match self {
            Self::Chat | Self::Transcription => &["text"],
            Self::Speech => &["audio"],
            Self::Image => &["image"],
        }
    }
}

/// A model's capability and modalities after derivation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub capability: Option<Capability>,
    pub input: Vec<String>,
    pub output: Vec<String>,
}

/// Checks one public or upstream model name.
fn public_name(field: &str, value: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        (1..=MAX_NAME_CHARS).contains(&value.chars().count())
            && !value.chars().any(char::is_control),
        "models {field} {value:?} must be 1-{MAX_NAME_CHARS} characters without control characters"
    );
    Ok(())
}

/// Checks the catalog: sizes, fields, and unique names.
pub(super) fn validate(models: &[Model]) -> anyhow::Result<()> {
    anyhow::ensure!(
        models.len() <= MAX_MODELS,
        "at most {MAX_MODELS} models are allowed"
    );
    let mut names = HashSet::new();
    for model in models {
        model.validate()?;
        for name in std::iter::once(&model.id).chain(&model.aliases) {
            anyhow::ensure!(
                names.insert(name.to_lowercase()),
                "model name {name:?} is used twice, ids and aliases must be unique"
            );
        }
    }
    Ok(())
}

impl Config {
    /// The catalog served to customers. Without configured entries, it holds
    /// one minimal entry per configured route model.
    pub fn catalog(&self) -> Cow<'_, [Model]> {
        if !self.models.is_empty() {
            return Cow::Borrowed(&self.models);
        }
        let mut routes: Vec<&str> = Vec::new();
        let configured = [&self.llm, &self.tts, &self.stt, &self.image]
            .into_iter()
            .flat_map(|u| std::iter::once(&u.model).chain(&u.fallbacks));
        for name in configured.filter(|m| !m.is_empty()) {
            if !routes.iter().any(|r| r.eq_ignore_ascii_case(name)) {
                routes.push(name);
            }
        }
        Cow::Owned(routes.into_iter().map(Model::route).collect())
    }

    /// The capability of the upstream model `upstream`: the first route
    /// (chat, speech, transcription, image) that lists it as its model or a
    /// fallback.
    pub fn capability_of(&self, upstream: &str) -> Option<Capability> {
        let routes = [
            (Capability::Chat, &self.llm),
            (Capability::Speech, &self.tts),
            (Capability::Transcription, &self.stt),
            (Capability::Image, &self.image),
        ];
        routes
            .into_iter()
            .find(|(_, u)| {
                std::iter::once(&u.model)
                    .chain(&u.fallbacks)
                    .any(|m| !m.is_empty() && m.eq_ignore_ascii_case(upstream))
            })
            .map(|(c, _)| c)
    }

    /// Whether a key with `limits` may use the upstream model `upstream`.
    ///
    /// With a catalog, the allowlist is checked against the id of the first
    /// entry that sends this upstream model. Otherwise the raw name is checked.
    pub fn upstream_allowed(&self, limits: &Limits, upstream: &str) -> bool {
        let id = self
            .models
            .iter()
            .find(|m| m.upstream_name().eq_ignore_ascii_case(upstream))
            .map_or(upstream, |m| m.id.as_str());
        limits.allows(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            ..Model::default()
        }
    }

    #[test]
    fn names_are_unique_case_insensitively_across_ids_and_aliases() {
        assert!(validate(&[model("a"), model("b")]).is_ok());
        assert!(validate(&[model("a"), model("A")]).is_err());
        let aliased = Model {
            aliases: vec!["Fast".into()],
            ..model("a")
        };
        assert!(validate(&[aliased.clone(), model("fast")]).is_err());
        let twice = Model {
            aliases: vec!["x".into(), "X".into()],
            ..model("a")
        };
        assert!(validate(&[twice]).is_err());
        assert!(validate(&[aliased]).is_ok());
    }

    #[test]
    fn fields_are_bounded() {
        for bad in [
            model(""),
            model(&"x".repeat(129)),
            model("a\nb"),
            Model {
                upstream: "bad\u{7}".into(),
                ..model("a")
            },
            Model {
                aliases: (0..11).map(|i| format!("alias{i}")).collect(),
                ..model("a")
            },
            Model {
                context_length: Some(0),
                ..model("a")
            },
            Model {
                input_modalities: vec!["text".into(); 9],
                ..model("a")
            },
            Model {
                output_modalities: vec!["Text".into()],
                ..model("a")
            },
        ] {
            assert!(validate(std::slice::from_ref(&bad)).is_err(), "{bad:?}");
        }
        let many: Vec<Model> = (0..=MAX_MODELS).map(|i| model(&format!("m{i}"))).collect();
        assert!(validate(&many).is_err());
        assert!(validate(&many[..MAX_MODELS]).is_ok());
    }

    #[test]
    fn upstream_defaults_to_the_id_and_names_match_aliases() {
        let plain = model("gpt");
        assert_eq!(plain.upstream_name(), "gpt");
        let routed = Model {
            upstream: "vendor/gpt-1".into(),
            aliases: vec!["Fast".into()],
            ..plain
        };
        assert_eq!(routed.upstream_name(), "vendor/gpt-1");
        assert!(routed.answers_to("GPT") && routed.answers_to("fast"));
        assert!(!routed.answers_to("vendor/gpt-1"));
    }

    #[test]
    fn empty_catalog_lists_route_models_once() {
        let mut config = Config::default();
        assert!(config.catalog().is_empty());
        config.llm.model = "m".into();
        config.llm.fallbacks = vec!["backup".into(), "M".into()];
        config.stt.model = "whisper-1".into();
        config.image.model = "backup".into();
        config.tts.fallbacks = vec!["voice-b".into()];
        let ids: Vec<_> = config.catalog().iter().map(|m| m.id.clone()).collect();
        assert_eq!(ids, ["m", "backup", "voice-b", "whisper-1"]);
        config.models = vec![model("only")];
        assert_eq!(config.catalog().len(), 1);
    }

    #[test]
    fn modalities_derive_from_the_serving_route() {
        let mut config = Config::default();
        config.llm.model = "chat-1".into();
        config.llm.fallbacks = vec!["chat-2".into()];
        config.tts.model = "voice-1".into();
        config.tts.fallbacks = vec!["voice-2".into()];
        config.stt.model = "ears-1".into();
        config.image.model = "paint-1".into();
        let strs = |v: &[String]| v.iter().map(String::as_str).collect::<Vec<_>>().join("+");
        for (id, kind, input, output) in [
            ("chat-1", Capability::Chat, "text", "text"),
            ("CHAT-2", Capability::Chat, "text", "text"),
            ("voice-1", Capability::Speech, "text", "audio"),
            ("voice-2", Capability::Speech, "text", "audio"),
            ("ears-1", Capability::Transcription, "audio", "text"),
            ("paint-1", Capability::Image, "text", "image"),
        ] {
            let r = model(id).resolved(&config);
            assert_eq!(r.capability, Some(kind), "{id}");
            assert_eq!(
                (strs(&r.input).as_str(), strs(&r.output).as_str()),
                (input, output)
            );
        }
        // An alias entry derives from its upstream, not its id.
        let aliased = Model {
            upstream: "voice-1".into(),
            ..model("speaker")
        };
        assert_eq!(
            aliased.resolved(&config).capability,
            Some(Capability::Speech)
        );
        // Explicit modalities win, per side.
        let explicit = Model {
            input_modalities: vec!["text".into(), "image".into()],
            ..model("chat-1")
        };
        let r = explicit.resolved(&config);
        assert_eq!(
            (strs(&r.input), strs(&r.output)),
            ("text+image".into(), "text".into())
        );
        // No route, no guess.
        let r = model("unknown").resolved(&config);
        assert_eq!(
            r,
            Resolved {
                capability: None,
                input: vec![],
                output: vec![]
            }
        );
    }

    #[test]
    fn allowlist_checks_the_entry_id_of_an_upstream_model() {
        let config = Config {
            models: vec![
                Model {
                    upstream: "vendor/fast-1".into(),
                    ..model("fast")
                },
                model("slow"),
            ],
            ..Config::default()
        };
        let only = |id: &str| Limits {
            allowed_models: vec![id.into()],
            ..Limits::default()
        };
        assert!(config.upstream_allowed(&only("FAST"), "vendor/fast-1"));
        assert!(!config.upstream_allowed(&only("vendor/fast-1"), "vendor/fast-1"));
        assert!(config.upstream_allowed(&only("other"), "other"), "raw name");
        assert!(config.upstream_allowed(&Limits::default(), "anything"));
    }
}

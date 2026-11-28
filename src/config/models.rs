//! The public model catalog: which models callers see, what each one does,
//! and the routes that serve it.
use super::{Budget, Config, Provider};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Most catalog entries.
const MAX_MODELS: usize = 200;
/// Most routes of one entry.
const MAX_ROUTES: usize = 20;
/// Most aliases of one entry.
const MAX_ALIASES: usize = 10;
/// Longest public model name, in characters.
const MAX_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 1000;
const MAX_MODALITIES: usize = 8;
const MAX_MODALITY_CHARS: usize = 32;
/// Largest accepted `context_length`.
const MAX_CONTEXT_LENGTH: u64 = 1_000_000_000;

/// Which callers a model serves and whether it is billed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Keyless, rate limited per client, never billed.
    Free,
    /// Needs an API key and a balance.
    #[default]
    Paid,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Paid => "paid",
        }
    }
}

/// How a model with several routes picks one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    /// Score-based spread over the routes that have headroom.
    #[default]
    Balanced,
    /// The first available route in list order.
    Ordered,
}

/// One upstream a model can be served from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Route {
    /// Id of a configured provider.
    pub provider: String,
    /// Model name sent to the provider.
    pub model: String,
    /// Share in the `balanced` strategy (1-100).
    pub weight: u32,
    /// Budget of this (provider, model) pair.
    pub limits: Budget,
    /// A last resort: tried, in listed order, only once no other route of
    /// the model can take the request (each is unavailable or has failed
    /// it). Fallback attempts do not count against `routing.max_attempts`.
    /// For example a cheap paid model behind free routes.
    pub fallback: bool,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            weight: 1,
            limits: Budget::default(),
            fallback: false,
        }
    }
}

/// One public model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Model {
    /// Public model id, unique case-insensitively across ids and aliases.
    pub id: String,
    pub tier: Tier,
    /// What the model does. Required. Chat also covers embeddings,
    /// moderations, completions, responses and realtime.
    pub capability: Option<Capability>,
    /// Served when a request names no model. At most one enabled entry per
    /// capability and tier.
    pub default: bool,
    pub strategy: Strategy,
    /// Speech only: the voice sent when the client names none.
    pub voice: String,
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
    /// Input modalities. Empty derives them from the capability.
    pub input_modalities: Vec<String>,
    /// Output modalities. Empty derives them like `input_modalities`.
    pub output_modalities: Vec<String>,
    /// List the price, with the caller's markup applied (paid models only).
    pub show_pricing: bool,
    /// The upstreams that serve this model (1-20).
    pub routes: Vec<Route>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: String::new(),
            tier: Tier::Paid,
            capability: None,
            default: false,
            strategy: Strategy::Balanced,
            voice: String::new(),
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
            routes: Vec::new(),
        }
    }
}

impl Model {
    /// The capability. Validation requires one, chat is the fallback for an
    /// unvalidated entry.
    pub fn capability(&self) -> Capability {
        self.capability.unwrap_or(Capability::Chat)
    }

    /// Whether `name` is this entry's id or one of its aliases.
    pub fn answers_to(&self, name: &str) -> bool {
        self.id.eq_ignore_ascii_case(name)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
    }

    /// The capability, input and output modalities of this entry. Explicit
    /// modalities win, empty ones come from the capability.
    pub fn resolved(&self) -> Resolved {
        let capability = self.capability();
        let pick = |explicit: &[String], derived: &[&str]| -> Vec<String> {
            if explicit.is_empty() {
                derived.iter().map(|m| (*m).to_owned()).collect()
            } else {
                explicit.to_vec()
            }
        };
        Resolved {
            capability,
            input: pick(&self.input_modalities, capability.input()),
            output: pick(&self.output_modalities, capability.output()),
        }
    }

    fn validate(&self, providers: &[Provider]) -> anyhow::Result<()> {
        public_name("id", &self.id)?;
        anyhow::ensure!(
            self.capability.is_some(),
            "models.{}: capability is required (chat, speech, transcription or image)",
            self.id
        );
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
            ("voice", &self.voice, MAX_NAME_CHARS),
        ] {
            anyhow::ensure!(
                value.chars().count() <= limit && !value.chars().any(char::is_control),
                "models.{}: {field} must be at most {limit} characters without control characters",
                self.id
            );
        }
        anyhow::ensure!(
            self.voice.is_empty() || self.capability() == Capability::Speech,
            "models.{}: voice applies to speech models only",
            self.id
        );
        anyhow::ensure!(
            !(self.show_pricing && self.tier == Tier::Free),
            "models.{}: show_pricing applies to paid models only",
            self.id
        );
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
        anyhow::ensure!(
            (1..=MAX_ROUTES).contains(&self.routes.len()),
            "models.{}: needs 1-{MAX_ROUTES} routes",
            self.id
        );
        anyhow::ensure!(
            self.routes.iter().any(|r| !r.fallback),
            "models.{}: needs at least one route that is not a fallback",
            self.id
        );
        for (i, route) in self.routes.iter().enumerate() {
            anyhow::ensure!(
                providers.iter().any(|p| p.id == route.provider),
                "models.{}: route {} names unknown provider {:?}",
                self.id,
                i + 1,
                route.provider
            );
            public_name("route model", &route.model)?;
            anyhow::ensure!(
                (1..=100).contains(&route.weight),
                "models.{}: route weight must be between 1 and 100",
                self.id
            );
            route
                .limits
                .validate(&format!("models.{}.routes", self.id))?;
            anyhow::ensure!(
                !self.routes[..i]
                    .iter()
                    .any(|r| r.provider == route.provider && r.model == route.model),
                "models.{}: route {}/{} is listed twice",
                self.id,
                route.provider,
                route.model
            );
        }
        Ok(())
    }
}

/// What a model does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    pub capability: Capability,
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

/// Whether `model` is enabled and has a route on an enabled provider.
fn servable(model: &Model, providers: &[Provider]) -> bool {
    model.enabled
        && model
            .routes
            .iter()
            .any(|r| providers.iter().any(|p| p.id == r.provider && p.enabled))
}

/// Checks the catalog: sizes, fields, routes, unique names, and at most one
/// servable default per capability and tier.
pub(super) fn validate(models: &[Model], providers: &[Provider]) -> anyhow::Result<()> {
    anyhow::ensure!(
        models.len() <= MAX_MODELS,
        "at most {MAX_MODELS} models are allowed"
    );
    let mut names = HashSet::new();
    let mut defaults = HashSet::new();
    for model in models {
        model.validate(providers)?;
        for name in std::iter::once(&model.id).chain(&model.aliases) {
            anyhow::ensure!(
                names.insert(name.to_lowercase()),
                "model name {name:?} is used twice, ids and aliases must be unique"
            );
        }
        // Entries without an enabled provider (for example a provider whose
        // key variable is unset) are hidden, so they never compete.
        anyhow::ensure!(
            !(model.default && servable(model, providers))
                || defaults.insert((model.capability(), model.tier)),
            "models.{}: only one servable default per capability and tier is allowed",
            model.id
        );
    }
    Ok(())
}

impl Config {
    /// The provider with `id`.
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Whether `model` can be served: it is enabled and at least one of its
    /// routes is on an enabled provider. Other entries are hidden and refused
    /// like disabled ones.
    pub fn serves(&self, model: &Model) -> bool {
        servable(model, &self.providers)
    }

    /// The servable catalog entry a client name (id or alias) refers to.
    pub fn find_model(&self, name: &str) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.answers_to(name) && self.serves(m))
    }

    /// The servable default entry of a capability and tier.
    pub fn default_model(&self, capability: Capability, tier: Tier) -> Option<&Model> {
        self.models
            .iter()
            .find(|m| m.default && m.capability() == capability && m.tier == tier && self.serves(m))
    }

    /// Whether anyone can try the service for free: keyless access is on and
    /// at least one servable model is free.
    pub fn offers_free_tier(&self) -> bool {
        self.free.enabled
            && self
                .models
                .iter()
                .any(|m| m.tier == Tier::Free && self.serves(m))
    }

    /// Every distinct (provider, upstream model) pair of the enabled
    /// catalog, with the public models that use it.
    pub fn route_pairs(&self) -> Vec<(&str, &str, Vec<&str>)> {
        let mut out: Vec<(&str, &str, Vec<&str>)> = Vec::new();
        for model in &self.models {
            for route in &model.routes {
                match out
                    .iter_mut()
                    .find(|(p, m, _)| *p == route.provider && *m == route.model)
                {
                    Some((_, _, users)) => users.push(&model.id),
                    None => out.push((&route.provider, &route.model, vec![&model.id])),
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Vec<Provider> {
        vec![Provider {
            id: "p".into(),
            base_url: "https://p.example/v1".into(),
            ..Provider::default()
        }]
    }

    fn route(model: &str) -> Route {
        Route {
            provider: "p".into(),
            model: model.into(),
            ..Route::default()
        }
    }

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            capability: Some(Capability::Chat),
            routes: vec![route(id)],
            ..Model::default()
        }
    }

    fn check(models: &[Model]) -> anyhow::Result<()> {
        validate(models, &providers())
    }

    #[test]
    fn names_are_unique_case_insensitively_across_ids_and_aliases() {
        assert!(check(&[model("a"), model("b")]).is_ok());
        assert!(check(&[model("a"), model("A")]).is_err());
        let aliased = Model {
            aliases: vec!["Fast".into()],
            ..model("a")
        };
        assert!(check(&[aliased.clone(), model("fast")]).is_err());
        let twice = Model {
            aliases: vec!["x".into(), "X".into()],
            ..model("a")
        };
        assert!(check(&[twice]).is_err());
        assert!(check(&[aliased]).is_ok());
    }

    #[test]
    fn fields_are_bounded() {
        for bad in [
            model(""),
            model(&"x".repeat(129)),
            model("a\nb"),
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
            assert!(check(std::slice::from_ref(&bad)).is_err(), "{bad:?}");
        }
        let many: Vec<Model> = (0..=MAX_MODELS).map(|i| model(&format!("m{i}"))).collect();
        assert!(check(&many).is_err());
        assert!(check(&many[..MAX_MODELS]).is_ok());
    }

    #[test]
    fn capability_routes_and_tier_rules_are_enforced() {
        let missing = Model {
            capability: None,
            ..model("a")
        };
        assert!(check(&[missing]).is_err(), "capability is required");
        let no_routes = Model {
            routes: vec![],
            ..model("a")
        };
        assert!(check(&[no_routes]).is_err());
        let too_many = Model {
            routes: (0..21).map(|i| route(&format!("m{i}"))).collect(),
            ..model("a")
        };
        assert!(check(&[too_many]).is_err());
        let unknown_provider = Model {
            routes: vec![Route {
                provider: "nope".into(),
                ..route("x")
            }],
            ..model("a")
        };
        assert!(check(&[unknown_provider]).is_err());
        for weight in [0, 101] {
            let bad = Model {
                routes: vec![Route {
                    weight,
                    ..route("x")
                }],
                ..model("a")
            };
            assert!(check(&[bad]).is_err(), "weight {weight}");
        }
        let duplicate = Model {
            routes: vec![route("x"), route("x")],
            ..model("a")
        };
        assert!(check(&[duplicate]).is_err());
        let priced_free = Model {
            tier: Tier::Free,
            show_pricing: true,
            ..model("a")
        };
        assert!(check(&[priced_free]).is_err());
        let voiced_chat = Model {
            voice: "alloy".into(),
            ..model("a")
        };
        assert!(check(&[voiced_chat]).is_err());
        let voiced_speech = Model {
            capability: Some(Capability::Speech),
            voice: "alloy".into(),
            ..model("a")
        };
        assert!(check(&[voiced_speech]).is_ok());
    }

    #[test]
    fn one_default_per_capability_and_tier() {
        let default = |id: &str, tier, capability| Model {
            default: true,
            tier,
            capability: Some(capability),
            ..model(id)
        };
        assert!(
            check(&[
                default("a", Tier::Paid, Capability::Chat),
                default("b", Tier::Free, Capability::Chat),
                default("c", Tier::Paid, Capability::Speech),
            ])
            .is_ok()
        );
        assert!(
            check(&[
                default("a", Tier::Paid, Capability::Chat),
                default("b", Tier::Paid, Capability::Chat),
            ])
            .is_err()
        );
        let retired = Model {
            enabled: false,
            ..default("b", Tier::Paid, Capability::Chat)
        };
        assert!(check(&[default("a", Tier::Paid, Capability::Chat), retired]).is_ok());
    }

    #[test]
    fn modalities_derive_from_the_capability_and_explicit_ones_win() {
        let strs = |v: &[String]| v.iter().map(String::as_str).collect::<Vec<_>>().join("+");
        for (capability, input, output) in [
            (Capability::Chat, "text", "text"),
            (Capability::Speech, "text", "audio"),
            (Capability::Transcription, "audio", "text"),
            (Capability::Image, "text", "image"),
        ] {
            let r = Model {
                capability: Some(capability),
                ..model("m")
            }
            .resolved();
            assert_eq!(r.capability, capability);
            assert_eq!(
                (strs(&r.input).as_str(), strs(&r.output).as_str()),
                (input, output)
            );
        }
        let explicit = Model {
            input_modalities: vec!["text".into(), "image".into()],
            ..model("m")
        };
        let r = explicit.resolved();
        assert_eq!(
            (strs(&r.input), strs(&r.output)),
            ("text+image".into(), "text".into())
        );
    }

    #[test]
    fn lookups_follow_aliases_and_enabled_flags() {
        let config = Config {
            providers: providers(),
            models: vec![
                Model {
                    aliases: vec!["Fast".into()],
                    default: true,
                    ..model("a")
                },
                Model {
                    enabled: false,
                    ..model("off")
                },
                Model {
                    tier: Tier::Free,
                    default: true,
                    ..model("free")
                },
            ],
            ..Config::default()
        };
        assert_eq!(config.find_model("FAST").unwrap().id, "a");
        assert!(config.find_model("off").is_none());
        assert_eq!(
            config
                .default_model(Capability::Chat, Tier::Paid)
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            config
                .default_model(Capability::Chat, Tier::Free)
                .unwrap()
                .id,
            "free"
        );
        assert!(
            config
                .default_model(Capability::Image, Tier::Free)
                .is_none()
        );
        let pairs = config.route_pairs();
        assert!(
            pairs
                .iter()
                .any(|(p, m, users)| *p == "p" && *m == "a" && users == &["a"])
        );
    }
}

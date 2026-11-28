//! The local model list (`GET /v1/models`, `GET /v1/models/{id}`) and the
//! resolution of public model names to upstream names.
//!
//! Upstream names, upstream URLs, keys and raw upstream prices never appear
//! here. Prices carry the caller's markup.
use crate::{
    App,
    config::{Capability, Config, Model, Tier},
    error::{Error, Result},
    identity::{Caller, Identity},
    meter, money,
};
use axum::{Json, body::Body, http::Response, response::IntoResponse};
use percent_encoding::percent_decode_str;
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// Serves a models route for a caller. The request is not metered or logged,
/// but a key's status is checked as at admission.
pub(super) async fn serve(app: &App, caller: &Caller, path: &str) -> Result<Response<Body>> {
    if let Some(identity) = caller.identity() {
        meter::preflight(app, identity).await?;
    }
    let config = app.config().await;
    let body = match path.strip_prefix("/v1/models/") {
        Some(id) => retrieve(&config, caller, &percent_decode_str(id).decode_utf8_lossy())?,
        None => list(&config, caller),
    };
    Ok(Json(body).into_response())
}

/// 401 for an anonymous request to a paid model.
fn api_key_required(model: &str) -> Error {
    Error::unauthorized(
        "api_key_required",
        format!("Model {model} requires an API key"),
    )
}

/// Resolves the model a request asked for (an id or alias, `None` for no
/// model) to its enabled catalog entry, for a request of `capability`.
///
/// - A named model must be enabled, and a key's allowlist must name it.
///   Unknown, disabled and not allowed models are indistinguishable.
/// - Anonymous callers may name free models only (`api_key_required`).
/// - Without a name the capability's default applies: the paid default for
///   keys and the operator (else the free one), the free default for anonymous
///   callers.
pub(super) fn resolve<'a>(
    config: &'a Config,
    caller: &Caller,
    capability: Capability,
    requested: Option<&str>,
) -> Result<&'a Model> {
    let allowed = |m: &Model| match caller {
        Caller::Key(identity) => identity.limits.allows(&m.id),
        _ => true,
    };
    if let Some(name) = requested.filter(|n| !n.is_empty()) {
        let model = config
            .find_model(name)
            .filter(|m| allowed(m))
            .ok_or_else(|| Error::model_not_found(name))?;
        if matches!(caller, Caller::Anonymous) && model.tier == Tier::Paid {
            return Err(api_key_required(name));
        }
        if model.capability() != capability {
            return Err(Error::bad(format!(
                "The model '{name}' cannot be used with this endpoint"
            ))
            .with_code("unsupported_model")
            .with_param("model"));
        }
        return Ok(model);
    }
    let tiers: &[Tier] = match caller {
        Caller::Anonymous => &[Tier::Free],
        _ => &[Tier::Paid, Tier::Free],
    };
    let defaults: Vec<&Model> = tiers
        .iter()
        .filter_map(|t| config.default_model(capability, *t))
        .collect();
    defaults
        .iter()
        .find(|m| allowed(m))
        .copied()
        .ok_or_else(|| {
            if defaults.is_empty() {
                Error::bad("This request needs a model: none is configured as the default")
                    .with_code("model_required")
                    .with_param("model")
            } else {
                Error::model_not_found("")
            }
        })
}

/// The `{"object":"list"}` response for the caller: enabled catalog entries
/// the caller may use, priced with its markup when pricing is shown.
/// Anonymous callers see free models only.
pub fn list(config: &Config, caller: &Caller) -> Value {
    let data: Vec<Value> = config
        .models
        .iter()
        .filter(|m| visible(config, m, caller))
        .map(|m| entry(config, caller.identity(), m))
        .collect();
    json!({"object": "list", "data": data})
}

/// One model by id or alias.
fn retrieve(config: &Config, caller: &Caller, id: &str) -> Result<Value> {
    config
        .models
        .iter()
        .find(|m| visible(config, m, caller) && m.answers_to(id))
        .map(|m| entry(config, caller.identity(), m))
        .ok_or_else(|| Error::model_not_found(id))
}

fn visible(config: &Config, model: &Model, caller: &Caller) -> bool {
    config.serves(model)
        && match caller {
            Caller::Anonymous => model.tier == Tier::Free,
            Caller::Operator => true,
            Caller::Key(identity) => identity.limits.allows(&model.id),
        }
}

fn entry(config: &Config, identity: Option<&Identity>, model: &Model) -> Value {
    let owner = if model.owned_by.is_empty() {
        &config.branding.name
    } else {
        &model.owned_by
    };
    let mut out = json!({
        "id": model.id,
        "object": "model",
        "created": model.created,
        "owned_by": owner,
        "tier": model.tier.as_str(),
    });
    if !model.name.is_empty() {
        out["name"] = model.name.clone().into();
    }
    if !model.description.is_empty() {
        out["description"] = model.description.clone().into();
    }
    if let Some(length) = model.context_length {
        out["context_length"] = length.into();
    }
    let resolved = model.resolved();
    let mut architecture = serde_json::Map::new();
    if !resolved.input.is_empty() {
        architecture.insert("input_modalities".into(), json!(resolved.input));
    }
    if !resolved.output.is_empty() {
        architecture.insert("output_modalities".into(), json!(resolved.output));
    }
    if !resolved.input.is_empty() && !resolved.output.is_empty() {
        architecture.insert(
            "modality".into(),
            format!(
                "{}->{}",
                resolved.input.join("+"),
                resolved.output.join("+")
            )
            .into(),
        );
    }
    architecture.insert("capability".into(), resolved.capability.as_str().into());
    out["architecture"] = architecture.into();
    // The first route's price. Every route is billed at its own model's price.
    if model.show_pricing
        && model.tier == Tier::Paid
        && let Some(route) = model.routes.first()
        && let Some(pricing) = pricing(config, identity, &route.model)
    {
        out["pricing"] = pricing;
    }
    out
}

/// Per-unit USD prices of `upstream`, as the caller is billed: markup from
/// the key, else the account, else the global setting.
fn pricing(config: &Config, identity: Option<&Identity>, upstream: &str) -> Option<Value> {
    let price = config.price(upstream)?;
    let markup = identity.map_or(config.markup_pct.as_str(), |i| i.markup.as_str());
    let factor = Decimal::ONE + money::decimal(markup).ok()? / Decimal::ONE_HUNDRED;
    let unit = |usd: &str, per: u32| {
        Some(
            (money::decimal(usd).ok()? * factor / Decimal::from(per))
                .normalize()
                .to_string(),
        )
    };
    let mut out = json!({
        "prompt": unit(&price.input, 1_000_000)?,
        "completion": unit(&price.output, 1_000_000)?,
    });
    if let Some(cached) = &price.cached_input {
        out["input_cache_read"] = unit(cached, 1_000_000)?.into();
    }
    out["image"] = unit(&price.image, 1)?.into();
    for (key, usd, per) in [
        ("audio_second", &price.audio_second, 1),
        ("characters", &price.characters, 1_000_000),
    ] {
        if !money::decimal(usd).ok()?.is_zero() {
            out[key] = unit(usd, per)?.into();
        }
    }
    if !money::decimal(&price.request).ok()?.is_zero() {
        out["request"] = unit(&price.request, 1)?.into();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Price, Provider, Route},
        keys::Limits,
    };

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            capability: Some(Capability::Chat),
            routes: vec![Route {
                provider: "p".into(),
                model: id.into(),
                ..Route::default()
            }],
            ..Model::default()
        }
    }

    fn key(allowed: &[&str], markup: &str) -> Caller {
        Caller::Key(Identity {
            key_id: "k".into(),
            account_id: "a".into(),
            dashboard: false,
            prefix: "sk-res-ab".into(),
            active: true,
            limits: Limits {
                allowed_models: allowed.iter().map(|m| (*m).to_owned()).collect(),
                ..Limits::default()
            },
            markup: markup.into(),
        })
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.branding.name = "Acme".into();
        config.markup_pct = "30".into();
        config.providers = vec![Provider {
            id: "p".into(),
            base_url: "https://p.example/v1".into(),
            ..Provider::default()
        }];
        config.prices.insert(
            "vendor/fast-1".into(),
            Price {
                input: "2".into(),
                output: "8".into(),
                cached_input: Some("0.5".into()),
                image: "0.04".into(),
                request: "0.001".into(),
                ..Price::default()
            },
        );
        config.models = vec![
            Model {
                aliases: vec!["Fast".into(), "latest".into()],
                name: "Fast One".into(),
                description: "Quick answers".into(),
                owned_by: "Vendor".into(),
                created: 1_700_000_000,
                context_length: Some(128_000),
                input_modalities: vec!["text".into(), "image".into()],
                output_modalities: vec!["text".into()],
                show_pricing: true,
                routes: vec![Route {
                    provider: "p".into(),
                    model: "vendor/fast-1".into(),
                    ..Route::default()
                }],
                ..model("fast")
            },
            model("plain"),
            Model {
                enabled: false,
                ..model("retired")
            },
            Model {
                tier: Tier::Free,
                default: true,
                ..model("gratis")
            },
        ];
        config
    }

    fn ids(list: &Value) -> Vec<&str> {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn list_hides_disabled_entries_and_aliases_and_filters_by_caller() {
        let config = config();
        let anonymous = list(&config, &Caller::Anonymous);
        assert_eq!(anonymous["object"], "list");
        assert_eq!(
            ids(&anonymous),
            ["gratis"],
            "anonymous callers see free models"
        );
        assert_eq!(anonymous["data"][0]["tier"], "free");
        assert_eq!(
            ids(&list(&config, &key(&[], "30"))),
            ["fast", "plain", "gratis"]
        );
        assert_eq!(
            ids(&list(&config, &Caller::Operator)),
            ["fast", "plain", "gratis"]
        );
        let paid = list(&config, &key(&[], "30"));
        assert_eq!(paid["data"][0]["tier"], "paid");
    }

    #[test]
    fn entries_carry_overrides_and_optional_fields_only_when_set() {
        let list = list(&config(), &key(&[], "30"));
        let fast = &list["data"][0];
        assert_eq!(fast["object"], "model");
        assert_eq!(fast["created"], 1_700_000_000);
        assert_eq!(fast["owned_by"], "Vendor");
        assert_eq!(fast["name"], "Fast One");
        assert_eq!(fast["description"], "Quick answers");
        assert_eq!(fast["context_length"], 128_000);
        assert_eq!(
            fast["architecture"],
            json!({
                "capability": "chat",
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
                "modality": "text+image->text",
            })
        );
        let plain = &list["data"][1];
        assert_eq!(plain["owned_by"], "Acme", "empty owner is the brand");
        assert_eq!(plain["created"], 0);
        for absent in ["name", "description", "context_length", "pricing"] {
            assert!(plain.get(absent).is_none(), "{absent}");
        }
        assert!(!list.to_string().contains("vendor/fast-1"), "upstream name");
        assert!(!list.to_string().contains("\"p\""), "provider id");
    }

    #[test]
    fn pricing_is_hidden_by_default_and_shown_with_the_callers_markup() {
        let mut config = config();
        let caller = key(&[], "30");
        assert_eq!(list(&config, &caller)["data"][1].get("pricing"), None);
        // Global markup of 30 percent for a caller without a key.
        let operator = list(&config, &Caller::Operator);
        assert_eq!(
            operator["data"][0]["pricing"],
            json!({
                "prompt": "0.0000026",
                "completion": "0.0000104",
                "input_cache_read": "0.00000065",
                "image": "0.052",
                "request": "0.0013",
            })
        );
        // The key's own markup wins.
        let doubled = list(&config, &key(&[], "100"));
        assert_eq!(doubled["data"][0]["pricing"]["prompt"], "0.000004");
        // Optional prices are left out when unset or zero.
        config.prices.get_mut("vendor/fast-1").unwrap().cached_input = None;
        config.prices.get_mut("vendor/fast-1").unwrap().request = "0".into();
        assert!(
            list(&config, &caller)["data"][0]["pricing"]
                .get("characters")
                .is_none()
        );
        let price = config.prices.get_mut("vendor/fast-1").unwrap();
        price.audio_second = "0.001".into();
        price.characters = "15".into();
        let speech = &list(&config, &caller)["data"][0]["pricing"];
        assert_eq!(speech["audio_second"], "0.0013");
        assert_eq!(speech["characters"], "0.0000195");
        let price = config.prices.get_mut("vendor/fast-1").unwrap();
        (price.audio_second, price.characters) = ("0".into(), "0".into());
        let pricing = &list(&config, &caller)["data"][0]["pricing"];
        assert!(pricing.get("input_cache_read").is_none() && pricing.get("request").is_none());
    }

    #[test]
    fn keys_see_only_the_models_their_allowlist_names() {
        let config = config();
        let caller = key(&["PLAIN"], "30");
        assert_eq!(ids(&list(&config, &caller)), ["plain"]);
        assert_eq!(
            retrieve(&config, &caller, "fast").unwrap_err().code,
            "model_not_found"
        );
        assert!(retrieve(&config, &caller, "plain").is_ok());
        // Aliases and upstream names do not satisfy an allowlist.
        let by_alias = key(&["latest", "vendor/fast-1"], "30");
        assert!(resolve(&config, &by_alias, Capability::Chat, Some("fast")).is_err());
        assert!(resolve(&config, &by_alias, Capability::Chat, Some("latest")).is_err());
        assert!(
            list(&config, &by_alias)["data"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retrieval_and_resolution_follow_aliases_case_insensitively() {
        let config = config();
        let caller = key(&[], "30");
        assert_eq!(retrieve(&config, &caller, "LATEST").unwrap()["id"], "fast");
        assert_eq!(
            resolve(&config, &caller, Capability::Chat, Some("Fast"))
                .unwrap()
                .id,
            "fast"
        );
        for hidden in ["retired", "vendor/fast-1", "missing"] {
            let e = resolve(&config, &caller, Capability::Chat, Some(hidden)).unwrap_err();
            assert_eq!(
                (e.status.as_u16(), e.code, e.param),
                (404, "model_not_found", Some("model"))
            );
            assert_eq!(
                e.message,
                format!("The model '{hidden}' does not exist or you do not have access to it.")
            );
            assert!(retrieve(&config, &caller, hidden).is_err());
        }
    }

    #[test]
    fn anonymous_callers_need_a_key_for_paid_models() {
        let config = config();
        let e = resolve(&config, &Caller::Anonymous, Capability::Chat, Some("fast")).unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (401, "api_key_required"));
        assert_eq!(e.message, "Model fast requires an API key");
        assert_eq!(
            resolve(
                &config,
                &Caller::Anonymous,
                Capability::Chat,
                Some("gratis")
            )
            .unwrap()
            .id,
            "gratis"
        );
        // Unknown models stay unknown rather than asking for a key.
        let e = resolve(&config, &Caller::Anonymous, Capability::Chat, Some("nope")).unwrap_err();
        assert_eq!(e.code, "model_not_found");
        assert!(retrieve(&config, &Caller::Anonymous, "fast").is_err());
    }

    #[test]
    fn defaults_follow_the_caller_and_the_capability() {
        let mut config = config();
        // No paid default: keys fall back to the free one.
        let caller = key(&[], "30");
        assert_eq!(
            resolve(&config, &caller, Capability::Chat, None)
                .unwrap()
                .id,
            "gratis"
        );
        config.models[1].default = true;
        assert_eq!(
            resolve(&config, &caller, Capability::Chat, None)
                .unwrap()
                .id,
            "plain"
        );
        assert_eq!(
            resolve(&config, &Caller::Operator, Capability::Chat, Some(""))
                .unwrap()
                .id,
            "plain"
        );
        // Anonymous callers only ever get the free default.
        assert_eq!(
            resolve(&config, &Caller::Anonymous, Capability::Chat, None)
                .unwrap()
                .id,
            "gratis"
        );
        let e = resolve(&config, &Caller::Anonymous, Capability::Image, None).unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (400, "model_required"));
        // A restricted key whose defaults it may not use must name a model.
        let restricted = key(&["fast"], "30");
        assert_eq!(
            resolve(&config, &restricted, Capability::Chat, None)
                .unwrap_err()
                .code,
            "model_not_found"
        );
        // A model of another capability is refused with a clear error.
        let e = resolve(&config, &caller, Capability::Speech, Some("plain")).unwrap_err();
        assert_eq!((e.status.as_u16(), e.code), (400, "unsupported_model"));
    }
}

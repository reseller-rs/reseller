//! The local model list (`GET /v1/models`, `GET /v1/models/{id}`) and the
//! resolution of public model names to upstream names.
//!
//! Upstream names, upstream URLs, keys and raw upstream prices never appear
//! here. Prices carry the caller's markup.
use crate::{
    App,
    config::{Config, Model},
    error::{Error, Result},
    identity::Identity,
    meter, money,
};
use axum::{Json, body::Body, http::Response, response::IntoResponse};
use percent_encoding::percent_decode_str;
use rust_decimal::Decimal;
use serde_json::{Value, json};

/// Serves a models route for an authenticated (or anonymous) caller. The
/// request is not metered or logged, but the key and balance are checked as
/// at admission.
pub(super) async fn serve(
    app: &App,
    identity: Option<&Identity>,
    path: &str,
) -> Result<Response<Body>> {
    if let Some(identity) = identity {
        meter::preflight(app, identity).await?;
    }
    let config = app.config().await;
    let body = match path.strip_prefix("/v1/models/") {
        Some(id) => retrieve(
            &config,
            identity,
            &percent_decode_str(id).decode_utf8_lossy(),
        )?,
        None => list(&config, identity),
    };
    Ok(Json(body).into_response())
}

/// Resolves a model name sent by a client to its enabled catalog entry.
/// Unknown, disabled and not allowed models are indistinguishable.
pub(super) fn resolve<'a>(
    config: &'a Config,
    identity: Option<&Identity>,
    requested: &str,
) -> Result<&'a Model> {
    config
        .models
        .iter()
        .find(|m| m.enabled && m.answers_to(requested))
        .filter(|m| identity.is_none_or(|i| i.limits.allows(&m.id)))
        .ok_or_else(|| Error::model_not_found(requested))
}

/// The `{"object":"list"}` response for the caller.
fn list(config: &Config, identity: Option<&Identity>) -> Value {
    let data: Vec<Value> = config
        .catalog()
        .iter()
        .filter(|m| visible(m, identity))
        .map(|m| entry(config, identity, m))
        .collect();
    json!({"object": "list", "data": data})
}

/// One model by id or alias.
fn retrieve(config: &Config, identity: Option<&Identity>, id: &str) -> Result<Value> {
    config
        .catalog()
        .iter()
        .find(|m| visible(m, identity) && m.answers_to(id))
        .map(|m| entry(config, identity, m))
        .ok_or_else(|| Error::model_not_found(id))
}

fn visible(model: &Model, identity: Option<&Identity>) -> bool {
    model.enabled && identity.is_none_or(|i| i.limits.allows(&model.id))
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
    if !model.input_modalities.is_empty() && !model.output_modalities.is_empty() {
        out["architecture"] = json!({
            "input_modalities": model.input_modalities,
            "output_modalities": model.output_modalities,
            "modality": format!(
                "{}->{}",
                model.input_modalities.join("+"),
                model.output_modalities.join("+")
            ),
        });
    }
    if model.show_pricing
        && let Some(pricing) = pricing(config, identity, model.upstream_name())
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
    if !money::decimal(&price.request).ok()?.is_zero() {
        out["request"] = unit(&price.request, 1)?.into();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Price, keys::Limits};

    fn model(id: &str) -> Model {
        Model {
            id: id.into(),
            ..Model::default()
        }
    }

    fn identity(allowed: &[&str], markup: &str) -> Identity {
        Identity {
            key_id: "k".into(),
            account_id: "a".into(),
            dashboard: false,
            prefix: "sk-res-ab".into(),
            limits: Limits {
                allowed_models: allowed.iter().map(|m| (*m).to_owned()).collect(),
                ..Limits::default()
            },
            markup: markup.into(),
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.branding.name = "Acme".into();
        config.markup_pct = "30".into();
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
                upstream: "vendor/fast-1".into(),
                aliases: vec!["Fast".into(), "latest".into()],
                name: "Fast One".into(),
                description: "Quick answers".into(),
                owned_by: "Vendor".into(),
                created: 1_700_000_000,
                context_length: Some(128_000),
                input_modalities: vec!["text".into(), "image".into()],
                show_pricing: true,
                ..model("fast")
            },
            model("plain"),
            Model {
                enabled: false,
                ..model("retired")
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
    fn list_hides_disabled_entries_and_aliases() {
        let list = list(&config(), None);
        assert_eq!(list["object"], "list");
        assert_eq!(ids(&list), ["fast", "plain"]);
    }

    #[test]
    fn entries_carry_overrides_and_optional_fields_only_when_set() {
        let list = list(&config(), None);
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
    }

    #[test]
    fn pricing_is_hidden_by_default_and_shown_with_the_callers_markup() {
        let mut config = config();
        assert_eq!(list(&config, None)["data"][1].get("pricing"), None);
        // Global markup of 30 percent.
        let anonymous = list(&config, None);
        assert_eq!(
            anonymous["data"][0]["pricing"],
            json!({
                "prompt": "0.0000026",
                "completion": "0.0000104",
                "input_cache_read": "0.00000065",
                "image": "0.052",
                "request": "0.0013",
            })
        );
        // The key's own markup wins.
        let key = identity(&[], "100");
        let doubled = list(&config, Some(&key));
        assert_eq!(doubled["data"][0]["pricing"]["prompt"], "0.000004");
        // Optional prices are left out when unset or zero.
        config.prices.get_mut("vendor/fast-1").unwrap().cached_input = None;
        config.prices.get_mut("vendor/fast-1").unwrap().request = "0".into();
        let pricing = &list(&config, None)["data"][0]["pricing"];
        assert!(pricing.get("input_cache_read").is_none() && pricing.get("request").is_none());
    }

    #[test]
    fn keys_see_only_the_models_their_allowlist_names() {
        let config = config();
        let key = identity(&["PLAIN"], "30");
        assert_eq!(ids(&list(&config, Some(&key))), ["plain"]);
        assert_eq!(
            retrieve(&config, Some(&key), "fast").unwrap_err().code,
            "model_not_found"
        );
        assert!(retrieve(&config, Some(&key), "plain").is_ok());
        // Aliases and upstream names do not satisfy an allowlist.
        let by_alias = identity(&["latest", "vendor/fast-1"], "30");
        assert!(resolve(&config, Some(&by_alias), "fast").is_err());
        assert!(resolve(&config, Some(&by_alias), "latest").is_err());
        assert!(
            list(&config, Some(&by_alias))["data"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retrieval_and_resolution_follow_aliases_case_insensitively() {
        let config = config();
        assert_eq!(retrieve(&config, None, "LATEST").unwrap()["id"], "fast");
        assert_eq!(resolve(&config, None, "Fast").unwrap().id, "fast");
        assert_eq!(
            resolve(&config, None, "fast").unwrap().upstream_name(),
            "vendor/fast-1"
        );
        for hidden in ["retired", "vendor/fast-1", "missing"] {
            let e = resolve(&config, None, hidden).unwrap_err();
            assert_eq!(
                (e.status.as_u16(), e.code, e.param),
                (404, "model_not_found", Some("model"))
            );
            assert_eq!(
                e.message,
                format!("The model '{hidden}' does not exist or you do not have access to it.")
            );
            assert!(retrieve(&config, None, hidden).is_err());
        }
    }

    #[test]
    fn an_empty_catalog_lists_the_route_models() {
        let mut config = Config::default();
        config.branding.name = "Acme".into();
        config.llm.model = "m".into();
        config.llm.fallbacks = vec!["backup".into()];
        config.tts.model = "tts-1".into();
        let list = list(&config, None);
        assert_eq!(ids(&list), ["m", "backup", "tts-1"]);
        assert_eq!(
            list["data"][0],
            json!({"id": "m", "object": "model", "created": 0, "owned_by": "Acme"})
        );
        let key = identity(&["BACKUP"], "30");
        assert_eq!(ids(&super::list(&config, Some(&key))), ["backup"]);
        assert!(retrieve(&config, None, "tts-1").is_ok());
        assert!(retrieve(&config, None, "other").is_err());
    }
}

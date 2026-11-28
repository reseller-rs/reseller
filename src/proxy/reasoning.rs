//! Reasoning effort policy for LLM requests.
//!
//! Clients set the level as `reasoning_effort` (Chat Completions) or
//! `reasoning.effort` (Responses API, and OpenRouter's chat form). Supported
//! levels differ by model, so the policy in `[reasoning]` applies per
//! upstream model:
//!
//! - A configured model with levels: an unsupported requested level is a 400
//!   before anything is sent upstream, and a missing level gets the model's
//!   `default`.
//! - A configured model without levels has no reasoning control: the
//!   parameter is removed, so the upstream cannot reject it.
//! - Requests to free models are capped at `free_effort` (the highest supported level
//!   at or below it), and configured reasoning models get that level even
//!   when the request names none.
//! - Models that are not configured get the client's value unchanged, apart
//!   from the free cap.
//!
//! Each attempt gets the policy again for the upstream model of its route,
//! from the original request, with an unsupported level mapped to the
//! nearest lower one instead of a 400.
use crate::{
    config::{Config, ReasoningModel, effort_rank},
    error::{Error, Result},
};
use serde_json::Value;

/// Routes whose JSON body carries a reasoning level.
pub fn applies(path: &str) -> bool {
    matches!(path, "/v1/chat/completions" | "/v1/responses")
}

/// The level a request asked for, and whether it used the nested
/// `reasoning.effort` form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Requested {
    pub level: String,
    pub nested: bool,
}

fn invalid_type(param: &'static str) -> Error {
    Error::bad(format!("{param} must be a string")).with_param(param)
}

/// Reads the requested level. `reasoning_effort` wins over `reasoning.effort`.
pub fn requested(body: &Value) -> Result<Option<Requested>> {
    match body.get("reasoning_effort") {
        None | Some(Value::Null) => {}
        Some(Value::String(level)) => {
            return Ok(Some(Requested {
                level: level.clone(),
                nested: false,
            }));
        }
        Some(_) => return Err(invalid_type("reasoning_effort")),
    }
    match body.get("reasoning").and_then(|r| r.get("effort")) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(level)) => Ok(Some(Requested {
            level: level.clone(),
            nested: true,
        })),
        Some(_) => Err(invalid_type("reasoning.effort")),
    }
}

/// The model's levels ordered from lowest to highest.
fn ordered(spec: &ReasoningModel) -> Vec<&str> {
    let mut levels: Vec<&str> = spec.efforts.iter().map(String::as_str).collect();
    levels.sort_by_key(|l| effort_rank(l));
    levels
}

/// The highest supported level at or below `ceiling`, else the lowest
/// supported level.
fn at_most<'a>(spec: &'a ReasoningModel, ceiling: &str) -> Option<&'a str> {
    let levels = ordered(spec);
    let limit = effort_rank(ceiling);
    levels
        .iter()
        .rev()
        .find(|l| effort_rank(l) <= limit)
        .or(levels.first())
        .map(|l| {
            spec.efforts
                .iter()
                .find(|e| e == l)
                .map_or(*l, String::as_str)
        })
}

fn higher(level: &str, than: &str) -> bool {
    matches!((effort_rank(level), effort_rank(than)), (Some(a), Some(b)) if a > b)
}

/// Applies the policy for `model` to `body`. `free` (a free model) caps the
/// level, and `strict` turns an unsupported level into a 400 (the first
/// route) rather than mapping it down (later attempts).
pub fn apply(
    body: &mut Value,
    path: &str,
    config: &Config,
    model: &str,
    original: Option<&Requested>,
    free: bool,
    strict: bool,
) -> Result<()> {
    let cap = Some(config.reasoning.free_effort.as_str()).filter(|c| free && !c.is_empty());
    let target: Option<String> = match config.reasoning.model(model) {
        Some(spec) if spec.efforts.is_empty() => None,
        Some(spec) => {
            let wanted =
                match original {
                    Some(r) if spec.efforts.contains(&r.level) => Some(r.level.as_str()),
                    Some(r) if strict => {
                        return Err(Error::bad(format!(
                        "The model '{model}' does not support reasoning_effort '{}'. Supported: {}",
                        r.level,
                        ordered(spec).join(", ")
                    ))
                    .with_code("unsupported_reasoning_effort")
                    .with_param(if r.nested { "reasoning.effort" } else { "reasoning_effort" }));
                    }
                    Some(r) => at_most(spec, &r.level),
                    None => Some(spec.default.as_str()).filter(|d| !d.is_empty()),
                };
            match cap {
                Some(cap) if wanted.is_none_or(|w| higher(w, cap)) => at_most(spec, cap),
                _ => wanted,
            }
            .map(str::to_owned)
        }
        None => original.map(|r| match cap {
            Some(cap) if higher(&r.level, cap) => cap.to_owned(),
            _ => r.level.clone(),
        }),
    };
    write(
        body,
        path,
        original.is_some_and(|r| r.nested),
        target.as_deref(),
    );
    Ok(())
}

/// Replaces the level in `body`: nested for the Responses API or when the
/// client used the nested form, top-level `reasoning_effort` otherwise.
/// `None` removes it.
fn write(body: &mut Value, path: &str, nested: bool, level: Option<&str>) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    object.remove("reasoning_effort");
    if let Some(Value::Object(reasoning)) = object.get_mut("reasoning") {
        reasoning.remove("effort");
        if reasoning.is_empty() {
            object.remove("reasoning");
        }
    }
    let Some(level) = level else {
        return;
    };
    if nested || path == "/v1/responses" {
        let reasoning = object
            .entry("reasoning")
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(reasoning) = reasoning {
            reasoning.insert("effort".into(), level.into());
        }
    } else {
        object.insert("reasoning_effort".into(), level.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        let mut c = Config::default();
        c.reasoning.models.insert(
            "r1".into(),
            ReasoningModel {
                efforts: vec!["low".into(), "medium".into(), "high".into()],
                default: "medium".into(),
            },
        );
        c.reasoning.models.insert(
            "mini".into(),
            ReasoningModel {
                efforts: vec!["medium".into(), "high".into()],
                default: String::new(),
            },
        );
        c.reasoning
            .models
            .insert("plain".into(), ReasoningModel::default());
        c
    }

    fn run(body: Value, path: &str, model: &str, free: bool, strict: bool) -> Result<Value> {
        let mut body = body;
        let original = requested(&body)?;
        apply(
            &mut body,
            path,
            &config(),
            model,
            original.as_ref(),
            free,
            strict,
        )?;
        Ok(body)
    }

    const CHAT: &str = "/v1/chat/completions";

    #[test]
    fn supported_levels_pass_and_defaults_fill() {
        let b = run(json!({"reasoning_effort":"high"}), CHAT, "R1", false, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"high"}));
        let b = run(json!({}), CHAT, "r1", false, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"medium"}));
        // The Responses API uses the nested form and keeps other fields.
        let b = run(
            json!({"reasoning":{"summary":"auto"}}),
            "/v1/responses",
            "r1",
            false,
            true,
        )
        .unwrap();
        assert_eq!(b, json!({"reasoning":{"summary":"auto","effort":"medium"}}));
    }

    #[test]
    fn unsupported_levels_are_rejected_or_mapped_for_fallbacks() {
        let e = run(json!({"reasoning_effort":"xhigh"}), CHAT, "r1", false, true).unwrap_err();
        assert_eq!(
            (e.status.as_u16(), e.code, e.param),
            (
                400,
                "unsupported_reasoning_effort",
                Some("reasoning_effort")
            )
        );
        let b = run(
            json!({"reasoning_effort":"xhigh"}),
            CHAT,
            "r1",
            false,
            false,
        )
        .unwrap();
        assert_eq!(b, json!({"reasoning_effort":"high"}));
        let b = run(
            json!({"reasoning":{"effort":"none"}}),
            CHAT,
            "r1",
            false,
            false,
        )
        .unwrap();
        assert_eq!(b, json!({"reasoning":{"effort":"low"}}));
        assert!(run(json!({"reasoning_effort":5}), CHAT, "r1", false, true).is_err());
    }

    #[test]
    fn models_without_reasoning_control_drop_the_parameter() {
        let b = run(
            json!({"reasoning_effort":"high","reasoning":{"effort":"low"}}),
            CHAT,
            "plain",
            false,
            true,
        )
        .unwrap();
        assert_eq!(b, json!({}));
    }

    #[test]
    fn free_accounts_are_capped() {
        // Lowered to the cap, and filled when absent.
        let b = run(json!({"reasoning_effort":"high"}), CHAT, "r1", true, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"low"}));
        let b = run(json!({}), CHAT, "r1", true, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"low"}));
        // Lower requests stay, and a model without "low" gets its lowest level.
        let b = run(json!({"reasoning_effort":"low"}), CHAT, "r1", true, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"low"}));
        let b = run(json!({}), CHAT, "mini", true, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"medium"}));
        // Unconfigured models: only an explicit higher level is lowered.
        let b = run(json!({"reasoning_effort":"max"}), CHAT, "other", true, true).unwrap();
        assert_eq!(b, json!({"reasoning_effort":"low"}));
        let b = run(
            json!({"reasoning_effort":"none"}),
            CHAT,
            "other",
            true,
            true,
        )
        .unwrap();
        assert_eq!(b, json!({"reasoning_effort":"none"}));
        assert_eq!(
            run(json!({}), CHAT, "other", true, true).unwrap(),
            json!({})
        );
    }
}

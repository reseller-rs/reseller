//! Tool catalogue, argument validation, and the account/signup tools.
use super::{Ctx, Output, ToolError, generate, internal};
use crate::{api, config::Config, error::Error, payments};
use axum::{body::Body, http::Method};
use serde_json::{Map, Value, json};

/// Credential a tool needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Auth {
    /// Anonymous callers may use it.
    None,
    /// API key or dashboard token.
    Any,
    /// API key only (the tool spends credit through the proxy).
    Key,
}

impl Auth {
    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Any => "api_key_or_dashboard_token",
            Self::Key => "api_key",
        }
    }
}

const TOOLS: [(&str, Auth); 17] = [
    ("generate_text", Auth::Key),
    ("generate_speech", Auth::Key),
    ("transcribe_audio", Auth::Key),
    ("generate_image", Auth::Key),
    ("list_models", Auth::Key),
    ("account_overview", Auth::Any),
    ("usage_summary", Auth::Any),
    ("list_requests", Auth::Any),
    ("list_keys", Auth::Any),
    ("create_key", Auth::Any),
    ("rotate_key", Auth::Any),
    ("block_key", Auth::Any),
    ("list_plans", Auth::None),
    ("redeem_code", Auth::Any),
    ("start_checkout", Auth::Any),
    ("signup_challenge", Auth::None),
    ("create_account", Auth::None),
];

fn auth_of(name: &str) -> Option<Auth> {
    TOOLS.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
}

pub(super) fn exists(name: &str) -> bool {
    auth_of(name).is_some()
}

/// Whether `name` must be called with an API key (not a dashboard token).
/// The stdio bridge uses this to pick the least-privileged credential.
pub fn requires_api_key(name: &str) -> bool {
    auth_of(name) == Some(Auth::Key)
}

/// Validated view of a tool's `arguments` object.
pub(super) struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    pub fn new(value: &'a Value, allowed: &[&str]) -> Result<Self, ToolError> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let map = match value {
            Value::Null => &*EMPTY,
            Value::Object(map) => map,
            _ => return Err(ToolError::invalid("arguments must be an object")),
        };
        if let Some(key) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(ToolError::invalid(format!("Unknown argument: {key}")));
        }
        Ok(Self(map))
    }

    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.0.get(key).filter(|v| !v.is_null())
    }

    /// Optional string of `min..=max` characters.
    pub fn str(&self, key: &str, min: usize, max: usize) -> Result<Option<&'a str>, ToolError> {
        let Some(v) = self.get(key) else {
            return Ok(None);
        };
        let s = v
            .as_str()
            .ok_or_else(|| ToolError::invalid(format!("{key} must be a string")))?;
        let n = s.chars().count();
        if !(min..=max).contains(&n) {
            return Err(ToolError::invalid(format!(
                "{key} must be {min} to {max} characters"
            )));
        }
        Ok(Some(s))
    }

    pub fn required_str(&self, key: &str, min: usize, max: usize) -> Result<&'a str, ToolError> {
        self.str(key, min, max)?
            .ok_or_else(|| ToolError::invalid(format!("{key} is required")))
    }

    pub fn int(&self, key: &str, min: i64, max: i64) -> Result<Option<i64>, ToolError> {
        let Some(v) = self.get(key) else {
            return Ok(None);
        };
        let n = v
            .as_i64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| {
                ToolError::invalid(format!("{key} must be an integer {min} to {max}"))
            })?;
        Ok(Some(n))
    }

    pub fn number(&self, key: &str, min: f64, max: f64) -> Result<Option<f64>, ToolError> {
        let Some(v) = self.get(key) else {
            return Ok(None);
        };
        let n = v
            .as_f64()
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| ToolError::invalid(format!("{key} must be a number {min} to {max}")))?;
        Ok(Some(n))
    }

    pub fn one_of(
        &self,
        key: &str,
        options: &[&'static str],
    ) -> Result<Option<&'static str>, ToolError> {
        let Some(s) = self.str(key, 1, 64)? else {
            return Ok(None);
        };
        options
            .iter()
            .copied()
            .find(|o| *o == s)
            .map(Some)
            .ok_or_else(|| {
                ToolError::invalid(format!("{key} must be one of: {}", options.join(", ")))
            })
    }

    /// A key id: letters, digits, `_` and `-` only (it becomes a URL segment).
    pub fn id(&self, key: &str) -> Result<&'a str, ToolError> {
        let s = self.required_str(key, 1, 64)?;
        if !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ToolError::invalid(format!("{key} is not a valid id")));
        }
        Ok(s)
    }
}

fn hints(read_only: bool, destructive: bool, idempotent: bool, open_world: bool) -> Value {
    if read_only {
        json!({"readOnlyHint": true, "openWorldHint": open_world})
    } else {
        json!({
            "readOnlyHint": false,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": open_world,
        })
    }
}

fn tool(name: &str, title: &str, description: &str, schema: Value, annotations: Value) -> Value {
    let auth = auth_of(name).expect("tool is registered");
    let requirement = match auth {
        Auth::None => "No authentication needed.",
        Auth::Any => "Requires authentication: an API key or a dashboard token.",
        Auth::Key => "Requires an API key (sk-res-...); dashboard tokens are rejected.",
    };
    json!({
        "name": name,
        "title": title,
        "description": format!("{description} {requirement}"),
        "inputSchema": schema,
        "annotations": annotations,
        "_meta": {"reseller/auth": auth.label()},
    })
}

fn object(properties: Value, required: &[&str]) -> Value {
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

const SECRET_WARNING: &str = "The credentials in the result are shown ONCE and cannot be retrieved later: store them securely and never repeat them in chat.";

/// Every tool with its schema, annotations and auth requirement.
pub(super) fn definitions() -> Vec<Value> {
    let audio_formats = ["mp3", "wav", "opus", "aac", "flac", "pcm"];
    vec![
        tool(
            "generate_text",
            "Generate text",
            "Generate text with a chat model. Provide either `prompt` or `messages`. \
             Returns the reply plus the billed cost, token usage and remaining balance.",
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 200_000, "description": "A single user message."},
                    "messages": {
                        "type": "array", "minItems": 1, "maxItems": 100,
                        "description": "A conversation; use instead of `prompt`.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "role": {"type": "string", "enum": ["system", "developer", "user", "assistant"]},
                                "content": {"type": "string", "maxLength": 200_000},
                            },
                            "required": ["role", "content"],
                            "additionalProperties": false,
                        },
                    },
                    "system": {"type": "string", "maxLength": 50_000, "description": "Optional system instructions placed first."},
                    "model": {"type": "string", "minLength": 1, "maxLength": 200, "description": "Model id; see list_models. Defaults to the server's model."},
                    "max_tokens": {"type": "integer", "minimum": 1, "maximum": 131_072},
                    "temperature": {"type": "number", "minimum": 0, "maximum": 2},
                },
                "anyOf": [{"required": ["prompt"]}, {"required": ["messages"]}],
                "additionalProperties": false,
            }),
            hints(false, false, false, true),
        ),
        tool(
            "generate_speech",
            "Generate speech",
            "Convert text to speech. Returns an audio content block (base64) and the billed cost.",
            object(
                json!({
                    "text": {"type": "string", "minLength": 1, "maxLength": 4096},
                    "voice": {"type": "string", "minLength": 1, "maxLength": 100},
                    "model": {"type": "string", "minLength": 1, "maxLength": 200},
                    "format": {"type": "string", "enum": audio_formats, "default": "mp3"},
                }),
                &["text"],
            ),
            hints(false, false, false, true),
        ),
        tool(
            "transcribe_audio",
            "Transcribe audio",
            "Transcribe speech to text. The audio is passed as base64 and is limited by the server's media size limit.",
            object(
                json!({
                    "audio_base64": {"type": "string", "minLength": 1, "description": "Base64 audio file contents (no data: prefix)."},
                    "filename": {"type": "string", "minLength": 1, "maxLength": 255, "description": "File name with extension, e.g. clip.mp3."},
                    "model": {"type": "string", "minLength": 1, "maxLength": 200},
                    "language": {"type": "string", "pattern": "^[a-z]{2,3}(-[A-Za-z]{2,4})?$", "description": "Optional ISO-639 language hint."},
                }),
                &["audio_base64", "filename"],
            ),
            hints(false, false, false, true),
        ),
        tool(
            "generate_image",
            "Generate images",
            "Generate images from a prompt. Inline results are returned as image content blocks; \
             if the provider returns hosted URLs they are returned as text links and never fetched by the server.",
            object(
                json!({
                    "prompt": {"type": "string", "minLength": 1, "maxLength": 4000},
                    "model": {"type": "string", "minLength": 1, "maxLength": 200},
                    "size": {"type": "string", "pattern": "^(auto|[0-9]{2,5}x[0-9]{2,5})$", "description": "For example 1024x1024."},
                    "n": {"type": "integer", "minimum": 1, "maximum": 4, "default": 1},
                }),
                &["prompt"],
            ),
            hints(false, false, false, true),
        ),
        tool(
            "list_models",
            "List models",
            "List the models available through this service.",
            object(json!({}), &[]),
            hints(true, false, true, true),
        ),
        tool(
            "account_overview",
            "Account overview",
            "Show the account balance, debt, active subscription and credit lots.",
            object(json!({}), &[]),
            hints(true, false, true, false),
        ),
        tool(
            "usage_summary",
            "Usage summary",
            "Spending and token usage over the last N days, optionally for one key.",
            object(
                json!({
                    "days": {"type": "integer", "minimum": 1, "maximum": 365, "default": 30},
                    "key_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$"},
                }),
                &[],
            ),
            hints(true, false, true, false),
        ),
        tool(
            "list_requests",
            "List requests",
            "Page through the account's request history with billed cost.",
            object(
                json!({
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
                    "offset": {"type": "integer", "minimum": 0, "maximum": 1_000_000, "default": 0},
                    "model": {"type": "string", "minLength": 1, "maxLength": 200},
                    "status": {"type": "string", "enum": ["2xx", "4xx", "5xx", "pending"]},
                    "kind": {"type": "string", "enum": ["llm", "tts", "stt", "image"]},
                }),
                &[],
            ),
            hints(true, false, true, false),
        ),
        tool(
            "list_keys",
            "List keys",
            "List the account's API keys (prefixes and limits only, never secrets).",
            object(json!({}), &[]),
            hints(true, false, true, false),
        ),
        tool(
            "create_key",
            "Create child key",
            &format!("Create another API key on this account. {SECRET_WARNING}"),
            object(
                json!({"name": {"type": "string", "minLength": 1, "maxLength": 80}}),
                &["name"],
            ),
            hints(false, false, false, false),
        ),
        tool(
            "rotate_key",
            "Rotate key",
            &format!(
                "Replace a key with a new one and block the old one. Keys younger than the \
                 cooldown cannot be rotated. {SECRET_WARNING}"
            ),
            object(
                json!({"key_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$"}}),
                &["key_id"],
            ),
            hints(false, true, false, false),
        ),
        tool(
            "block_key",
            "Block key",
            "Block a key so it can no longer call the API. Customers cannot unblock keys.",
            object(
                json!({"key_id": {"type": "string", "pattern": "^[A-Za-z0-9_-]{1,64}$"}}),
                &["key_id"],
            ),
            hints(false, true, true, false),
        ),
        tool(
            "list_plans",
            "List plans",
            "List purchasable plans and available payment methods.",
            object(json!({}), &[]),
            hints(true, false, true, false),
        ),
        tool(
            "redeem_code",
            "Redeem code",
            "Redeem a single-use credit or plan code.",
            object(
                json!({"code": {"type": "string", "minLength": 1, "maxLength": 200}}),
                &["code"],
            ),
            hints(false, false, false, false),
        ),
        tool(
            "start_checkout",
            "Start checkout",
            "Start buying a plan. Returns a payment URL (card) or crypto payment instructions; \
             the credit arrives once the payment is confirmed.",
            object(
                json!({
                    "plan": {"type": "string", "minLength": 1, "maxLength": 100, "description": "Plan code from list_plans."},
                    "method": {"type": "string", "enum": [payments::STRIPE, payments::CRYPTO]},
                }),
                &["plan"],
            ),
            hints(false, false, false, true),
        ),
        tool(
            "signup_challenge",
            "Get signup challenge",
            "Step 1 of creating an account: returns an ALTCHA proof-of-work challenge \
             ({algorithm, challenge, maxnumber, salt, signature}). Find the integer `number` in 0..=maxnumber \
             with hex(sha256(salt + decimal(number))) == challenge, then pass base64(JSON{algorithm, challenge, number, salt, signature}) \
             as `altcha` to create_account. Each solution is single use and expires. \
             Returns {\"required\": false} when no proof is needed.",
            object(json!({}), &[]),
            hints(true, false, false, false),
        ),
        tool(
            "create_account",
            "Create account",
            &format!(
                "Step 2 of creating an account: submit the solved challenge to receive a new account with \
                 free trial credit, an API key and a dashboard token. {SECRET_WARNING}"
            ),
            object(
                json!({
                    "altcha": {"type": "string", "maxLength": 2048, "description": "Base64 solution payload; omit when the challenge says it is not required."},
                    "name": {"type": "string", "maxLength": 80},
                    "contact": {"type": "string", "maxLength": 200, "description": "Optional contact such as an email address."},
                }),
                &[],
            ),
            hints(false, false, false, false),
        ),
    ]
}

/// The `instructions` text returned by `initialize`.
pub(super) fn instructions(config: &Config) -> String {
    let base = config.public_base();
    let signup = if config.signup.enabled {
        format!(
            "New accounts receive ${} of trial credit valid for {} days.",
            config.signup.free_credit_usd, config.signup.free_credit_expiry_days
        )
    } else {
        "Self-service signup is disabled; ask the operator for credentials.".to_owned()
    };
    format!(
        "{name} is a metered AI API (text, speech, transcription, images) at {base}.\n\
         \n\
         Authentication: send `Authorization: Bearer <token>`. An API key (sk-res-...) can use every tool; \
         a dashboard token (dash_...) can manage the account but cannot generate content. \
         Without a token, call signup_challenge, solve the proof of work, then create_account \
         (or use the local `reseller mcp` bridge, which does this for you). Credentials are shown once: store them securely.\n\
         \n\
         Tools: generate_text, generate_speech, transcribe_audio, generate_image, list_models (API key); \
         account_overview, usage_summary, list_requests, list_keys, create_key, rotate_key, block_key, \
         redeem_code, start_checkout (any credential); list_plans, signup_challenge, create_account (public).\n\
         \n\
         Pricing: usage is billed in USD against prepaid credit; every generation result reports its \
         billed cost, tokens and the remaining balance. {signup} Use list_plans and start_checkout to add credit; \
         insufficient_balance errors mean the balance is exhausted. Rate limits return an error with a retry delay.",
        name = config.branding.name,
    )
}

fn require(ctx: &Ctx, name: &str) -> Result<(), ToolError> {
    match (auth_of(name), &ctx.identity) {
        (Some(Auth::Any | Auth::Key), None) => Err(ToolError::new(
            "authentication_required",
            "This tool needs credentials. To create an account call signup_challenge, solve the \
             proof of work and call create_account; to use an existing account configure the MCP \
             client to send `Authorization: Bearer <API key or dashboard token>`. \
             (The local `reseller mcp` bridge offers signup and login tools.)",
        )),
        (Some(Auth::Key), Some(identity)) if identity.dashboard => Err(ToolError::new(
            "dashboard_token_not_allowed",
            "Dashboard tokens cannot generate content. Authenticate with an API key (sk-res-...); \
             create one with create_key if needed.",
        )),
        _ => Ok(()),
    }
}

/// Runs a tool.
pub(super) async fn call(ctx: &Ctx, name: &str, arguments: &Value) -> Result<Output, ToolError> {
    require(ctx, name)?;
    match name {
        "generate_text" => generate::text(ctx, arguments).await,
        "generate_speech" => generate::speech(ctx, arguments).await,
        "transcribe_audio" => generate::transcribe(ctx, arguments).await,
        "generate_image" => generate::image(ctx, arguments).await,
        "list_models" => generate::models(ctx, arguments).await,
        _ => account(ctx, name, arguments).await,
    }
}

/// Calls a customer API route in-process (same code path as the HTTP API).
async fn customer(
    ctx: &Ctx,
    method: Method,
    uri: &str,
    body: Option<&Value>,
) -> Result<Value, ToolError> {
    let (content_type, body) = match body {
        Some(b) => (
            Some("application/json"),
            Body::from(serde_json::to_vec(b).map_err(Error::internal)?),
        ),
        None => (None, Body::empty()),
    };
    let req = internal(method, uri, ctx.bearer.as_deref(), content_type, body)?;
    Ok(api::customer(&ctx.app, req, &ctx.ip).await?)
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        if let Some(v) = v {
            serializer.append_pair(k, v);
        }
    }
    serializer.finish()
}

async fn account(ctx: &Ctx, name: &str, arguments: &Value) -> Result<Output, ToolError> {
    let value = match name {
        "account_overview" => {
            Args::new(arguments, &[])?;
            customer(ctx, Method::GET, "/v1/account", None).await?
        }
        "usage_summary" => {
            let a = Args::new(arguments, &["days", "key_id"])?;
            let key = a.get("key_id").map(|_| a.id("key_id")).transpose()?;
            let q = query(&[
                ("days", a.int("days", 1, 365)?.map(|d| d.to_string())),
                ("key", key.map(str::to_owned)),
            ]);
            customer(ctx, Method::GET, &format!("/v1/account/usage?{q}"), None).await?
        }
        "list_requests" => {
            let a = Args::new(arguments, &["limit", "offset", "model", "status", "kind"])?;
            let q = query(&[
                (
                    "limit",
                    Some(a.int("limit", 1, 100)?.unwrap_or(20).to_string()),
                ),
                (
                    "offset",
                    a.int("offset", 0, 1_000_000)?.map(|n| n.to_string()),
                ),
                ("model", a.str("model", 1, 200)?.map(str::to_owned)),
                (
                    "status",
                    a.one_of("status", &["2xx", "4xx", "5xx", "pending"])?
                        .map(str::to_owned),
                ),
                (
                    "kind",
                    a.one_of("kind", &["llm", "tts", "stt", "image"])?
                        .map(str::to_owned),
                ),
            ]);
            customer(ctx, Method::GET, &format!("/v1/account/requests?{q}"), None).await?
        }
        "list_keys" => {
            Args::new(arguments, &[])?;
            customer(ctx, Method::GET, "/v1/keys", None).await?
        }
        "create_key" => {
            let a = Args::new(arguments, &["name"])?;
            let body = json!({"name": a.required_str("name", 1, 80)?});
            customer(ctx, Method::POST, "/v1/keys", Some(&body)).await?
        }
        "rotate_key" => {
            let a = Args::new(arguments, &["key_id"])?;
            let uri = format!("/v1/keys/{}/rotate", a.id("key_id")?);
            customer(ctx, Method::POST, &uri, None).await?
        }
        "block_key" => {
            let a = Args::new(arguments, &["key_id"])?;
            let uri = format!("/v1/keys/{}", a.id("key_id")?);
            customer(
                ctx,
                Method::PATCH,
                &uri,
                Some(&json!({"status": "blocked"})),
            )
            .await?
        }
        "list_plans" => {
            Args::new(arguments, &[])?;
            customer(ctx, Method::GET, "/v1/plans", None).await?
        }
        "redeem_code" => {
            let a = Args::new(arguments, &["code"])?;
            let body = json!({"code": a.required_str("code", 1, 200)?});
            customer(ctx, Method::POST, "/v1/account/redeem", Some(&body)).await?
        }
        "start_checkout" => {
            let a = Args::new(arguments, &["plan", "method"])?;
            let mut body = json!({"plan_code": a.required_str("plan", 1, 100)?});
            if let Some(method) = a.one_of("method", &[payments::STRIPE, payments::CRYPTO])? {
                body["method"] = method.into();
            }
            customer(ctx, Method::POST, "/v1/account/checkout", Some(&body)).await?
        }
        "signup_challenge" => {
            Args::new(arguments, &[])?;
            customer(ctx, Method::GET, "/v1/signup/challenge", None).await?
        }
        "create_account" => {
            let a = Args::new(arguments, &["altcha", "name", "contact"])?;
            let mut body = json!({});
            for (key, max) in [("altcha", 2048), ("name", 80), ("contact", 200)] {
                if let Some(v) = a.str(key, 0, max)? {
                    body[key] = v.into();
                }
            }
            customer(ctx, Method::POST, "/v1/signup", Some(&body)).await?
        }
        other => {
            return Err(ToolError::new(
                "unknown_tool",
                format!("Unknown tool: {other}"),
            ));
        }
    };
    Ok(Output::json(value))
}

//! `reseller mcp`: a local stdio MCP server that talks to a remote reseller
//! over HTTPS.
//!
//! It runs on the agent's machine so credentials and the signup proof of
//! work stay local. Transport is newline-delimited JSON-RPC on stdin/stdout
//! (diagnostics go to stderr only). `tools/list` and `tools/call` are
//! forwarded to the remote `/mcp` endpoint with the stored credential; a few
//! local tools (`signup`, `login`, `logout`, `whoami`, `reveal_credentials`)
//! manage that credential. Audio and image results are also saved to disk.
use super::{Output, SUPPORTED_VERSIONS, ToolError, negotiate, requires_api_key, tools::Args};
use crate::security::pow::{self, Challenge};
use anyhow::{Context, bail};
use axum::http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
    task::JoinSet,
};

/// Remote tools replaced by the local `signup` tool.
const HIDDEN_REMOTE_TOOLS: [&str; 2] = ["signup_challenge", "create_account"];
/// Remote tools that work without credentials.
const PUBLIC_REMOTE_TOOLS: [&str; 1] = ["list_plans"];
const MASK: &str = "<stored locally; use reveal_credentials>";
const MAX_REMOTE_RESPONSE: u64 = 512 * 1024 * 1024;

/// Credentials for one server.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Secrets never appear in logs, even through `{:?}`.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("key_prefix", &self.key_prefix)
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

/// The credential file: a JSON object keyed by server origin.
#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_all(&self) -> anyhow::Result<BTreeMap<String, Credentials>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!("{} is not a valid credentials file", self.path.display())
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    pub fn load(&self, origin: &str) -> anyhow::Result<Option<Credentials>> {
        Ok(self.read_all()?.remove(origin))
    }

    pub fn save(&self, origin: &str, credentials: Credentials) -> anyhow::Result<()> {
        let mut all = self.read_all()?;
        all.insert(origin.to_owned(), credentials);
        self.write_all(&all)
    }

    /// Returns whether an entry existed.
    pub fn remove(&self, origin: &str) -> anyhow::Result<bool> {
        let mut all = self.read_all()?;
        let existed = all.remove(origin).is_some();
        if existed {
            self.write_all(&all)?;
        }
        Ok(existed)
    }

    /// Writes through a private temporary file and renames it into place,
    /// so a crash never leaves a truncated file and the content is never
    /// readable by other users.
    fn write_all(&self, all: &BTreeMap<String, Credentials>) -> anyhow::Result<()> {
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        let name = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("credentials.json");
        let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> anyhow::Result<()> {
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(all)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.with_context(|| format!("writing {}", self.path.display()))
    }
}

/// `$XDG_CONFIG_HOME/reseller/credentials.json`, else
/// `~/.config/reseller/credentials.json`.
pub fn default_credentials_path() -> anyhow::Result<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = match var("XDG_CONFIG_HOME").filter(|p| p.is_absolute()) {
        Some(xdg) => xdg,
        None => var("HOME")
            .context("cannot locate a config directory; pass --credentials")?
            .join(".config"),
    };
    Ok(base.join("reseller").join("credentials.json"))
}

/// Validates the server URL and returns its origin. Plain HTTP is only
/// accepted for loopback addresses.
pub fn server_origin(raw: &str) -> anyhow::Result<String> {
    let url = url::Url::parse(raw).with_context(|| format!("invalid server URL {raw:?}"))?;
    let local = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    };
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        "http" => {
            bail!("refusing plain HTTP to {raw}: use https (http is allowed only for localhost)")
        }
        other => bail!("unsupported URL scheme {other:?}"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("the server URL must not contain credentials");
    }
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        bail!("the server URL must be an origin such as https://example.com");
    }
    Ok(url.origin().ascii_serialization())
}

pub struct Options {
    /// Server origin, e.g. `https://reseller.example.com`.
    pub url: String,
    /// Credential file; `None` selects the default location.
    pub credentials: Option<PathBuf>,
    pub output_dir: PathBuf,
    /// A credential used without being stored.
    pub token: Option<String>,
}

pub struct Bridge {
    client: reqwest::Client,
    origin: String,
    store: Store,
    output_dir: PathBuf,
    token: Option<String>,
    /// Serialises credential-file updates within this process.
    writes: Mutex<()>,
}

fn is_secret(token: &str, prefix: &str) -> bool {
    token
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Non-secret head of a credential, for display.
fn display_prefix(token: &str) -> String {
    let len = if token.starts_with("dash_") { 13 } else { 15 };
    token.chars().take(len).collect()
}

fn tool_result(output: Output) -> Value {
    output.result()
}

impl Bridge {
    pub fn new(options: Options) -> anyhow::Result<Self> {
        let origin = server_origin(&options.url)?;
        let path = match options.credentials {
            Some(p) => p,
            None => default_credentials_path()?,
        };
        if let Some(token) = &options.token
            && !is_secret(token, "sk-res-")
            && !is_secret(token, "dash_")
        {
            bail!("--token must be an sk-res-... API key or a dash_... dashboard token");
        }
        let output_dir = std::path::absolute(&options.output_dir)
            .with_context(|| format!("resolving {}", options.output_dir.display()))?;
        let client = reqwest::Client::builder()
            // A redirect must never carry the bearer token elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(330))
            .user_agent(concat!("reseller-mcp-bridge/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            origin,
            store: Store::new(path),
            output_dir,
            token: options.token,
            writes: Mutex::new(()),
        })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// One HTTP exchange with the server; returns the parsed JSON body.
    async fn send(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, ToolError> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.origin))
            .header("accept", "application/json, text/event-stream");
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|e| {
            ToolError::new(
                "network_error",
                format!("Could not reach {}: {}", self.origin, e.without_url()),
            )
        })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        if response
            .content_length()
            .is_some_and(|n| n > MAX_REMOTE_RESPONSE)
        {
            return Err(ToolError::new(
                "response_too_large",
                "The server response is too large",
            ));
        }
        let bytes = response.bytes().await.map_err(|e| {
            ToolError::new(
                "network_error",
                format!("Reading the response failed: {}", e.without_url()),
            )
        })?;
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        if status.is_success() {
            return parsed.ok_or_else(|| {
                ToolError::new(
                    "invalid_response",
                    "The server returned a non-JSON response",
                )
            });
        }
        let error = parsed.as_ref().map(|v| &v["error"]);
        let code = match error.map(|e| &e["code"]) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => format!("http_{}", status.as_u16()),
        };
        let mut message = error
            .and_then(|e| e["message"].as_str())
            .map_or_else(|| format!("HTTP {}", status.as_u16()), str::to_owned);
        if status == 401 {
            message.push_str(" Run the login or signup tool, or check --token / RESELLER_TOKEN.");
        }
        Err(ToolError {
            code,
            message,
            retry_after,
        })
    }

    /// A JSON-RPC call to the remote `/mcp` endpoint; returns its `result`.
    async fn rpc(
        &self,
        bearer: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, ToolError> {
        let message = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut response = self
            .send(Method::POST, "/mcp", bearer, Some(&message))
            .await?;
        if let Some(error) = response.get("error") {
            return Err(ToolError::new(
                &error["code"].to_string(),
                error["message"].as_str().unwrap_or("MCP error"),
            ));
        }
        Ok(response["result"].take())
    }

    /// Credentials in effect: `--token`, else the stored entry.
    fn effective(&self) -> anyhow::Result<Option<Credentials>> {
        if let Some(token) = &self.token {
            let mut c = Credentials::default();
            if token.starts_with("dash_") {
                c.dashboard_token = Some(token.clone());
            } else {
                c.api_key = Some(token.clone());
            }
            return Ok(Some(c));
        }
        self.store.load(&self.origin)
    }

    /// The least-privileged credential that can run `tool`.
    fn bearer_for(credentials: &Credentials, tool: &str) -> Option<String> {
        let (first, second) = if requires_api_key(tool) {
            (&credentials.api_key, &credentials.dashboard_token)
        } else {
            (&credentials.dashboard_token, &credentials.api_key)
        };
        first.clone().or_else(|| second.clone())
    }

    /// Handles one JSON-RPC message; `None` for notifications and responses.
    pub async fn handle(&self, message: Value) -> Option<Value> {
        let reply = |id: Value, outcome: Result<Value, (i64, String)>| {
            Some(match outcome {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                }
            })
        };
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            return reply(
                Value::Null,
                Err((
                    -32600,
                    "Expected a JSON-RPC 2.0 message (batches are not supported)".into(),
                )),
            );
        }
        let id = message.get("id")?.clone();
        let Some(method) = message["method"].as_str() else {
            return reply(id, Err((-32600, "Missing method".into())));
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let Some(requested) = params["protocolVersion"].as_str() else {
                    return reply(id, Err((-32602, "protocolVersion is required".into())));
                };
                reply(id, Ok(self.initialize(requested).await))
            }
            "ping" => reply(id, Ok(json!({}))),
            "tools/list" => reply(id, Ok(json!({"tools": self.list_tools().await}))),
            "tools/call" => {
                let Some(name) = params["name"].as_str() else {
                    return reply(id, Err((-32602, "name is required".into())));
                };
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                match self.call_tool(name, &arguments).await {
                    Ok(Some(result)) => reply(id, Ok(result)),
                    Ok(None) => reply(id, Err((-32602, format!("Unknown tool: {name}")))),
                    Err(e) => reply(id, Ok(e.result())),
                }
            }
            other => reply(id, Err((-32601, format!("Method not found: {other}")))),
        }
    }

    async fn initialize(&self, requested: &str) -> Value {
        let mut instructions = format!(
            "This is the local reseller bridge for {}. Use `signup` to create an account (the proof of \
             work is solved locally) or `login` to use existing credentials; secrets stay in a \
             private local file and are never shown unless you call `reveal_credentials`. \
             Generated audio and images are also saved to {}.",
            self.origin,
            self.output_dir.display()
        );
        let remote = self
            .rpc(
                None,
                "initialize",
                json!({
                    "protocolVersion": negotiate(Some(requested)),
                    "capabilities": {},
                    "clientInfo": {"name": "reseller-mcp-bridge", "version": env!("CARGO_PKG_VERSION")},
                }),
            )
            .await;
        match remote {
            Ok(result) => {
                if let Some(text) = result["instructions"].as_str() {
                    instructions.push_str("\n\nServer instructions:\n");
                    instructions.push_str(text);
                }
            }
            Err(e) => tracing::warn!("remote initialize failed: {}", e.message),
        }
        json!({
            "protocolVersion": negotiate(Some(requested)),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "reseller-mcp-bridge",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": instructions,
        })
    }

    async fn list_tools(&self) -> Vec<Value> {
        let mut tools = local_tools();
        match self.rpc(None, "tools/list", json!({})).await {
            Ok(result) => {
                if let Some(remote) = result["tools"].as_array() {
                    tools.extend(
                        remote
                            .iter()
                            .filter(|t| {
                                !t["name"]
                                    .as_str()
                                    .is_some_and(|n| HIDDEN_REMOTE_TOOLS.contains(&n))
                            })
                            .cloned(),
                    );
                }
            }
            Err(e) => tracing::warn!("remote tools/list failed: {}", e.message),
        }
        tools
    }

    /// `Ok(None)` when the tool is unknown locally and the remote says so.
    async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Option<Value>, ToolError> {
        let local = match name {
            "signup" => Some(self.signup(arguments).await),
            "login" => Some(self.login(arguments).await),
            "logout" => Some(self.logout(arguments).await),
            "whoami" => Some(self.whoami(arguments).await),
            "reveal_credentials" => Some(self.reveal(arguments)),
            _ => None,
        };
        if let Some(output) = local {
            return output.map(|o| Some(tool_result(o)));
        }
        if HIDDEN_REMOTE_TOOLS.contains(&name) {
            return Ok(None);
        }
        let credentials = self
            .effective()
            .map_err(|e| ToolError::new("credentials_unreadable", format!("{e:#}")))?;
        let bearer = credentials.as_ref().and_then(|c| Self::bearer_for(c, name));
        if bearer.is_none() && !PUBLIC_REMOTE_TOOLS.contains(&name) {
            return Err(ToolError::new(
                "not_signed_in",
                "Not signed in. Call the signup tool to create an account, or the login tool \
                 to use an existing API key or dashboard token.",
            ));
        }
        let params = json!({"name": name, "arguments": arguments});
        let mut result = match self.rpc(bearer.as_deref(), "tools/call", params).await {
            Ok(result) => result,
            // An unknown remote tool is a protocol error for the caller.
            Err(e) if e.code == "-32602" => return Ok(None),
            Err(e) => return Err(e),
        };
        self.after_call(name, arguments, &mut result).await;
        Ok(Some(result))
    }

    /// Post-processing of a remote tool result: stores rotated credentials
    /// and saves media to disk.
    async fn after_call(&self, name: &str, arguments: &Value, result: &mut Value) {
        if name == "rotate_key" && result["isError"] != true {
            self.adopt_rotation(arguments, result).await;
        }
        let Some(blocks) = result["content"].as_array_mut() else {
            return;
        };
        let mut saved = Vec::new();
        for block in blocks.iter() {
            let kind = block["type"].as_str().unwrap_or_default();
            if !matches!(kind, "image" | "audio") {
                continue;
            }
            let (Some(data), Some(mime)) = (block["data"].as_str(), block["mimeType"].as_str())
            else {
                continue;
            };
            match self.save_media(kind, mime, data).await {
                Ok(path) => saved.push(format!("Saved {kind} to {}", path.display())),
                Err(e) => tracing::warn!("could not save {kind}: {e:#}"),
            }
        }
        for line in saved {
            blocks.push(json!({"type": "text", "text": line}));
        }
    }

    /// If the rotated key is the stored one, keeps the stored credentials
    /// current and hides the new secrets from the conversation.
    async fn adopt_rotation(&self, arguments: &Value, result: &mut Value) {
        let (Some(old), Some(new)) = (
            arguments["key_id"].as_str(),
            result["structuredContent"].as_object(),
        ) else {
            return;
        };
        let (Some(key), Some(dashboard), Some(id)) = (
            new.get("key").and_then(Value::as_str),
            new.get("dashboard_token").and_then(Value::as_str),
            new.get("id").and_then(Value::as_str),
        ) else {
            return;
        };
        let _guard = self.writes.lock().await;
        let Ok(Some(mut stored)) = self.store.load(&self.origin) else {
            return;
        };
        if stored.key_id.as_deref() != Some(old) {
            return;
        }
        stored.api_key = Some(key.to_owned());
        stored.dashboard_token = Some(dashboard.to_owned());
        stored.key_id = Some(id.to_owned());
        stored.key_prefix = Some(display_prefix(key));
        if let Err(e) = self.store.save(&self.origin, stored) {
            tracing::error!("could not store the rotated credentials: {e:#}");
            return;
        }
        let masked = json!({
            "id": id,
            "key_prefix": display_prefix(key),
            "note": "The rotated key replaced the stored credentials in the local credentials file.",
            "key": MASK,
            "dashboard_token": MASK,
        });
        result["structuredContent"] = masked.clone();
        result["content"] = json!([{"type": "text", "text": masked.to_string()}]);
    }

    async fn save_media(&self, kind: &str, mime: &str, data: &str) -> anyhow::Result<PathBuf> {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let bytes = STANDARD.decode(data).context("invalid base64")?;
        let extension = match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "audio/mpeg" => "mp3",
            "audio/wav" => "wav",
            "audio/ogg" => "ogg",
            "audio/aac" => "aac",
            "audio/flac" => "flac",
            "audio/pcm" => "pcm",
            _ => "bin",
        };
        tokio::fs::create_dir_all(&self.output_dir).await?;
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let path = self
            .output_dir
            .join(format!("{kind}-{millis}-{}.{extension}", &unique[..8]));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        Ok(path)
    }

    fn reject_token_override(&self, action: &str) -> Result<(), ToolError> {
        if self.token.is_some() {
            return Err(ToolError::new(
                "token_override_active",
                format!(
                    "A --token / RESELLER_TOKEN is set, so {action} is disabled; unset it first."
                ),
            ));
        }
        Ok(())
    }

    fn masked(&self, credentials: &Credentials, extra: Value) -> Output {
        let mut value = json!({
            "server": self.origin,
            "account_id": credentials.account_id,
            "key_prefix": credentials.key_prefix,
            "credentials_file": self.store.path().display().to_string(),
        });
        if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        Output::json(value)
    }

    async fn signup(&self, arguments: &Value) -> Result<Output, ToolError> {
        let a = Args::new(arguments, &["name", "contact"])?;
        let name = a.str("name", 0, 80)?;
        let contact = a.str("contact", 0, 200)?;
        self.reject_token_override("signup")?;
        let internal =
            |e: anyhow::Error| ToolError::new("credentials_unreadable", format!("{e:#}"));
        let _guard = self.writes.lock().await;
        if let Some(existing) = self.store.load(&self.origin).map_err(internal)? {
            return Err(ToolError::new(
                "already_signed_in",
                format!(
                    "Already signed in to {} (key {}). Call logout first to create another account.",
                    self.origin,
                    existing.key_prefix.as_deref().unwrap_or("unknown")
                ),
            ));
        }
        let challenge = self
            .send(Method::GET, "/v1/signup/challenge", None, None)
            .await?;
        let mut body = json!({});
        if let Some(name) = name {
            body["name"] = name.into();
        }
        if let Some(contact) = contact {
            body["contact"] = contact.into();
        }
        if challenge["required"] != false {
            let challenge: Challenge = serde_json::from_value(challenge).map_err(|_| {
                ToolError::new(
                    "invalid_response",
                    "The server sent an unrecognised challenge",
                )
            })?;
            let altcha = tokio::task::spawn_blocking(move || {
                pow::solve(&challenge).map(|n| pow::encode(&challenge, n))
            })
            .await
            .map_err(|e| ToolError::new("internal_error", e.to_string()))?
            .ok_or_else(|| {
                ToolError::new(
                    "pow_unsolved",
                    "The proof-of-work challenge had no solution",
                )
            })?;
            body["altcha"] = altcha.into();
        }
        let created = self
            .send(Method::POST, "/v1/signup", None, Some(&body))
            .await?;
        let field = |k: &str| created[k].as_str().map(str::to_owned);
        let credentials = Credentials {
            api_key: field("key"),
            dashboard_token: field("dashboard_token"),
            key_id: field("id"),
            key_prefix: field("key_prefix"),
            account_id: field("account_id"),
        };
        if credentials.api_key.is_none() || credentials.dashboard_token.is_none() {
            return Err(ToolError::new(
                "invalid_response",
                "The server returned no credentials",
            ));
        }
        self.store
            .save(&self.origin, credentials.clone())
            .map_err(|e| ToolError::new("credentials_unwritable", format!("{e:#}")))?;
        Ok(self.masked(
            &credentials,
            json!({
                "message": "Account created. Credentials were saved locally; secrets are not shown. \
                            Use whoami to see the balance.",
                "notice": created["notice"],
                "limits": created["limits"],
            }),
        ))
    }

    async fn login(&self, arguments: &Value) -> Result<Output, ToolError> {
        let a = Args::new(arguments, &["api_key", "dashboard_token"])?;
        self.reject_token_override("login")?;
        let api_key = a.str("api_key", 1, 200)?;
        let dashboard = a.str("dashboard_token", 1, 200)?;
        if api_key.is_none() && dashboard.is_none() {
            return Err(ToolError::invalid("Provide api_key and/or dashboard_token"));
        }
        if api_key.is_some_and(|k| !is_secret(k, "sk-res-")) {
            return Err(ToolError::invalid(
                "api_key must look like sk-res-<64 hex characters>",
            ));
        }
        if dashboard.is_some_and(|k| !is_secret(k, "dash_")) {
            return Err(ToolError::invalid(
                "dashboard_token must look like dash_<64 hex characters>",
            ));
        }
        let mut account = None;
        for token in [api_key, dashboard].into_iter().flatten() {
            let overview = self
                .send(Method::GET, "/v1/account", Some(token), None)
                .await?;
            let id = overview["account"]["id"].as_str().map(str::to_owned);
            if account.is_some() && account != id {
                return Err(ToolError::invalid(
                    "The API key and dashboard token belong to different accounts",
                ));
            }
            account = id;
        }
        let first = api_key.or(dashboard).unwrap_or_default();
        let key_prefix = display_prefix(first);
        // Best effort: learn the key id so rotations can update this file.
        let key_id = match self
            .send(
                Method::GET,
                &format!("/v1/keys?q={}", display_prefix(api_key.unwrap_or(first))),
                Some(first),
                None,
            )
            .await
        {
            Ok(keys) => keys["keys"]
                .as_array()
                .and_then(|list| {
                    list.iter()
                        .find(|k| k["key_prefix"].as_str() == Some(key_prefix.as_str()))
                })
                .and_then(|k| k["id"].as_str().map(str::to_owned)),
            Err(_) => None,
        };
        let credentials = Credentials {
            api_key: api_key.map(str::to_owned),
            dashboard_token: dashboard.map(str::to_owned),
            key_id,
            key_prefix: Some(key_prefix),
            account_id: account,
        };
        let _guard = self.writes.lock().await;
        self.store
            .save(&self.origin, credentials.clone())
            .map_err(|e| ToolError::new("credentials_unwritable", format!("{e:#}")))?;
        Ok(self.masked(
            &credentials,
            json!({"message": "Logged in. Credentials were verified and saved locally; secrets are not shown."}),
        ))
    }

    async fn logout(&self, arguments: &Value) -> Result<Output, ToolError> {
        Args::new(arguments, &[])?;
        self.reject_token_override("logout")?;
        let _guard = self.writes.lock().await;
        let existed = self
            .store
            .remove(&self.origin)
            .map_err(|e| ToolError::new("credentials_unwritable", format!("{e:#}")))?;
        Ok(Output::json(json!({
            "server": self.origin,
            "removed": existed,
            "message": if existed { "Stored credentials were deleted." } else { "No stored credentials for this server." },
        })))
    }

    async fn whoami(&self, arguments: &Value) -> Result<Output, ToolError> {
        Args::new(arguments, &[])?;
        let credentials = self
            .effective()
            .map_err(|e| ToolError::new("credentials_unreadable", format!("{e:#}")))?;
        let Some(credentials) = credentials else {
            return Ok(Output::json(json!({
                "server": self.origin,
                "signed_in": false,
                "message": "Not signed in. Use signup or login.",
            })));
        };
        let bearer = Self::bearer_for(&credentials, "account_overview").unwrap_or_default();
        let overview = self
            .send(Method::GET, "/v1/account", Some(&bearer), None)
            .await?;
        let kind = if bearer.starts_with("dash_") {
            "dashboard_token"
        } else {
            "api_key"
        };
        Ok(Output::json(json!({
            "server": self.origin,
            "signed_in": true,
            "credential": kind,
            "source": if self.token.is_some() { "token option" } else { "credentials file" },
            "key_prefix": credentials.key_prefix.clone().unwrap_or_else(|| display_prefix(&bearer)),
            "account_id": overview["account"]["id"],
            "balance_usd": overview["account"]["balance_usd"],
            "credentials_file": self.store.path().display().to_string(),
        })))
    }

    fn reveal(&self, arguments: &Value) -> Result<Output, ToolError> {
        Args::new(arguments, &[])?;
        let stored = self
            .store
            .load(&self.origin)
            .map_err(|e| ToolError::new("credentials_unreadable", format!("{e:#}")))?
            .ok_or_else(|| {
                ToolError::new("not_signed_in", "No stored credentials for this server.")
            })?;
        Ok(Output::json(json!({
            "server": self.origin,
            "api_key": stored.api_key,
            "dashboard_token": stored.dashboard_token,
            "warning": "These are secrets. Do not repeat them in chat, logs or files.",
        })))
    }
}

fn local_tools() -> Vec<Value> {
    let key = "^sk-res-[0-9a-f]{64}$";
    let dash = "^dash_[0-9a-f]{64}$";
    vec![
        json!({
            "name": "signup",
            "title": "Sign up",
            "description": "Create a new account on the server: solves the proof of work locally and saves the \
                            credentials to a private local file. Secrets are NOT shown (use reveal_credentials). \
                            Fails if already signed in.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string", "maxLength": 80},
                "contact": {"type": "string", "maxLength": 200, "description": "Optional contact such as an email address."},
            }, "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": true},
        }),
        json!({
            "name": "login",
            "title": "Log in",
            "description": "Use existing credentials: verifies them against the server and saves them to a private \
                            local file. Prefer the --token option or RESELLER_TOKEN environment variable so secrets \
                            never pass through the conversation. Secrets are not echoed back.",
            "inputSchema": {"type": "object", "properties": {
                "api_key": {"type": "string", "pattern": key},
                "dashboard_token": {"type": "string", "pattern": dash},
            }, "anyOf": [{"required": ["api_key"]}, {"required": ["dashboard_token"]}], "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true},
        }),
        json!({
            "name": "logout",
            "title": "Log out",
            "description": "Delete the locally stored credentials for this server. The account and keys are not affected.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": false},
        }),
        json!({
            "name": "whoami",
            "title": "Who am I",
            "description": "Show the server, key prefix, account id and balance of the current credentials. Never prints secrets.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "openWorldHint": true},
        }),
        json!({
            "name": "reveal_credentials",
            "title": "Reveal stored credentials",
            "description": "SENSITIVE: returns the stored API key and dashboard token in plain text. Only call when the \
                            user explicitly asks to see them, and never repeat them elsewhere.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true, "openWorldHint": false},
        }),
    ]
}

/// Runs the stdio server until stdin closes.
pub async fn run(options: Options) -> anyhow::Result<()> {
    let bridge = Arc::new(Bridge::new(options)?);
    tracing::info!(
        server = bridge.origin(),
        versions = SUPPORTED_VERSIONS.join(","),
        "reseller MCP bridge ready on stdio"
    );
    let (out, mut lines_out) = mpsc::channel::<String>(64);
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines_out.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err()
                || stdout.write_all(b"\n").await.is_err()
                || stdout.flush().await.is_err()
            {
                break;
            }
        }
    });
    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut tasks = JoinSet::new();
    while let Some(line) = input.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let out = out.clone();
        match serde_json::from_str::<Value>(&line) {
            Ok(message) => {
                let bridge = bridge.clone();
                tasks.spawn(async move {
                    if let Some(reply) = bridge.handle(message).await {
                        let _ = out.send(reply.to_string()).await;
                    }
                });
            }
            Err(_) => {
                let reply = json!({"jsonrpc": "2.0", "id": null,
                    "error": {"code": -32700, "message": "Parse error"}});
                let _ = out.send(reply.to_string()).await;
            }
        }
    }
    while tasks.join_next().await.is_some() {}
    drop(out);
    let _ = writer.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(key: &str) -> Credentials {
        Credentials {
            api_key: Some(key.into()),
            ..Credentials::default()
        }
    }

    #[test]
    fn store_is_keyed_by_origin_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("credentials.json");
        let store = Store::new(path.clone());
        assert_eq!(store.load("https://a.example").unwrap(), None);
        store.save("https://a.example", creds("one")).unwrap();
        store.save("https://b.example", creds("two")).unwrap();
        assert_eq!(
            store
                .load("https://a.example")
                .unwrap()
                .unwrap()
                .api_key
                .as_deref(),
            Some("one")
        );
        assert_eq!(
            store
                .load("https://b.example")
                .unwrap()
                .unwrap()
                .api_key
                .as_deref(),
            Some("two")
        );
        assert!(store.remove("https://a.example").unwrap());
        assert!(!store.remove("https://a.example").unwrap());
        assert_eq!(store.load("https://a.example").unwrap(), None);
        assert!(store.load("https://b.example").unwrap().is_some());
        let file: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(file["https://b.example"].is_object());
        // No temporary files are left behind.
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    #[test]
    fn corrupt_store_is_an_error_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, b"not json").unwrap();
        let store = Store::new(path.clone());
        assert!(store.load("https://a.example").is_err());
        assert!(store.save("https://a.example", creds("x")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"not json");
    }

    #[test]
    fn debug_output_hides_secrets() {
        let mut c = creds("sk-res-secret");
        c.dashboard_token = Some("dash_secret".into());
        let shown = format!("{c:?}");
        assert!(!shown.contains("secret"));
    }

    #[test]
    fn server_urls_must_be_https_or_loopback() {
        assert_eq!(
            server_origin("https://Example.com/").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            server_origin("http://localhost:8080").unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            server_origin("http://127.0.0.1:9").unwrap(),
            "http://127.0.0.1:9"
        );
        assert_eq!(server_origin("http://[::1]:9").unwrap(), "http://[::1]:9");
        for bad in [
            "http://example.com",
            "http://192.168.1.2",
            "ftp://example.com",
            "https://user:pw@example.com",
            "https://example.com/v1",
            "https://example.com/?a=b",
            "not a url",
        ] {
            assert!(server_origin(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn least_privileged_credential_is_chosen() {
        let both = Credentials {
            api_key: Some("k".into()),
            dashboard_token: Some("d".into()),
            ..Credentials::default()
        };
        assert_eq!(
            Bridge::bearer_for(&both, "generate_text").as_deref(),
            Some("k")
        );
        assert_eq!(
            Bridge::bearer_for(&both, "account_overview").as_deref(),
            Some("d")
        );
        assert_eq!(
            Bridge::bearer_for(&creds("k"), "account_overview").as_deref(),
            Some("k")
        );
    }
}

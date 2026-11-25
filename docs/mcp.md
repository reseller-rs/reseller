# MCP: reseller for AI agents

reseller speaks the [Model Context Protocol](https://modelcontextprotocol.io). An agent can generate text, speech, transcripts, and images, check its balance, and top up. A human does not need to write any HTTP client code.

Every generation is billed to the account like a normal `/v1` request. Both connection methods use the same admission, limits, metering, model policy, and prices as the HTTP API.

## Contents

- [Choose a connection](#choose-a-connection)
- [Remote endpoint](#remote-endpoint)
- [Connect with OAuth](#connect-with-oauth)
- [Local stdio bridge](#local-stdio-bridge-reseller-mcp)
- [Tools](#tools)
- [Signup and login walkthrough](#signup-and-login-walkthrough)
- [Error reference](#error-reference)
- [Security notes](#security-notes)

## Choose a connection

| | Remote endpoint | Local stdio bridge |
|---|---|---|
| Command | None. The client calls `POST /mcp`. | `reseller mcp --url https://HOST` |
| Credentials | OAuth sign-in in the browser, or an `Authorization: Bearer` header you configure. | Stored in a private local file. The agent never needs to see them. |
| Signup | In the browser, on the OAuth consent page. | Built in. The `signup` tool solves the proof of work locally. |
| Generated media | Returned inline only | Returned inline and saved to disk |
| Best for | Claude, ChatGPT, Cursor, VS Code, and hosted agents | Desktop and coding agents (the more secure option) |

## Remote endpoint

`POST https://HOST/mcp` is a stateless Streamable HTTP server.

- Each POST carries one JSON-RPC 2.0 message and gets an `application/json` reply.
- There are no sessions, no SSE stream, and no `Mcp-Session-Id`. The `Mcp-Session-Id` and `Last-Event-ID` request headers are ignored.
- JSON-RPC batches are rejected. Send one message per request.

### Authentication

| Credential | Tools available |
|---|---|
| `Authorization: Bearer sk-res-...` (API key) | All tools |
| `Authorization: Bearer dash_...` (dashboard token) | Account tools only (see [Tools](#tools)) |
| `Authorization: Bearer mcp_at_...` (OAuth access token) | All tools, like the API key it was authorized with. Valid on `/mcp` only. |
| `x-api-key` header | Same as the bearer API key |
| `Authorization: Bearer <administrator token>` | Operator: `generate_text`, `generate_speech`, `transcribe_audio`, `generate_image`, `list_models`, and `list_plans`. Nothing is billed, and requests are logged without an account. Account tools return the tool error `admin_token_not_customer`. Use `/admin` to manage accounts. |
| None | Refused. There is no anonymous mode. |

A request without a credential gets `401` with `WWW-Authenticate: Bearer resource_metadata="https://HOST/.well-known/oauth-protected-resource/mcp", scope="mcp"`, which tells an OAuth-capable client where to sign in. A rejected credential gets the same challenge plus `error="invalid_token"`. With `[mcp].oauth = false` the challenge has no `resource_metadata`.

### Connect with OAuth

Add `https://HOST/mcp` as a remote MCP server in Claude, ChatGPT, Cursor, or VS Code. No header or key is needed. The client discovers the authorization server, registers itself, and opens a browser page on this server. There you sign in with an API key or dashboard token, or create a new account, then allow or deny the connection. The client receives a one-hour access token and a refresh token (valid 90 days idle, rotated on each use). It refreshes silently.

- The grant is bound to the key you signed in with. Tokens stop working as soon as that key is blocked, revoked, or expired, or the account is suspended. Customers list and revoke connected apps in the dashboard (`GET /v1/account/apps`, `DELETE /v1/account/apps/{id}`).
- OAuth access tokens are accepted on `/mcp` only, never on `/v1/*` or the customer routes.
- Standards: authorization code flow with PKCE `S256`, dynamic public-client registration (RFC 7591), authorization server metadata (RFC 8414), protected resource metadata (RFC 9728), resource indicators (RFC 8707), revocation (RFC 7009). Metadata lives at `/.well-known/oauth-protected-resource[/mcp]` and `/.well-known/oauth-authorization-server`. The only scope is `mcp`.
- Redirect URIs may be `https`, `http` on `localhost`, `127.0.0.1`, or `[::1]`, or a private-use scheme such as `cursor://` and `vscode://`. Clients that cannot use OAuth send a static bearer token instead:

```sh
claude mcp add --transport http reseller https://HOST/mcp \
  --header "Authorization: Bearer sk-res-..."
```

Set `[mcp].oauth = false` to turn the OAuth endpoints off. Static bearer tokens keep working.

### Protocol versions

| Version | Status | Notes |
|---|---|---|
| `2026-07-28` | Current, stateless | Per-request `_meta`, no handshake |
| `2025-11-25` | Legacy | `initialize` handshake |
| `2025-06-18`, `2025-03-26` | Not served | Rejected with `-32022` |

`server/discover` lists the supported versions as `["2026-07-28", "2025-11-25"]`.

### Methods

| Era | Methods | Notes |
|---|---|---|
| 2026-07-28 | `server/discover`, `tools/list`, `tools/call` | Capabilities are `{"tools": {}}`. The tool list never changes at runtime, so there is no list-changed stream. |
| 2025-11-25 | `initialize`, `ping`, `tools/list`, `tools/call` | Results have the 2025-11-25 shape, with no `resultType`, caching hints, or `_meta` identity. |

Other methods are not served. This includes `resources/*`, `prompts/*`, and `subscriptions/listen`. Modern requests to these methods answer `404` with `-32601`.

Notifications (messages without an `id`) get `202 Accepted`. `GET` and `DELETE` return `405`.

### Modern requests (2026-07-28)

Every request is self-describing. There is no handshake and no state between requests.

- **Headers:**
  - `MCP-Protocol-Version: 2026-07-28`
  - `Mcp-Method`, which carries the JSON-RPC `method`
  - `Mcp-Name`, which is sent for `tools/call` and carries the tool name. Values that are not plain ASCII are sent as `=?base64?<base64>?=`.
  - Header names are case-insensitive.
- **Header check:** a missing header, or one that disagrees with the body, is rejected before anything runs. The response is `400` with JSON-RPC error `-32020` (HeaderMismatch).
- **`params._meta`:**
  - `io.modelcontextprotocol/protocolVersion` is required and must equal the header.
  - `io.modelcontextprotocol/clientCapabilities` is required. An empty object `{}` is fine.
  - `io.modelcontextprotocol/clientInfo` (`{name, version}`) is recommended.
  - A missing or malformed required field gives `400` with `-32602`.
- **Client capabilities:** the server never asks the client for anything, so it never returns `-32021`.
- **Unsupported version:** the response is `400` with `-32022` and `error.data = {"supported": ["2026-07-28", "2025-11-25"], "requested": "<version>"}`. Retry with a supported version.
- **Results:** every result carries `resultType: "complete"` and `_meta["io.modelcontextprotocol/serverInfo"]` (`{name, title, version}`).
- **Caching:** `server/discover` and `tools/list` also carry `ttlMs` (300000) and `cacheScope: "public"`. The tool list is the same for every caller, whatever the credential.
- **Pagination:** `tools/list` is a single page. A `cursor` parameter gives `-32602`.

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_plans","arguments":{},
 "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
          "io.modelcontextprotocol/clientInfo":{"name":"my-agent","version":"1.0"},
          "io.modelcontextprotocol/clientCapabilities":{}}}}
```

### Legacy requests (2025-11-25)

A request without modern `_meta` is served with 2025-11-25 semantics.

1. Send `initialize` without the `MCP-Protocol-Version` header. The result always names `2025-11-25`: a client that requested another revision continues with it or disconnects, as the 2025-11-25 lifecycle says. An `initialize` without `protocolVersion` is refused with `400` and `-32602`.
2. Send `MCP-Protocol-Version: 2025-11-25` on every later request. A request without the header, or with another unsupported version, is refused with `-32020` or `-32022`.

Nothing is stored on the server. No session is created.

Clients that support both eras should send a modern request first. They should fall back to `initialize` only when a `400` carries none of the modern error codes (`-32020`, `-32021`, or `-32022`).

### Limits

| Limit | Behaviour |
|---|---|
| Management rate | Counts against `rate_limits.management_per_minute` per client IP. Returns `429 management_rate_limited`. |
| Auth failures | Invalid credentials count against `rate_limits.auth_failures_per_hour`. Exhausting it returns `429 too_many_auth_failures` for that IP. |
| Message size | At most `mcp.max_media_bytes x 4/3 + 256 KiB`. Larger messages get `413 request_too_large`. |
| Generated media | Audio or images larger than `mcp.max_media_bytes` (default 16 MiB, up to 256 MiB) are refused with `response_too_large`. The request may still have been billed. |
| Disabled | `[mcp].enabled = false` answers `404 mcp_disabled`. |

### Client configuration

**Claude Code** (remote, with an existing key):

```sh
claude mcp add --transport http reseller https://HOST/mcp \
  --header "Authorization: Bearer sk-res-..."
```

**Project `.mcp.json`** (keep the key in an environment variable, not in the file):

```json
{
  "mcpServers": {
    "reseller": {
      "type": "http",
      "url": "https://HOST/mcp",
      "headers": { "Authorization": "Bearer ${RESELLER_KEY}" }
    }
  }
}
```

**Cursor** (`.cursor/mcp.json`) and other clients that support remote servers with headers:

```json
{
  "mcpServers": {
    "reseller": {
      "url": "https://HOST/mcp",
      "headers": { "Authorization": "Bearer sk-res-..." }
    }
  }
}
```

**Claude Desktop** and any client that launches local commands should use the stdio bridge:

```json
{
  "mcpServers": {
    "reseller": {
      "command": "reseller",
      "args": ["mcp", "--url", "https://HOST"]
    }
  }
}
```

Config files that hold a literal key are secrets. Do not commit them.

## Local stdio bridge: `reseller mcp`

```sh
reseller mcp --url https://HOST
claude mcp add reseller -- reseller mcp --url https://HOST
```

The bridge reads and writes newline-delimited JSON-RPC on stdin and stdout. Logs go to stderr.

It forwards `tools/list` and `tools/call` to the remote `/mcp` endpoint, using the stored credential and the 2026-07-28 protocol. The remote server must therefore support 2026-07-28.

### Dual-era behaviour toward the local client

- **Modern (2026-07-28):**
  - The bridge answers `server/discover` and any request that carries per-request `_meta`.
  - All results carry `resultType: "complete"` and the bridge's own `serverInfo`. This includes the local tools and the forwarded remote tools.
  - Lists carry `ttlMs` and `cacheScope: "public"`. `ttlMs` is `0` when the remote server cannot be reached, so a partial tool list is not cached.
  - Errors use `-32602` (missing `_meta`, unknown tool, or cursor), `-32022` (unsupported version, with the supported list), or `-32601` (unknown method, including `ping`).
  - `notifications/cancelled` stops a request that is still in flight.
- **Legacy (2025-11-25):**
  - An `initialize` switches the process to the legacy era. Its result names `2025-11-25`, whatever revision was requested.
  - `ping`, `tools/list`, and `tools/call` then work without `_meta` and return 2025-11-25-shaped results.
  - A request without `_meta` before any `initialize` gets `-32602`.

### Bridge flags

| Flag | Environment variable | Default | Meaning |
|---|---|---|---|
| `--url <ORIGIN>` | `RESELLER_URL` | Required | Server origin, for example `https://reseller.example.com`. Must use `https`. Plain `http` is accepted only for `localhost`, `127.0.0.1`, and `::1`. No path, query, or credentials. |
| `--credentials <FILE>` | `RESELLER_CREDENTIALS` | `$XDG_CONFIG_HOME/reseller/credentials.json`, else `~/.config/reseller/credentials.json` | Credential file |
| `--output-dir <DIR>` | `RESELLER_OUTPUT_DIR` | `reseller-output` (relative to the working directory) | Where generated audio and images are saved |
| `--token <TOKEN>` | `RESELLER_TOKEN` | None | An `sk-res-...` key or a `dash_...` token, used without storing it. Disables `signup`, `login`, and `logout`. |

### Credential file

The credential file is a JSON object keyed by server origin, so one file can serve several servers. Each entry holds `api_key`, `dashboard_token`, `key_id`, `key_prefix`, and `account_id`.

- The file is written atomically through a temporary file created with mode `0600`.
- Its directory is created with mode `0700`.
- Prefer `RESELLER_TOKEN` over `--token` on shared machines. Command lines are visible to other local users.

### Output directory

Audio and images returned by `generate_speech` and `generate_image` are also saved to `<output-dir>/audio-<millis>-<id>.<ext>` or `image-<millis>-<id>.<ext>`. Here `<id>` is 8 hex characters. Files are never overwritten.

The tool result gains a text line such as `Saved audio to <path>`.

### Why the bridge is the more secure option

- **Secrets stay local.** After `signup` or `login`, the API key and dashboard token are written to the private file. They are **not** returned to the model, so they never enter the conversation, logs, or the provider's servers. Only `reveal_credentials` prints them, and only when asked.
- **Local proof of work.** The bridge solves the proof of work, so the agent never has to compute hashes or handle raw signup secrets.
- **Least privilege.** Each tool call uses the least-privileged credential that can run it. Tools that need an API key get the key. Account-management tools get the dashboard token when one is stored.
- **No redirects, no plain HTTP.** The HTTP client never follows redirects, so a bearer token cannot be forwarded elsewhere. It also refuses plain HTTP to non-local hosts.
- **Automatic rotation save.** A rotated key that was the stored one is saved to the file automatically, and it is masked in the result.

### Bridge-only tools

The bridge adds these local tools. The remote server answers signed-in callers only, so before signup the bridge lists its own tools plus the server's tool catalogue, and calls fail with `not_signed_in` until `signup` or `login`.

| Tool | Inputs | Result and effects |
|---|---|---|
| `signup` | `name?` (up to 80 characters), `contact?` (up to 200 characters) | Fetches the challenge, solves it locally, creates the account with free trial credit, and saves the credentials. Returns the server, account ID, key prefix, credential file path, and limits. Returns **no secrets**. Fails with `already_signed_in` if the server already has stored credentials. Call `logout` first. |
| `login` | `api_key?` (`sk-res-` plus 64 hex), `dashboard_token?` (`dash_` plus 64 hex). At least one is required. | Checks the credentials with the server (`GET /v1/account`). Both must belong to the same account. Then stores them. Prefer `--token` or `RESELLER_TOKEN`, so secrets never pass through the conversation. |
| `logout` | None | Deletes the stored credentials for this server. The account and its keys are not affected. |
| `whoami` | None | Server, credential type and source, key prefix, account ID, and balance |
| `reveal_credentials` | None | Returns the stored API key and dashboard token in plain text. Sensitive: use it only when the user explicitly asks. |

## Tools

Authentication levels:

| Level | Accepted credential |
|---|---|
| **any** | API key or dashboard token |
| **key** | API key only. A dashboard token gets `dashboard_token_not_allowed`. |

Costs are billed to the account at your configured prices and markup.

The content of a `generate_*` or `transcribe_audio` result is the result only, so the model reads no billing. The billing goes to the client in the result's `_meta["reseller/billing"]`: `{request_id, model, tokens, billed_usd, usage_estimated, balance_usd}`. `account_overview` and `list_requests` give the same figures to an agent that needs them.

| Tool | Auth | Inputs | Output | Side effects and cost |
|---|---|---|---|---|
| `generate_text` | key | `prompt` or `messages[]` (1 to 100 items of `{role, content}`, where role is `system`, `developer`, `user`, or `assistant`; not both), `system?`, `model?`, `max_tokens?` (1 to 131072), `temperature?` (0 to 2). Text fields are limited to 200,000 characters. | Reply text. Structured: `{text, model, finish_reason, usage}` | Billed per token. `usage_estimated` in the billing marks estimated usage. |
| `generate_speech` | key | `text` (1 to 4096 characters), `voice?`, `model?`, `format?` (`wav` by default, `mp3`, `opus`, `aac`, `flac`, or `pcm`). `wav` is built from upstream `pcm`, so it works with every provider, including PCM-only ones such as Gemini TTS. | Audio content block (base64). Structured: `{format, mime_type, bytes}` | Billed per input character or per audio second |
| `transcribe_audio` | key | `audio_base64` (no `data:` prefix), `filename` (with extension), `model?`, `language?` (ISO-639) | Transcript text. Structured: `{text, language, duration}` | Billed per audio second or per token. Audio is limited by `mcp.max_media_bytes` (`media_too_large`). |
| `generate_image` | key | `prompt` (1 to 4000 characters), `model?`, `size?` (`auto` or `WxH`), `n?` (1 to 4, default 1) | Inline image content blocks (base64), or text links for provider-hosted URLs. Structured: `{images[]}` | Billed per image or per token. Hosted image URLs are returned as links. The server never fetches them. |
| `list_models` | key | None | `{models: [ids]}` | None |
| `account_overview` | any | None | Balance, debt, subscription, and credit lots | Read-only |
| `usage_summary` | any | `days?` (1 to 365, default 30), `key_id?` | Daily and per-model usage | Read-only |
| `list_requests` | any | `limit?` (1 to 100, default 20), `offset?`, `model?`, `status?` (`2xx`, `4xx`, `5xx`, or `pending`), `kind?` (`llm`, `tts`, `stt`, or `image`) | Request history with billed cost and the `estimated` flag | Read-only |
| `list_keys` | any | None | Keys with ID, prefix, name, status, and limits. Never secrets. | Read-only |
| `create_key` | any | `name` (1 to 80 characters) | New key `{id, key, key_prefix, dashboard_token, ...}`, shown once | Creates a child key. Subject to `account_key_limit`, `key_creation_daily_limit`, `creation_rate_limited`, `ip_key_limit`, `creation_cooldown`, and `child_key_limit`. |
| `rotate_key` | any | `key_id` | New credentials, shown once | **Blocks the old key.** Limited by `rotate_cooldown`, the daily budget, and the global hourly cap. |
| `block_key` | any | `key_id` | Updated key | Blocks a key. Customers cannot unblock it. |
| `list_plans` | any | None | Plans and enabled payment methods | Read-only |
| `redeem_code` | any | `code` | New balance or subscription | Consumes a single-use code. Returns `invalid_code` on failure. Rate limited by `redeem_rate_limited`. |
| `start_checkout` | any | `plan` (plan code), `method?` (`stripe` or `crypto`) | Payment URL (card), or crypto payment instructions | Creates a pending order (rate limited by `checkout_rate_limited`). Credit arrives only after the payment is confirmed. |

Every tool rejects unknown arguments with `invalid_arguments`.

The results of `create_key` and `rotate_key` contain secrets.

Each tool carries MCP annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) and a `_meta["reseller/auth"]` entry. That entry is `api_key_or_dashboard_token` or `api_key`.

## Signup and login walkthrough

Signing up through MCP tools is gone: with OAuth, signup happens on the browser consent page, and the bridge's `signup` tool uses `/v1/signup` directly.

### With the bridge (recommended)

1. The agent calls `whoami`, or any tool. Before signup, the response shows `signed_in: false` and the error code `not_signed_in`.
2. The agent calls `signup`, optionally with `name` and `contact`. The bridge fetches `GET /v1/signup/challenge`, finds the proof-of-work number locally, posts to `POST /v1/signup`, and writes the credential file. The result shows only the account ID and key prefix.
3. The agent calls `whoami` to see the trial balance, then uses `generate_text` and the other tools.
4. When credit runs out (`insufficient_quota`), the agent calls `list_plans` and `start_checkout`. Alternatively, the user redeems a code with `redeem_code`.
5. To use an existing account, either set `RESELLER_TOKEN` before starting the bridge, or call `login`. Supply the credentials outside the transcript where possible.

### With the remote endpoint

1. Add `https://HOST/mcp` to the client. It opens the consent page in the browser.
2. On the page, sign in with an existing key, or create an account (the page solves the proof of work). Self-service signup can be disabled (`403 signup_disabled`). In that case, ask the operator for a key.
3. Allow the connection. The client now works with the account, and the trial credit applies.

## Error reference

Failures appear at three levels.

1. **HTTP errors before the protocol layer.** Credential, origin, version, size, and rate-limit problems use the standard API shape, `{"error":{"message","type","param","code"}}`, with the usual status.
2. **JSON-RPC errors.** These report protocol misuse. Unknown tools and malformed requests are protocol errors.
3. **Tool errors.** These report failures while running a valid call. They are normal results with `resultType: "complete"` and `isError: true`.

### JSON-RPC and HTTP status codes

| JSON-RPC code | Name | HTTP status | When it happens |
|---|---|---|---|
| `-32700` | Parse error | 400 | The body is not valid JSON |
| `-32600` | Invalid request | 400 | Malformed envelope, or a batch |
| `-32601` | Method not found | 404 (modern era) | Unknown or unserved method, including `ping` in the modern era |
| `-32602` | Invalid params | 400 | Bad params, unknown tool, cursor on `tools/list`, missing or malformed `_meta`, or a legacy `initialize` without `protocolVersion` |
| `-32020` | HeaderMismatch | 400 | `MCP-Protocol-Version`, `Mcp-Method`, or `Mcp-Name` is missing, or disagrees with the body |
| `-32021` | Not returned | n/a | Client capabilities are never required by this server |
| `-32022` | Unsupported protocol version | 400 | Requested version is not supported. `error.data.supported` lists the supported versions. |

Other HTTP statuses on `/mcp`:

| Status | Code | When it happens |
|---|---|---|
| 401 | `invalid_api_key` | The credential is not valid |
| 403 | `origin_not_allowed` | The `Origin` header is not allowed |
| 403 | `account_suspended` | The account is suspended |
| 403 | `signup_disabled` | Self-service signup is disabled |
| 404 | `mcp_disabled` | `[mcp].enabled = false` |
| 413 | `request_too_large` | The message exceeds the size limit |
| 429 | `management_rate_limited`, `too_many_auth_failures` | Rate limits. The response carries `Retry-After`. |
| 202 | (none) | A notification was accepted |
| 405 | (none) | `GET` or `DELETE` was used |

### Tool error codes

A tool error has `isError: true`. The text reads `<message> (error code: <code>) Retry after N seconds.` The `structuredContent.error` object is `{code, message, retry_after_seconds}`. Agents should branch on `code`.

| Code | Meaning and action |
|---|---|
| `invalid_arguments` | Bad or unknown arguments. Fix the call. |
| `authentication_required` | No credential reached the tool. Reconnect with OAuth or send a bearer token. |
| `admin_token_not_customer` | The administrator token has no customer account. Use `/admin`, or connect with a customer key. |
| `invalid_token` (HTTP 401) | The OAuth access token is invalid, expired, or revoked. The client refreshes or signs in again. |
| `dashboard_token_not_allowed` | The tool needs an API key. Use `create_key`, or use the API key. |
| `invalid_api_key`, `key_revoked`, `key_blocked`, `key_expired`, `account_suspended` | Credential problems. Rotate or replace the key, or contact the operator. |
| `insufficient_quota` (402) | Out of credit, or the request may cost more than the balance (lower `max_tokens`). Use `list_plans` and `start_checkout`, or `redeem_code`. |
| `model_not_found` (404) | The model is unknown or the key's allowlist excludes it. Omit `model`, or use `list_models`. |
| `rate_limit_exceeded`, `request_limit_exceeded`, `token_limit_exceeded`, `spend_limit_exceeded`, `concurrency_limit_exceeded` | Wait `retry_after_seconds` when it is given. |
| `creation_cooldown`, `ip_key_limit`, and other key or signup guards | Wait, then retry. See the README rate-limit table. |
| `pow_required`, `pow_invalid`, `pow_replayed` | The signup proof of work is missing, wrong or expired, or already used (bridge `signup`). Fetch a new challenge. |
| `upstream_error` | The provider returned a non-2xx response. The message includes its status, detail, and request ID. |
| `invalid_upstream_response` | The provider's reply was not usable (invalid JSON, or no audio or images) |
| `response_too_large`, `media_too_large` | Output or input exceeds `mcp.max_media_bytes`. The request may still have been billed. Check `list_requests`. |

### Bridge error codes

| Code | Meaning |
|---|---|
| `not_signed_in` | No stored credentials for this server |
| `already_signed_in` | `signup` was called while credentials exist. Call `logout` first. |
| `token_override_active` | `--token` or `RESELLER_TOKEN` is set, so `signup`, `login`, or `logout` is disabled |
| `network_error` | The bridge could not reach the server |
| `invalid_response` | The server reply was not usable |
| `pow_unsolved` | The bridge could not solve the proof of work |
| `credentials_unreadable` | The credential file could not be read |
| `credentials_unwritable` | The credential file could not be written |
| `http_<status>` | A server error without a code, for example `http_503` |

## Security notes

- **Secrets are shown once.** `create_key` and `rotate_key` return secrets that cannot be retrieved later. The bridge's `reveal_credentials` returns the stored secrets again on request.
- **Bridge masking.** The bridge masks credentials for `signup`, `login`, and rotation of the stored key. A `create_key` call, or a rotation of a different key through the bridge, still returns the new secrets in the transcript. Store them right away.
- **Never paste a dashboard token into a shared transcript, issue, or chat.** It manages the account (rotate, block, buy plans, redeem codes), even though it cannot call the proxy. Treat `sk-res-` keys the same way, because they spend your credit.
- **Prefer environment variables or the credential file.** Avoid inline tokens in command lines and in committed config files.
- **Contain exposure.** Rotate any key that may have been exposed (`rotate_key`), or block it (`block_key`).
- **SSRF.** `generate_image` never fetches provider-hosted image URLs. They are returned as links. The server only contacts the upstreams you configured. The bridge only contacts the `--url` origin, and it does not follow redirects.
- **Origin protection.** `/mcp` rejects browser requests from foreign origins. Do not set `cors_origin = "*"` on a deployment where that matters.
- **Abuse controls apply to agents too.** Signup proof of work, per-IP and global creation limits, key caps, and the auth-failure lockout all apply. An agent that loops on invalid credentials locks its own IP out for up to an hour.
- **Spend limits.** Set a spend limit on agent keys (`keys.default_limits.spend`, or a per-key limit set by an administrator). This stops a runaway agent from draining the account.

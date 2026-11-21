# MCP: reseller for AI agents

reseller speaks the [Model Context Protocol](https://modelcontextprotocol.io), so an agent can create an account, generate text, speech, transcripts and images, check its balance, and top up, without a human writing HTTP clients. There are two ways to connect:

| | Remote endpoint | Local stdio bridge |
|---|---|---|
| Command | none; the client calls `POST /mcp` | `reseller mcp --url https://HOST` |
| Credentials | you configure a `Authorization: Bearer` header | stored in a private local file; the agent never needs to see them |
| Signup | the agent must solve the proof of work itself and handle the secrets | built in (`signup` tool solves the proof of work locally) |
| Generated media | returned inline only | returned inline and saved to disk |
| Best for | existing key, hosted/remote agents | desktop and coding agents; the more secure option |

Both expose the same service tools and use the same admission, limits, metering, model policy, and prices as the HTTP API; every generation is billed to the account like a normal `/v1` request.

## Remote endpoint

`POST https://HOST/mcp` is a stateless Streamable HTTP server: each POST carries one JSON-RPC 2.0 message and gets an `application/json` reply (no sessions, no SSE stream).

- **Auth:** `Authorization: Bearer sk-res-…` (API key; all tools) or `Bearer dash_…` (dashboard token; account tools only, see [tools](#tools)). `x-api-key` also works. Without a credential, `initialize`, `tools/list`, and the public tools (`list_plans`, `signup_challenge`, `create_account`) still work; other tools return `authentication_required`.
- **Protocol versions:** `2025-11-25`, `2025-06-18`, `2025-03-26`. `initialize` answers with the requested version when supported, otherwise the newest. If an `MCP-Protocol-Version` header is sent it must be one of these (else `400`).
- **Methods:** `initialize`, `ping`, `tools/list`, `tools/call`. Notifications (messages without `id`) get `202 Accepted`. JSON-RPC batches are rejected. `GET /mcp` returns `405` (no server-initiated stream). Capabilities: `tools` only.
- **Origin rule:** a request with an `Origin` header is accepted only if it matches `public_url` (or `cors_origin`, or any origin when `cors_origin = "*"`); otherwise `403 origin_not_allowed`. This stops web pages from driving a private deployment through a browser (DNS rebinding). Clients without an `Origin` header (CLI, desktop, the bridge) are unaffected.
- **Limits:** counts against `rate_limits.management_per_minute` per client IP (`429 management_rate_limited`); invalid credentials count against `rate_limits.auth_failures_per_hour` and lock the IP out with `429 too_many_auth_failures`; a message may be at most `mcp.max_media_bytes × 4/3 + 256 KiB` (`413 request_too_large`); generated audio/images larger than `mcp.max_media_bytes` (default 16 MiB, configurable up to 256 MiB) are refused with `response_too_large` (the request may still have been billed). `[mcp].enabled = false` answers `404 mcp_disabled`.

### Client configuration

Claude Code:

```sh
claude mcp add --transport http reseller https://HOST/mcp \
  --header "Authorization: Bearer sk-res-…"
```

Project `.mcp.json` (keep the key in an environment variable, not in the file):

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

Cursor (`.cursor/mcp.json`) and other clients that support remote servers with headers:

```json
{
  "mcpServers": {
    "reseller": {
      "url": "https://HOST/mcp",
      "headers": { "Authorization": "Bearer sk-res-…" }
    }
  }
}
```

Claude Desktop (and any client that launches local commands) should use the stdio bridge below:

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

Config files holding a literal key are secrets: do not commit them.

## Local stdio bridge: `reseller mcp`

```sh
reseller mcp --url https://HOST
claude mcp add reseller -- reseller mcp --url https://HOST
```

The bridge speaks newline-delimited JSON-RPC on stdin/stdout (logs go to stderr) and forwards `tools/list` and `tools/call` to the remote `/mcp` with the stored credential.

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--url <ORIGIN>` | `RESELLER_URL` | required | Server origin, for example `https://reseller.example.com`. Must be `https`; plain `http` is accepted only for `localhost`, `127.0.0.1`, and `::1`. No path, query, or credentials. |
| `--credentials <FILE>` | `RESELLER_CREDENTIALS` | `$XDG_CONFIG_HOME/reseller/credentials.json`, else `~/.config/reseller/credentials.json` | Credential file |
| `--output-dir <DIR>` | `RESELLER_OUTPUT_DIR` | `reseller-output` (relative to the working directory) | Where generated audio and images are saved |
| `--token <TOKEN>` | `RESELLER_TOKEN` | none | An `sk-res-…` key or `dash_…` token to use without storing it; disables `signup`, `login`, and `logout` |

**Credential file.** A JSON object keyed by server origin (so one file serves several servers) holding `api_key`, `dashboard_token`, `key_id`, `key_prefix`, and `account_id`. It is written atomically through a temporary file created with mode `0600`; its directory is created with mode `0700`. Prefer an environment variable (`RESELLER_TOKEN`) over `--token` on shared machines, since command lines are visible to other local users.

**Output directory.** Audio and images returned by `generate_speech` and `generate_image` are also written to `<output-dir>/audio-<millis>-<id>.<ext>` or `image-…` (files are never overwritten); the tool result gains a text line `Saved audio to <path>`. The bridge also mentions the directory in its initialize instructions.

**Why it is the more secure option.**

- Secrets stay on your machine. After `signup` or `login` the API key and dashboard token are written to the private file and are **not** returned to the model, so they never enter the conversation, logs, or the provider's servers. Only `reveal_credentials` prints them, and only on request.
- The proof of work is solved locally by the bridge, so the agent does not need to compute hashes in-context or handle raw signup secrets.
- Each tool call is sent with the least-privileged credential that can run it: tools that need an API key get the key; account-management tools get the dashboard token when one is stored.
- The HTTP client never follows redirects (a bearer token cannot be forwarded elsewhere) and refuses plain HTTP to non-local hosts.
- A rotated key that was the stored one is saved to the file automatically and masked in the result.

### Bridge-only tools

The bridge adds these local tools and hides the remote `signup_challenge` and `create_account` (replaced by `signup`):

| Tool | Inputs | Result and effects |
|---|---|---|
| `signup` | `name?` (≤ 80), `contact?` (≤ 200) | Fetches the challenge, solves it locally, creates the account (free trial credit), saves credentials. Returns server, account id, key prefix, credentials file path, limits; **no secrets**. Fails with `already_signed_in` if the server already has stored credentials (call `logout` first). |
| `login` | `api_key?` (`sk-res-` + 64 hex), `dashboard_token?` (`dash_` + 64 hex); at least one | Verifies the credentials with the server (`GET /v1/account`; both must belong to the same account), then stores them. Prefer `--token` / `RESELLER_TOKEN` so secrets never pass through the conversation. |
| `logout` | none | Deletes the stored credentials for this server (the account and keys are unaffected). |
| `whoami` | none | Server, credential type and source, key prefix, account id, balance. |
| `reveal_credentials` | none | Returns the stored API key and dashboard token in plain text. Sensitive: only when the user explicitly asks. |

## Tools

Authentication: **none** (public), **any** (API key or dashboard token), **key** (API key only; a dashboard token gets `dashboard_token_not_allowed`). Costs are billed to the account at your configured prices and markup.

| Tool | Auth | Inputs | Output | Side effects / cost |
|---|---|---|---|---|
| `generate_text` | key | `prompt` or `messages[]` (1–100 of `{role: system\|developer\|user\|assistant, content}`; not both), `system?`, `model?`, `max_tokens?` (1–131072), `temperature?` (0–2). Text fields up to 200,000 characters. | Reply text plus a billing line; structured `{text, model, finish_reason, usage, billing}` | Billed per token (`billing.usage_estimated` marks estimated usage) |
| `generate_speech` | key | `text` (1–4096), `voice?`, `model?`, `format?` (`mp3` default, `wav`, `opus`, `aac`, `flac`, `pcm`) | Audio content block (base64) plus billing; structured `{format, mime_type, bytes, billing}` | Billed per input character / audio second |
| `transcribe_audio` | key | `audio_base64` (no `data:` prefix), `filename` (with extension), `model?`, `language?` (ISO-639) | Transcript text plus billing; structured `{text, language, duration, billing}` | Billed per audio second / tokens; audio limited by `mcp.max_media_bytes` (`media_too_large`) |
| `generate_image` | key | `prompt` (1–4000), `model?`, `size?` (`auto` or `WxH`), `n?` (1–4, default 1) | Image content blocks (inline base64) or text links for provider-hosted URLs, plus billing; structured `{images[], billing}` | Billed per image/tokens. Hosted image URLs are returned as links and never fetched by the server |
| `list_models` | key | none | `{models: [ids]}` | none |
| `account_overview` | any | none | Balance, debt, subscription, credit lots | read-only |
| `usage_summary` | any | `days?` (1–365, default 30), `key_id?` | Daily and per-model usage | read-only |
| `list_requests` | any | `limit?` (1–100, default 20), `offset?`, `model?`, `status?` (`2xx\|4xx\|5xx\|pending`), `kind?` (`llm\|tts\|stt\|image`) | Request history with billed cost and `estimated` flag | read-only |
| `list_keys` | any | none | Keys: id, prefix, name, status, limits (never secrets) | read-only |
| `create_key` | any | `name` (1–80) | New key `{id, key, key_prefix, dashboard_token, …}`, shown once | Creates a child key; subject to `account_key_limit`, `key_creation_daily_limit`, `creation_rate_limited`, `ip_key_limit`/`creation_cooldown`, `child_key_limit` |
| `rotate_key` | any | `key_id` | New credentials, shown once | **Blocks the old key**; limited by `rotate_cooldown`, the daily budget, and the global hourly cap |
| `block_key` | any | `key_id` | Updated key | Blocks a key; customers cannot unblock it |
| `list_plans` | none | none | Plans and enabled payment methods | read-only |
| `redeem_code` | any | `code` | New balance / subscription | Consumes a single-use code; `invalid_code` on failure, rate limited (`redeem_rate_limited`) |
| `start_checkout` | any | `plan` (plan code), `method?` (`stripe\|crypto`) | Payment URL (card) or crypto payment instructions | Creates a pending order (`checkout_rate_limited`); credit arrives only after the payment is confirmed |
| `signup_challenge` | none | none | ALTCHA challenge `{algorithm, challenge, maxnumber, salt, signature}` or `{"required": false}` | `challenge_rate_limited` |
| `create_account` | none | `altcha?` (base64 solution, ≤ 2048), `name?` (≤ 80), `contact?` (≤ 200) | New account: key, dashboard token, limits, shown once | Creates an account with free trial credit; subject to the signup guards |

Every tool rejects unknown arguments with `invalid_arguments`. `rotate_key`, `create_key`, and `create_account` results contain secrets. Tools carry MCP annotations (`readOnlyHint`, `destructiveHint`, `idempotentHint`, `openWorldHint`) and an `_meta["reseller/auth"]` entry (`none`, `api_key_or_dashboard_token`, `api_key`).

## Signup and login walkthrough

With the **bridge** (recommended):

1. Agent calls `whoami` (or any tool): `signed_in: false` / `not_signed_in`.
2. Agent calls `signup` (optionally with `name`, `contact`). The bridge fetches `GET /v1/signup/challenge`, finds the proof-of-work number locally, posts `POST /v1/signup`, and writes the credentials file. The result shows the account id and key prefix only.
3. Agent calls `whoami` to see the trial balance, then `generate_text` and friends. When credit runs out (`insufficient_balance`), it calls `list_plans` and `start_checkout`, or the user redeems a code with `redeem_code`.
4. To use an existing account instead, set `RESELLER_TOKEN` before starting the bridge, or call `login` with credentials (preferably supplied by the user outside the transcript).

With the **remote endpoint** and no credential:

1. `signup_challenge` returns the puzzle. If it is `{"required": false}`, skip to step 3.
2. Find the integer `number` in `0..=maxnumber` such that `hex(sha256(salt + decimal(number))) == challenge`, then base64-encode the JSON `{algorithm, challenge, number, salt, signature}`. Solutions are single use and expire after `signup.pow_ttl_seconds` (default 300 s).
3. `create_account` with `altcha` (and optional `name`, `contact`) returns `key`, `dashboard_token`, `dashboard_url`, and `limits`. These are shown **once** and now appear in the transcript: store them securely and reconfigure the client with `Authorization: Bearer <key>`. Because that exposes secrets to the model provider, prefer the bridge for first-time signup.

Self-service signup may be disabled (`403 signup_disabled`, or `create_account` fails with that code); ask the operator for a key instead.

## Error handling

Failures are reported at three levels:

1. **HTTP errors before the protocol layer** (credential, origin, version, size, or rate-limit problems) use the normal API shape `{"error":{"code","message"}}` with the usual status: `401 invalid_api_key`, `403 origin_not_allowed`, `403 account_suspended`, `404 mcp_disabled`, `413 request_too_large`, `429 management_rate_limited` / `too_many_auth_failures` (with `Retry-After`).
2. **JSON-RPC errors** for protocol misuse: `-32700` parse error and `-32600` invalid request (HTTP 400), `-32601` unknown method, `-32602` invalid params or unknown tool.
3. **Tool errors** are normal results with `isError: true`. The text reads `<message> (error code: <code>) Retry after N seconds.` and `structuredContent.error` is `{code, message, retry_after_seconds}`. Agents should branch on `code`:

| Code | Meaning / action |
|---|---|
| `invalid_arguments` | Bad or unknown arguments; fix the call |
| `authentication_required` | No credential; sign up or log in |
| `dashboard_token_not_allowed` | Tool needs an API key; use `create_key` or the API key |
| `invalid_api_key`, `key_revoked`, `key_blocked`, `key_expired`, `account_suspended` | Credential problems; rotate, replace, or contact the operator |
| `insufficient_balance` (402) | Out of credit; `list_plans` + `start_checkout`, or `redeem_code` |
| `model_not_allowed` | The key's allowlist excludes the model; omit `model` or use `list_models` |
| `rate_limit_exceeded`, `request_limit_exceeded`, `token_limit_exceeded`, `spend_limit_exceeded`, `concurrency_limit_exceeded`, and the key/signup guards (`creation_cooldown`, `ip_key_limit`, …) | Wait `retry_after_seconds` where given; limits are described in the README |
| `pow_required`, `pow_invalid`, `pow_replayed` | Signup proof of work missing, wrong/expired, or already used; fetch a new challenge |
| `upstream_error` | The provider returned a non-2xx response; message includes its status, detail, and request id |
| `invalid_upstream_response` | The provider's reply was not usable (invalid JSON, no audio/images) |
| `response_too_large` / `media_too_large` | Output or input exceeds `mcp.max_media_bytes`; the request may still have been billed (check `list_requests`) |

Bridge-specific codes: `not_signed_in`, `already_signed_in`, `token_override_active`, `network_error`, `invalid_response`, `pow_unsolved`, `credentials_unreadable`, `credentials_unwritable`, `http_<status>` (server errors without a code).

## Security notes

- **Secrets are shown once.** `create_account`, `create_key`, and `rotate_key` (and the bridge's `reveal_credentials`) return secrets that cannot be retrieved later. The bridge masks credentials for `signup`, `login`, and rotation of the stored key, but a `create_key` or a rotation of a different key through the bridge still returns the new secrets in the transcript: store them right away.
- **Never paste a dashboard token into a shared transcript, issue, or chat.** It manages the account (rotate, block, buy plans, redeem codes) even though it cannot call the proxy. Treat `sk-res-` keys the same way: they spend your credit.
- Prefer environment variables or the credential file over inline tokens in command lines and committed config files. Rotate a key you may have exposed (`rotate_key`) or block it (`block_key`).
- **SSRF.** `generate_image` never fetches provider-hosted image URLs; they are returned as links. The server only talks to the upstreams you configured. The bridge only contacts the `--url` origin and does not follow redirects.
- **Origin protection.** `/mcp` rejects browser requests from foreign origins; do not set `cors_origin = "*"` on a deployment where that matters.
- **Abuse controls apply to agents too**: signup proof of work, per-IP and global creation limits, key caps, and the auth-failure lockout. An agent looping on invalid credentials will lock its own IP out for up to an hour.
- Keep a spend limit on agent keys (`keys.default_limits.spend`, or per-key limits set by an administrator) so a runaway agent cannot drain the account.

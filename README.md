<p align="center">
  <img src="assets/logo.png" width="96" height="96" alt="reseller logo">
</p>

# reseller

**Your API. Your pricing. One binary.**

A self-hosted, OpenAI-compatible API reseller platform written in Rust. Proxy language, text-to-speech, speech-to-text, and image generation/editing requests; issue customer keys; meter usage; sell prepaid credit by card or crypto; and manage everything from an embedded, white-label web workspace.

- Streaming JSON, SSE, audio, multipart uploads, and WebSocket transport
- Separate LLM / TTS / STT / image upstreams with per-upstream timeouts, headers, model policies, and fallback chains
- **MCP server for AI agents**: a remote `POST /mcp` endpoint and a local `reseller mcp` stdio bridge that can sign an agent up, generate text/speech/transcripts/images, and manage the account ([docs/mcp.md](docs/mcp.md))
- **Proof-of-work signup** (ALTCHA-compatible) instead of an open key endpoint, plus per-IP and per-account abuse limits, rotation limits, and an auth-failure lockout
- Self-service accounts, shared balances, key rotation, model allowlists, and usage limits
- **Accurate metering**: integer micro-USD accounting with cached-input, per-character, per-image, per-second, and flat per-request prices, provider-reported cost override, estimated-usage flagging, expiring credit lots, markup overrides, and debt tracking
- Prepaid plans, single-use redeem codes, Stripe Checkout, and Payroof crypto payments (USDC/USDT and native coins on EVM chains) with verified, idempotent webhooks
- White-label branding: name, copy, logo, favicon, colours, custom CSS, and an optional theme directory
- Customer and administrator dashboards, request history, usage charts, live statistics API
- SQLite WAL with ordered migrations; no external database or frontend build required
- Non-root Docker image for AMD64 and ARM64

## Screenshots

Landing page — quick-start snippet, self-service API key with proof-of-work signup, and the prepaid plan catalogue:

![Landing page with a curl quick-start snippet, Get an API key and Open workspace buttons, and the prepaid credit plan cards](assets/scr.png)

## Quick start

Rust 1.98+ is required to build. The default database is `.reseller/reseller.db` relative to the working directory.

### Cargo

Once the first release has been published to crates.io:

```sh
cargo install reseller --locked
reseller init
# Edit reseller.toml: set [llm].api_key and your upstream URL/model.
reseller
```

From this checkout, before publication:

```sh
cargo install --path . --locked
reseller init
reseller
```

Or bootstrap from environment variables (first start only; see [Environment bootstrap](#environment-bootstrap)):

```sh
OPENAI_BASE_URL=https://openrouter.ai/api/v1 \
OPENAI_API_KEY=sk-or-v1-... \
OPENAI_MODEL=your-model \
reseller
```

On first startup an administrator token is generated and printed if none was supplied. **Save it.**

| Page | URL |
|---|---|
| Landing / create account | `http://localhost:56787/` |
| Customer workspace | `http://localhost:56787/workspace/` |
| Administrator console | `http://localhost:56787/admin/` |
| OpenAI-compatible base URL | `http://localhost:56787/v1` |
| MCP endpoint | `http://localhost:56787/mcp` |
| Liveness / readiness | `/healthz` / `/readyz` |

### Docker / GitHub Container Registry

After a release publishes the image:

```sh
docker run -d --name reseller --restart unless-stopped \
  -p 56787:56787 \
  -e OPENAI_BASE_URL=https://openrouter.ai/api/v1 \
  -e OPENAI_API_KEY=sk-or-v1-... \
  -e OPENAI_MODEL=your-model \
  -e PUBLIC_URL=https://api.example.com \
  -v reseller-data:/data \
  ghcr.io/reseller-rs/reseller:latest

docker logs reseller  # first-start administrator token
```

### Docker Compose

`docker compose up -d` uses the bundled `compose.yaml`. Environment substitution is supported through your shell or a Compose `.env` file (`OPENAI_*`, `TTS_*`, `STT_*`, `IMG_*`, `ADMIN_TOKEN`, `PUBLIC_URL`, `BRAND_NAME`, `STRIPE_*`, `PAYROOF_*`). Pin a release tag instead of `latest` for controlled upgrades.

Build locally with `docker build -t reseller:local .`. The release binary is a static musl build compiled with size-first release settings, and the runtime image is `scratch` plus the binary, the CA bundle, and an empty writable `/data` — no shell, no curl, no package manager. The image runs as UID/GID `10001`, uses `/data/reseller.db`, and its healthcheck invokes `reseller healthcheck`, which probes the local `/readyz` over TCP. Bind-mounted directories must be writable by that UID. Keep the data volume across upgrades. Allow up to the upstream timeout for graceful shutdown (the compose file sets `stop_grace_period: 330s`).

## Configuration

`reseller init` creates a starter TOML file (default `reseller.toml`, mode `0600`, never overwrites) from [`reseller.example.toml`](reseller.example.toml), which is the **full annotated reference** for every setting and default. Unknown keys are rejected.

On **first start only**, the TOML file and bootstrap environment variables seed SQLite (the file is not even read once the database holds settings). Afterwards **Admin → Settings** is authoritative; to manage settings as a file, use `reseller config export` / `reseller config import` with the server stopped. Environment/file edits do not override an existing database. CORS changes take effect after restart; other settings apply to new requests. Secrets are redacted in API responses; blank secret fields preserve the current value. Changing `key_pepper` invalidates all customer credentials, unredeemed codes, and outstanding signup challenges.

Sections at a glance (defaults in parentheses):

| Section | Purpose |
|---|---|
| top level | `public_url` (`http://localhost:56787`), `admin_token`, `key_pepper` (both at least 24 chars; generated if unset), `model_policy` / `voice_policy` (`default` \| `force` \| `passthrough`, default `default`), `markup_pct` (`30`), `require_key` (`true`), `accept_any_token` (`false`), `trust_proxy` (`false`), `cors_origin` (empty), `max_json_body_bytes` (50 MiB), `max_stream_body_bytes` (512 MiB), `upstream_timeout_seconds` (300), `log_retention_days` (30), `max_log_entries` (5000) |
| `[branding]` | White-label name, copy, logos, colours, CSS ([below](#branding-and-themes)) |
| `[llm]` `[tts]` `[stt]` `[image]` | Upstreams: `base_url` (`https://api.openai.com/v1`), `api_key`, `model`, `fallbacks` (max 10), `timeout_seconds` (0 = `upstream_timeout_seconds`, max 3600), `headers` (max 20; credential/framing headers are refused); `[tts]` also has `voice` |
| `[prices.<model>]` | Upstream prices, USD ([below](#pricing-and-metering)) |
| `[signup]` | Anonymous signup: `enabled` (`true`), `free_credit_usd` (`"0.25"`), `free_credit_expiry_days` (30; 0 = never; max 3650), `ip_limit_per_month` (2; 0 off), `cooldown_seconds` (30; 0 off), `global_hourly_limit` (100; 0 off), `pow_max_number` (100000; 0 disables proof of work; max 10000000), `pow_ttl_seconds` (300; 10–86400), `[signup.ip_overrides]` (`"1.2.3.4" = N`, 0 = unlimited) |
| `[keys]` | `max_per_account` (10 active keys, must be > 0), `max_new_per_day` (20 created or rotated per account per rolling 24 h; 0 off), `rotate_cooldown_seconds` (300; 0 off; max 86400), `[keys.default_limits]` (`rpm`, `requests`, `tokens`, `spend`, `max_children`, `allowed_models` copied onto each new key) |
| `[rate_limits]` | `management_per_minute` (120), `auth_failures_per_hour` (20), `max_concurrent_requests` (8), `checkouts_per_hour` (10), `redeem_failures_per_hour` (10), `challenges_per_minute` (10); 0 disables each |
| `[mcp]` | `enabled` (`true`), `max_media_bytes` (16 MiB; 1 byte to 256 MiB) largest audio/image returned inline |
| `[stripe]` | `secret_key`, `webhook_secret` |
| `[payroof]` | Crypto payments: `project`, `token`, `webhook_secret`, `stablecoins`, `native_coins`, `chains`, rate source, `quote_ttl_minutes`, `underpayment_tolerance_pct` (`0.5`) |

Use decimal **strings** for prices, percentages, and spend-limit amounts. Management endpoints also accept numeric USD amounts for grants/plans/codes. Responses expose money as decimal strings and timestamps as Unix seconds. Expiry inputs accept Unix seconds or RFC3339.

```toml
markup_pct = "30"

[prices.default]
input = "0.5"
output = "1.5"

[prices."my-tts-model"]
input = "0"
output = "0"
characters = "15"        # USD per 1M input characters

[keys.default_limits]
rpm = 60
max_children = 2
requests = { max = 2000, window_hours = 24 }
tokens = { max = 1000000, window_hours = 720 }
spend = { max_usd = "5", window_hours = 720 }
```

### Environment bootstrap

These variables seed the settings on **first start only** (and override the TOML file then). `ADMIN_TOKEN` and `KEY_PEPPER` must be at least 24 characters; leave them unset to have secure values generated.

```text
OPENAI_BASE_URL / OPENAI_API_KEY / OPENAI_MODEL / OPENAI_MODEL_FALLBACKS
TTS_BASE_URL    / TTS_API_KEY    / TTS_MODEL    / TTS_MODEL_FALLBACKS / TTS_VOICE
STT_BASE_URL    / STT_API_KEY    / STT_MODEL    / STT_MODEL_FALLBACKS
IMG_BASE_URL    / IMG_API_KEY    / IMG_MODEL    / IMG_MODEL_FALLBACKS
ADMIN_TOKEN / KEY_PEPPER / MARKUP_PCT / FREE_CREDIT_USD (-> signup.free_credit_usd)
PUBLIC_URL / BRAND_NAME
STRIPE_SECRET_KEY / STRIPE_WEBHOOK_SECRET
PAYROOF_API_URL / PAYROOF_PROJECT / PAYROOF_TOKEN / PAYROOF_WEBHOOK_SECRET
```

Fallback lists are comma-separated. Always-runtime variables: `HOST` (`0.0.0.0`), `PORT` (`56787`), `DB_FILE`, `RESELLER_CONFIG`, `RESELLER_THEME_DIR`, `RUST_LOG` (default `reseller=info`), and the `reseller mcp` variables. The application does not read `.env` files. `public_url` controls dashboard links, payment redirects, and the MCP Origin check; set it to the externally reachable origin. Price-table model names match case-insensitively; `default` is the fallback entry.

## Pricing and metering

Upstream cost is computed once from the exact decimal sum and rounded half-up to whole micro-USD (a positive cost never rounds to zero, so tiny requests cannot be repeated for free):

```text
usage_cost = provider usage.cost                 if the upstream reported one (even 0)
           = ((input - cached_input) * input_price
              + cached_input * cached_input_price     (defaults to input_price when unset)
              + output * output_price
              + characters * characters_price) / 1,000,000
             + audio_seconds * audio_second_price
             + images * image_price               otherwise
upstream   = usage_cost + request_price           request_price only for successful (2xx) requests
billed     = round(upstream * (1 + markup_pct / 100))
```

Price fields per `[prices.<model>]` (all decimal strings, USD): `input` and `output` per 1M tokens (defaults `0.5` / `1.5`), optional `cached_input` per 1M cached input tokens, `characters` per 1M TTS input characters, `audio_second` per second of audio, `image` per generated image, and `request` as a flat fee per successful request; all except `input`/`output` default to `0`. Cached tokens are those the provider reports within `prompt_tokens_details.cached_tokens` (or the Responses/realtime equivalents); reasoning tokens are already part of the provider's output count.

Rules:

1. A provider-reported `usage.cost` (USD) replaces the token, audio, image, and character components, including an explicit zero; the flat `request` fee is still added for successful requests.
2. Markup resolves **key → account → global** `markup_pct`.
3. Credit is consumed soonest-expiring-first; non-expiring lots come last.
4. Usage, debit, request metadata, and counters commit in one transaction.
5. For `/v1/chat/completions`, `/v1/responses`, and `/v1/completions`, when the upstream reports no usage at all, tokens are **estimated** (about four characters per token, from the request text and the generated text) and the request row is flagged `estimated: 1` (visible in `/v1/account/requests` and the admin request log; MCP generation results report `usage_estimated`). For other routes, **if a provider returns no usage and no supported audio duration, only the flat `request` fee applies**; use providers that report usage.

Every request row records the full breakdown behind its charge — `tokens`, `input_tokens`, `cached_tokens`, `output_tokens`, `characters`, `audio_ms`, `images`, `upstream_usd`, `billed_usd`, and `estimated` — so customers and administrators can audit each line.

Balance and spend/token checks are soft preflight limits: concurrent in-flight requests can overshoot (the per-account `max_concurrent_requests` limit bounds this). Uncovered spend becomes account debt, and future top-ups settle that debt before becoming spendable credit. Request admission and RPM checks are serialized in SQLite; hourly usage windows include the entire boundary hour. A completed admitted request counts even if the upstream failed. Model usage is aggregated for up to 367 days; detailed request retention is independently bounded by `log_retention_days` and `max_log_entries`.

SSE inspection works across chunk boundaries with bounded memory. JSON bodies up to 4 MiB are parsed whole; larger ones (for example base64 images) keep only their last 256 KiB, from which the trailing `usage` object is read, while inline images are counted as they stream. WAV and 16-bit PCM durations (rate/channels from the content type, default 24 kHz mono) can be estimated. WebSocket sessions are authenticated, admitted, and logged like other requests; usage from each realtime `response.done` event is summed and billed when the session ends. Balance is checked at admission only, so a long session can run into debt. Sessions are bounded by `upstream_timeout_seconds`. Upstream costs for failed fallback attempts cannot be recovered unless the provider reports them in the final response.

## Security model

**Credentials.** Every key has two secrets, each shown only once: the proxy API key `sk-res-<64 hex>` (full access: proxy plus account management) and the dashboard token `dash_<64 hex>` (account management only; it **cannot call the proxy**, which fails with `401 dashboard_token_not_allowed`). Both are stored only as HMAC-SHA256 digests keyed with `key_pepper`; redeem codes are hashed the same way. Send them as `Authorization: Bearer …` or `x-api-key`. Tokens in query strings are unsupported. The administrator token goes in `x-admin-token` or the bearer header and is compared in constant time. Blocked API keys retain workspace access; revoked credentials do not.

**Signup with proof of work.** Anonymous account creation is `GET /v1/signup/challenge` then `POST /v1/signup`. The challenge is an ALTCHA v1 (SHA-256) puzzle:

```json
{"algorithm":"SHA-256","challenge":"<hex>","maxnumber":100000,
 "salt":"<hex>?expires=<unix>","signature":"<hex>"}
```

The client finds the integer `number` in `0..=maxnumber` with `hex(sha256(salt + decimal(number))) == challenge`, then submits `base64(JSON{algorithm, challenge, number, salt, signature})` as `altcha`. `signature` is an HMAC-SHA256 under a key derived from `key_pepper`, so the server keeps no challenge state; solutions are single use (recorded in `pow_spent`, purged after expiry), expire after `pow_ttl_seconds`, and are only consumed when the account is actually created (a rate-limited attempt does not burn it). With `signup.pow_max_number = 0` the challenge endpoint returns `{"required":false}` and `altcha` may be omitted. The bundled web UI and the `reseller mcp` bridge solve the puzzle for you.

```sh
# Solve with any ALTCHA v1 client; shown here as the HTTP shape only.
curl http://localhost:56787/v1/signup/challenge
curl http://localhost:56787/v1/signup \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","contact":"me@example.com","altcha":"<base64 solution>"}'
```

Signup errors: `400 pow_required`, `400 pow_invalid` (malformed, bad signature, expired, number out of range, or incorrect solution), `409 pow_replayed`, `403 signup_disabled`, and the 429 guards below.

**Abuse limits.** Anything returning `429` carries a `Retry-After` header in seconds (60 when the server has no better estimate).

| Limit | Scope | Config | 429 `code` |
|---|---|---|---|
| Management requests (`/v1/signup`, `/v1/keys`, `/v1/account`, `/v1/plans`, `/mcp`, `/admin/api/*`) | client IP, per minute | `rate_limits.management_per_minute` | `management_rate_limited` |
| Signup challenges | client IP, per minute | `rate_limits.challenges_per_minute` | `challenge_rate_limited` |
| Invalid credentials | client IP, per hour | `rate_limits.auth_failures_per_hour` | `too_many_auth_failures` |
| Invalid redeem codes | account, per hour | `rate_limits.redeem_failures_per_hour` | `redeem_rate_limited` |
| Checkout attempts | account, per hour | `rate_limits.checkouts_per_hour` | `checkout_rate_limited` |
| In-flight proxy requests | account | `rate_limits.max_concurrent_requests` | `concurrency_limit_exceeded` |
| Self-service key creations per calendar month | IP | `signup.ip_limit_per_month`, `signup.ip_overrides` | `ip_key_limit` |
| Gap between creations | IP | `signup.cooldown_seconds` | `creation_cooldown` |
| Key creations (signup, child keys, rotations) per hour | global | `signup.global_hourly_limit` | `creation_rate_limited` |
| Active keys | account | `keys.max_per_account` | `account_key_limit` |
| Keys created or rotated per rolling 24 h | account (customers; admin exempt) | `keys.max_new_per_day` | `key_creation_daily_limit` |
| Minimum key age before rotation | key (customers; admin exempt) | `keys.rotate_cooldown_seconds` | `rotate_cooldown` |
| Active child keys of one API key | key | `keys.default_limits.max_children` / per-key limit | `child_key_limit` |

Per-key usage limits (`rpm`, `requests`, `tokens`, `spend`) return `rate_limit_exceeded`, `request_limit_exceeded`, `token_limit_exceeded`, and `spend_limit_exceeded` (all 429). Accounts that are subscribed or have `can_create_keys` set bypass the per-IP monthly allowance and cooldown, but not the global, daily, or account key caps. The in-memory limiter state is per process and not persisted; a restart only relaxes limits.

**Auth-failure lockout.** Each invalid API key, dashboard token, administrator token, or redeem code charges the client IP's auth-failure budget (`auth_failures_per_hour`, a token bucket: a burst of that many, then one more per `3600 / limit` seconds). Once exhausted, **every request from that IP that carries a credential** (on `/v1/*`, `/mcp`, or `/admin/api/*`) gets `429 too_many_auth_failures` before any database lookup, for at most one hour. Revoked or blocked keys and other non-guess errors do not count.

**Origin and headers.** `POST /mcp` refuses requests whose `Origin` header does not match `public_url` (or `cors_origin`, or any origin when `cors_origin = "*"`) with `403 origin_not_allowed`; requests without an `Origin` (CLI and desktop agents) are accepted. This blocks DNS-rebinding attacks through a browser. All responses carry `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and a strict `Content-Security-Policy` (`frame-ancestors 'none'`); `Strict-Transport-Security` is added when `public_url` is `https://`. When `cors_origin` is set (`*` or one exact origin), CORS allows the `Authorization`, `Content-Type`, `x-api-key`, `x-admin-token`, and `mcp-protocol-version` headers. The customer UI uses session storage, removes login tokens from URL fragments, and never embeds credentials in page HTML. Logs contain request metadata, not prompts, bodies, audio, auth headers, or query strings.

**Behind a proxy.** Place a TLS reverse proxy in front of public deployments. `trust_proxy` defaults off, and then the TCP peer address is the client IP. Enable it **only** when direct access to the application is blocked and your edge overwrites incoming forwarding headers: it trusts `CF-Connecting-IP`, then the **last** `X-Forwarded-For` address (the one your proxy appended; earlier entries are client-controlled). With `trust_proxy` wrongly enabled, clients can spoof their IP and evade every per-IP limit; with it wrongly disabled behind a proxy, all users share one IP and the per-IP limits (signup allowance, cooldown, lockout) apply to everyone at once.

**Database secrets.** Settings contain upstream and payment secrets that must be recoverable to proxy requests: protect the database and backups as credentials.

## Customer API

Customer routes use `Authorization: Bearer <api key or dashboard token>` unless marked public. Errors use `{"error":{"code","message"}}`. List endpoints default to 50 items and cap at 200.

```text
GET    /v1/plans                         public: active plans and payment_methods
GET    /v1/signup/challenge              public: proof-of-work challenge
POST   /v1/signup                        public: {"name"?, "contact"?, "altcha"}  -> new account + key
POST   /v1/keys                          authenticated: {"name"} -> child key on the caller's account (401 when anonymous)
GET    /v1/keys                          own keys
PATCH  /v1/keys/:id                      rename or block ({"name"?, "status":"blocked"}); customers cannot reactivate
POST   /v1/keys/:id/rotate               replace key, preserving limits/pricing/expiry; old key is blocked
GET    /v1/account                       account, balance, credit lots, subscription
GET    /v1/account/usage?days=30&key=    daily/model usage (days 1-365)
GET    /v1/account/requests?limit=20&offset=0&key=&model=&kind=&status=
POST   /v1/account/redeem                {"code":"RES-..."}
POST   /v1/account/checkout              {"plan_code":"monthly","method":"stripe"|"crypto"}
GET    /v1/account/payments              own orders (latest 50)
GET    /v1/account/payments/:id          order with crypto quotes and transfers
POST   /webhooks/stripe                  Stripe-signed raw event
POST   /webhooks/payroof                 Payroof-signed transfer event
```

Creating, listing, and rotating keys:

```sh
# Sign up (after solving the challenge); the response holds key, dashboard_token, dashboard_url, api_base, limits
curl -X POST http://localhost:56787/v1/signup -H 'Content-Type: application/json' \
  -d '{"name":"my-app","altcha":"<base64 solution>"}'

export RESELLER_KEY=sk-res-...

# Another key on the same account
curl -X POST http://localhost:56787/v1/keys \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"name":"ci"}'

curl http://localhost:56787/v1/keys -H "Authorization: Bearer $RESELLER_KEY"

# Rotate (429 rotate_cooldown if the key is younger than keys.rotate_cooldown_seconds)
curl -X POST http://localhost:56787/v1/keys/KEY_ID/rotate -H "Authorization: Bearer $RESELLER_KEY"
```

Account, usage, redeem, plans, and checkout:

```sh
curl http://localhost:56787/v1/account -H "Authorization: Bearer $RESELLER_KEY"
curl 'http://localhost:56787/v1/account/usage?days=7' -H "Authorization: Bearer $RESELLER_KEY"
curl 'http://localhost:56787/v1/account/requests?limit=20&status=2xx' -H "Authorization: Bearer $RESELLER_KEY"

curl -X POST http://localhost:56787/v1/account/redeem \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"code":"RES-XXXX-XXXX"}'          # 400 invalid_code when unknown, expired, or used

curl http://localhost:56787/v1/plans
curl -X POST http://localhost:56787/v1/account/checkout \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"plan_code":"monthly","method":"stripe"}'   # returns {"method","url","order_id"}
curl http://localhost:56787/v1/account/payments -H "Authorization: Bearer $RESELLER_KEY"
```

A new key (signup, child, or rotation) is returned once as `{id, account_id, key, key_prefix, dashboard_token, dashboard_url, api_base, limits, notice}`. All keys on an account share its credit.

## Proxying

```sh
curl http://localhost:56787/v1/chat/completions \
  -H "Authorization: Bearer $RESELLER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"your-model","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

Routing replaces `/v1` with the configured base path and preserves query strings:

- `/v1/audio/speech` → TTS
- `/v1/audio/transcriptions`, `/v1/audio/translations` → STT
- `/v1/images`, `/v1/images/*` (generations, edits, variations) → image
- All other `/v1/*` proxy routes (chat, responses, completions, embeddings, models, realtime) → LLM

`/v1/keys`, `/v1/account`, `/v1/plans`, and `/v1/signup` are handled locally and never proxied. Omit `model` to use the configured `[llm].model` under `model_policy = "default"`. `passthrough` forwards the client's value unchanged (a wrong model fails upstream), and `force` always uses the configured model. The same policy governs the multipart `model` field on uploads: `force` replaces it, `default` fills it only when missing, and the audio payload still streams without buffering. Fallbacks only apply when the final model equals the route's configured model.

Image generation and editing (JSON with inline reference images, or multipart edits):

```sh
curl http://localhost:56787/v1/images \
  -H "Authorization: Bearer $RESELLER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"meta/muse-image","prompt":"Make the sky orange",
       "input_references":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,…"}}]}'
```

JSON requests apply model/voice policies; chat streams request usage reporting. Multipart uploads stream with bounded size while only the `model` form field is rewritten by the policy. WebSocket (realtime) connections apply the same policy to the `?model=` query parameter before admission and forwarding.

Fallbacks apply only to JSON requests using the configured model, and only before forwarding response headers. Availability errors (`400/402/404/408/429/5xx`) and connection/timeouts can trigger the next model. Fallback models must also be allowed by the key. Client-selected models are not replaced by fallbacks, and multipart uploads get a single forward attempt. Redirects are not followed. Hop-by-hop headers and local credentials are removed before forwarding.

Responses include `X-Request-Id`, `X-Key-Prefix`, `X-Account-Balance-USD` (pre-request snapshot), and `X-Model-Fallback` when applicable. Failures use `{ "error": { "code": "...", "message": "..." } }`: `401` credentials (`missing_api_key`, `invalid_api_key`, `key_revoked`, `key_blocked`, `key_expired`, `dashboard_token_not_allowed`), `402 insufficient_balance`, `403` policy (`model_not_allowed`, `account_suspended`), `429` limits, `413` body size, `502/504` upstream failure, `503 proxy_configuration_error` when the route's upstream has no API key.

## MCP

Agents can use the service through the Model Context Protocol: a stateless Streamable HTTP endpoint at `POST /mcp` (JSON responses, protocol versions `2025-11-25`, `2025-06-18`, `2025-03-26`) and a local stdio bridge, `reseller mcp`, that keeps credentials and the signup proof of work on the agent's machine. Generation tools run through the same admission, limits, metering, and model policy as the HTTP API. Set `[mcp].enabled = false` to turn the endpoint off (`404 mcp_disabled`).

```sh
claude mcp add --transport http reseller https://api.example.com/mcp \
  --header "Authorization: Bearer sk-res-..."          # remote, with an existing key
claude mcp add reseller -- reseller mcp --url https://api.example.com   # local bridge (can sign up)
```

See [docs/mcp.md](docs/mcp.md) for client configuration, the tool reference, the signup walkthrough, and security notes.

## Admin API

All routes live under `/admin/api` and require the administrator token (`x-admin-token` or bearer). List limits default to 50 and cap at 200; detailed account credit history, codes, and payments return the latest 200.

```text
GET    /overview                        lifetime totals, margin, accounts, credit
GET    /usage?days=30                    daily/model series
GET    /events                          SSE snapshot every five seconds
GET    /keys?q=&status=&account=&limit=&offset=
GET    /keys/:id
PATCH  /keys/:id                        name, status, limits, markup_pct, expires_at
POST   /keys/:id/rotate                 exempt from customer rotation limits
POST   /keys/:id/revoke
GET    /accounts?q=&status=&limit=&offset=
GET    /accounts/:id
PATCH  /accounts/:id                    status, contact, note, markup_pct, can_create_keys
POST   /accounts/:id/keys               name, limits (bypasses creation guards)
POST   /accounts/:id/credits            amount_usd, source, expires_at?, note?
POST   /accounts/:id/subscriptions      plan_code
GET    /plans
POST   /plans                           upsert code, name, price_usd, credit_usd,
                                        duration_days, active?, description?
DELETE /plans/:code                     only while unreferenced; otherwise deactivate
GET    /codes
POST   /codes                           object or array (max 100) of
                                        {credit_usd OR plan_code, expires_at?, note?}
PATCH  /codes/:id                       expires_at, note (unredeemed codes only)
DELETE /codes/:id                       cancel an unredeemed code
GET    /payments                        orders: q, status, sort, dir, limit, offset
GET    /payments/:id                    order with quotes and transfers
POST   /payments/:id/complete           mark a pending/expired order paid (grants once)
GET    /payroof                         Payroof configuration, catalog, webhooks
POST   /payroof/webhook                 register {public_url}/webhooks/payroof
GET    /requests                        q, model, kind, status, path, ip, method,
                                        key, account, hour, limit, offset
GET    /requests/groups?field=model     model|ip|status|kind|path|key|hour
GET    /settings                        redacted configuration
PATCH  /settings                        validated partial configuration
```

## CLI reference

`reseller` with no subcommand runs `serve`. Global options (valid before or after the subcommand):

| Option | Env | Default | Meaning |
|---|---|---|---|
| `--config <FILE>` | `RESELLER_CONFIG` | `reseller.toml` | Seed file, read on first start only |
| `--database <FILE>` | `DB_FILE` | `.reseller/reseller.db` | SQLite database |
| `--host <IP>` | `HOST` | `0.0.0.0` | Listen address |
| `--port <PORT>` | `PORT` | `56787` | Listen port |
| `--theme-dir <DIR>` | `RESELLER_THEME_DIR` | none | White-label files served under `/theme/` |

| Subcommand | Description |
|---|---|
| `serve` | Start the proxy and dashboards (default) |
| `init` | Write a documented starter configuration; never overwrites |
| `doctor` | Validate configuration and database; prints upstream, signup, Stripe, Payroof, branding, and theme status |
| `token` | Print a random administrator token |
| `reset-admin` | Replace the stored administrator token (stop the server first, then restart) |
| `backup <DEST>` | Online SQLite snapshot (`VACUUM INTO`); refuses to overwrite |
| `healthcheck` | Probe the local `/readyz` (container health check) |
| `config export [--include-secrets]` | Stored settings as TOML; secrets blanked unless requested |
| `config import <FILE>` | Validate and replace stored settings; blank secrets keep stored values; stop the server first and restart afterwards |
| `mcp --url <ORIGIN> [--credentials <FILE>] [--output-dir <DIR>] [--token <TOKEN>]` | Local stdio MCP bridge (env `RESELLER_URL`, `RESELLER_CREDENTIALS`, `RESELLER_OUTPUT_DIR` default `reseller-output`, `RESELLER_TOKEN`); see [docs/mcp.md](docs/mcp.md) |

```sh
reseller --host 127.0.0.1 --port 8080
reseller --database /srv/reseller/reseller.db serve
reseller doctor
reseller backup ./reseller-backup.db
reseller config export > settings.toml
reseller --theme-dir ./theme
```

## Branding and themes

Everything customer-facing can be rebranded without rebuilding: edit `branding` in **Admin → Settings** (or `[branding]` in the seed file).

| Setting | Effect |
|---|---|
| `name`, `tagline`, `description` | Header, footer, page title (`name — tagline`), meta description |
| `logo_url`, `favicon_url` | Site path (for example `/theme/logo.svg`) or `http(s)` URL; empty uses the built-in mark |
| `theme_color`, `color_scheme` | Browser UI colour and `dark`/`light` scheme |
| `css_vars` | Overrides of the built-in CSS custom properties, without `--`: `accent`, `accent-2`, `accent-ink`, `bg`, `bg-soft`, `panel`, `panel-2`, `border`, `text`, `muted`, `radius`, … |
| `custom_css` | Extra stylesheet (≤ 64 KiB) appended after the built-in theme |
| `hero_eyebrow`, `hero_title`, `hero_text` | Landing page copy |
| `footer_text`, `footer_links` | Footer text and up to ten `{label, url}` links |

```json
{"branding": {"name": "Acme AI", "logo_url": "/theme/logo.svg", "favicon_url": "/theme/favicon.png",
  "css_vars": {"accent": "#ff7a1a", "accent-2": "#ff9f4a", "accent-ink": "#1d0d00"}}}
```

For files, start with `--theme-dir ./theme` (or `RESELLER_THEME_DIR`). Images, icons, and fonts in that directory are served at `/theme/<path>` (dot-files, traversal, symlinks out of the directory, and unknown file types are refused). A `theme.css` file is appended to `/assets/theme.css`, and an `index.html` file replaces the page template; it can use the placeholders `{{title}}`, `{{name}}`, `{{description}}`, `{{theme_color}}`, `{{color_scheme}}`, `{{logo_url}}`, `{{favicon_links}}`, `{{footer_text}}`, `{{footer_links}}`, and `{{branding_json}}` (keep the `<script type="application/json" id="branding">`, `/assets/style.css`, `/assets/theme.css`, and `/assets/app.js` references). All values are escaped. The Content-Security-Policy allows same-origin scripts/styles/fonts and `https:` images.

## Payments

Plans are non-renewing prepaid credit packages; `duration_days` sets their credit expiry. Customers buy them in **Workspace → Billing** with any enabled method; administrators can also issue single-use credit/plan codes, grant plans, or add credit directly. Every checkout creates a server-side order that snapshots the plan's price, credit, and duration, and a verified provider event (or an administrator) grants exactly that snapshot, once. Browser redirects never grant credit. Automatic recurring subscriptions and refunds/chargebacks are not implemented.

`GET /v1/plans` lists the enabled `payment_methods` (`stripe`, `crypto`); `POST /v1/account/checkout {"plan_code","method"}` starts a checkout (the first enabled method when `method` is omitted).

### Cards (Stripe Checkout)

1. Set `stripe.secret_key`, `stripe.webhook_secret`, and your external `public_url` in Settings (or the bootstrap environment on first start).
2. Register `https://api.example.com/webhooks/stripe` in Stripe for `checkout.session.completed` and `checkout.session.async_payment_succeeded`.
3. Stripe prices must be at least `$0.50`, in whole cents, and denominated in USD.

The webhook verifies the signature over the raw body with a five-minute tolerance. It requires a paid session with matching currency/amount/order/session. Event IDs and paid-order state prevent duplicate grants, including multiple event types for one session. Delayed methods are credited only when confirmed paid. Use Stripe test mode and the Stripe CLI to validate your configuration:

```sh
stripe listen --forward-to localhost:56787/webhooks/stripe
# Save the emitted whsec_ secret in Settings, then make a test checkout in the UI.
```

### Crypto (Payroof)

[Payroof](https://payroof.top) watches EVM chains for payments to deterministic per-order addresses derived from your watch-only key; it can never move funds. Reseller accepts **USD stablecoins** (`stablecoins`, default USDC and USDT, valued 1:1) and **native coins** (ETH, POL, BNB, … valued at the current USD rate) on every chain your Payroof project watches (or the `chains` allowlist).

1. Create a Payroof project (see the Payroof guide) and set `payroof.project` and `payroof.token` (a `prf_…` token with the `references` and `webhooks` permissions) in Settings.
2. Set `public_url` to your public **https** origin, open **Admin → Payments → Crypto payments setup**, and click **Register webhook**. This subscribes `{public_url}/webhooks/payroof` to `seen` and `confirmed` events and stores the signing secret (or paste the `whsec_…` of an existing webhook into `payroof.webhook_secret`).
3. Crypto checkout is offered once project, token, and webhook secret are set.

At checkout Reseller allocates the order's Payroof reference and stores a quote per accepted asset: stablecoins at the plan price in the token's own decimals, native coins at the current rate rounded up to 8 decimals. Rates come from a CoinGecko-compatible `rate_url` (`rate_ids` maps symbols to coin ids, cached for two minutes) or from fixed `native_rates_usd` (which also override the feed; `rate_source = "static"` disables the feed). Quotes hold for `quote_ttl_minutes`. The payment dialog shows each asset, network, amount, and deposit address and updates itself until the order is paid.

Webhooks are verified (HMAC-SHA256 with the decoded `whsec_` key, ±300 s) before parsing and deduplicated by `Payroof-Id`. `seen` records the transfer; `confirmed` values it against its quote (`amount / quoted amount × price`). Payments in several transfers or assets add up, and the order is paid once confirmed value covers the price minus `underpayment_tolerance_pct`. Transfers in unquoted assets, to a different address, or of native coins first seen after the quote expired are recorded with a note and not counted; stablecoins are honoured even after expiry. Review them in **Admin → Payments → Details**, where **Mark as paid** completes an order by hand. Events for other projects or unknown references are acknowledged and ignored. Sweep received funds from your own wallet.

## Operations

- **Health:** `GET /healthz` (liveness) and `GET /readyz` (runs `SELECT 1`); `reseller healthcheck` probes `/readyz` for containers.
- **Backups:** `reseller backup ./backup.db` takes an online snapshot including committed WAL data. Take one before every upgrade. Restore by stopping the server and replacing the database file. Backups contain upstream and payment secrets.
- **Retention:** request logs are pruned every minute to `log_retention_days` (30) and `max_log_entries` (5000); usage aggregates are kept for up to 367 days. Spent proof-of-work records are purged after their challenges expire.
- **Migrations** apply automatically at startup. Do not share one database between multiple running server instances; rate-limiter state is in memory and per process.
- **Shutdown:** SIGTERM/Ctrl-C drains connections and billing tasks; allow up to the upstream timeout.
- **Crashes:** an abrupt process or machine failure can lose usage that an upstream has not yet reported; interrupted requests remain identifiable in the database.

## Development

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Source layout (`src/`):

| Module | Responsibility |
|---|---|
| `main.rs` | CLI: `serve`, `init`, `doctor`, `token`, `reset-admin`, `backup`, `healthcheck`, `mcp`, `config export/import` |
| `web.rs` | Router, security headers, CORS, static assets, rate-limit gating, top-level dispatch |
| `theme.rs` | Branded page template, generated theme CSS, theme-directory files |
| `proxy/` | Upstream transport: JSON policies and fallbacks, streamed uploads (`multipart.rs`), usage inspection (`inspect.rs`), realtime relay (`websocket.rs`) |
| `meter.rs` | Admission (status, balance, RPM/usage limits), pricing (`Usage::cost`), exactly-once settlement |
| `api/` | Customer workspace (`customer.rs`) and administrator console (`admin.rs`) JSON APIs |
| `mcp/` | `POST /mcp` server (`mod.rs`, `tools.rs`, `generate.rs`) and the stdio bridge (`bridge.rs`) |
| `security/` | In-memory rate limiter (`limiter.rs`), ALTCHA proof of work (`pow.rs`), per-IP/account guards |
| `identity.rs`, `keys.rs` | Credential hashing/authentication; key limits, creation guards, rotation |
| `billing.rs`, `money.rs` | Credit ledger, plans, redeem codes, micro-USD arithmetic |
| `payments/` | Checkout orders (`mod.rs`), Stripe Checkout (`stripe.rs`), Payroof crypto quotes, rates and webhooks (`payroof.rs`) |
| `stats.rs` | Overview, usage series, log retention |
| `config/` | Settings (`mod.rs`), `branding.rs`, `payments.rs`, `signup.rs`, `keys.rs`, `rate_limits.rs`, `mcp.rs` |
| `db.rs`, `error.rs` | SQLite access, API error type |

The embedded UI (`assets/`) is plain HTML/CSS/JS served under a strict Content-Security-Policy; there is no frontend build step. Integration tests in `tests/` drive the real router against mock upstreams.

## License

MIT.

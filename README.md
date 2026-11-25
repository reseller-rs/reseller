<p align="center">
  <img src="assets/logo.png" width="96" height="96" alt="reseller logo">
</p>

# reseller

**Your API. Your pricing. One binary.**

reseller is a self-hosted, OpenAI-compatible API reseller written in Rust. Point it at your upstream model providers, then sell access as subscriptions with a monthly usage quota plus top-ups. It proxies language, speech, transcription, and image requests, issues customer keys, meters every request, and serves a white-label web workspace from the same binary.

## Features

- **Proxy:** streaming JSON, SSE, audio, multipart uploads, and WebSocket (realtime) transport.
- **Upstreams:** separate LLM, TTS, STT, and image upstreams with per-upstream timeouts, headers, model policies, and fallback chains.
- **MCP for AI agents:** a remote `POST /mcp` endpoint with OAuth 2.1 sign-in (add the URL in Claude, ChatGPT, Cursor, or VS Code) and a local `reseller mcp` stdio bridge. Agents can generate content and manage their account. See [docs/mcp.md](docs/mcp.md).
- **Signup and abuse control:** proof-of-work signup (ALTCHA-compatible), per-IP and per-account limits, rotation limits, and an auth-failure lockout.
- **Accounts and keys:** self-service accounts, shared balances, key rotation, model allowlists, and per-key usage limits.
- **Accurate metering:** integer micro-USD accounting with cached-input, per-character, per-image, per-second, and flat per-request prices. Per-request holds stop concurrent requests from overdrawing a balance or limit. Monthly quota and top-up lots, markup overrides, and debt tracking are included.
- **Payments:** subscriptions with a monthly quota, non-expiring top-ups, single-use redeem codes, Stripe Checkout, Payroof crypto payments (stablecoins and native coins on EVM chains), and native App Store (StoreKit 2) and Google Play purchases with verified, idempotent webhooks and refund-abuse protection.
- **White-label branding:** name, copy, logo, favicon, colours, custom CSS, and an optional theme directory.
- **Dashboards:** responsive customer and administrator consoles with linked accounts, keys, and requests, lifetime and 30-day spend rankings, usage charts, a public developer docs page at `/docs/`, and a live statistics API.
- **Storage and packaging:** SQLite in WAL mode. No external database or frontend build is required. The Docker image is non-root and built for AMD64 and ARM64.

## Contents

- [Screenshot](#screenshot)
- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Pricing and metering](#pricing-and-metering)
- [Security model](#security-model)
- [Customer API](#customer-api)
- [Proxying](#proxying)
- [MCP](#mcp)
- [Admin API](#admin-api)
- [CLI reference](#cli-reference)
- [Branding and themes](#branding-and-themes)
- [Payments](#payments)
- [Operations](#operations)
- [Development](#development)
- [License](#license)

## Screenshot

Landing page with the quick-start snippet, self-service API key with proof-of-work signup, and the plan catalogue:

![Landing page with a curl quick-start snippet, Get an API key and Open workspace buttons, and the subscription plan cards](assets/scr.png)

## Quick start

Building from source requires **Rust 1.98 or newer**. The default database is `.reseller/reseller.db` relative to the working directory.

### Option 1: Cargo

1. Install the binary from this checkout. Once the first release is on crates.io, you can use `cargo install reseller --locked` instead.

   ```sh
   cargo install --path . --locked
   ```

2. Write a starter configuration file.

   ```sh
   reseller init
   ```

3. Edit `reseller.toml` and set `[llm].api_key`, plus your upstream URL and model.
4. Start the server.

   ```sh
   reseller
   ```

5. Save the administrator token printed on first start (see [Admin token](#admin-token)).

To skip the file, bootstrap from environment variables on the first start only (see [Environment bootstrap](#environment-bootstrap)):

```sh
OPENAI_BASE_URL=https://openrouter.ai/api/v1 \
OPENAI_API_KEY=sk-or-v1-... \
OPENAI_MODEL=your-model \
reseller
```

### Option 2: Docker

1. Start the container from a published image.

   ```sh
   docker run -d --name reseller --restart unless-stopped \
     -p 56787:56787 \
     -e OPENAI_BASE_URL=https://openrouter.ai/api/v1 \
     -e OPENAI_API_KEY=sk-or-v1-... \
     -e OPENAI_MODEL=your-model \
     -e PUBLIC_URL=https://api.example.com \
     -v reseller-data:/data \
     ghcr.io/reseller-rs/reseller:latest
   ```

2. Read the first-start administrator token from the logs.

   ```sh
   docker logs reseller
   ```

To build locally instead, run `docker build -t reseller:local .`.

### Option 3: Docker Compose

1. Set the variables you need in your shell or in a Compose `.env` file. Supported names include `OPENAI_*`, `TTS_*`, `STT_*`, `IMG_*`, `ADMIN_TOKEN`, `PUBLIC_URL`, `BRAND_NAME`, `STRIPE_*`, and `PAYROOF_*`.
2. Start the bundled stack.

   ```sh
   docker compose up -d
   ```

Pin a release tag instead of `latest` to keep deployments reproducible.

### Container details

| Item | Value |
|---|---|
| Binary | Static musl build with size-first release settings |
| Base image | `scratch`, with the binary, the CA bundle, and an empty writable `/data` |
| Shell and tools | None (no shell, `curl`, or package manager) |
| User | UID/GID `10001` |
| Database | `/data/reseller.db` |
| Healthcheck | `reseller healthcheck`, which probes the local `/readyz` over TCP |
| Shutdown grace period | `330s` in `compose.yaml` (allow up to the upstream timeout) |

Bind-mounted directories must be writable by UID `10001`.

### Local URLs

The defaults assume `localhost` and port `56787`.

| Page or endpoint | URL |
|---|---|
| Landing and create account | `http://localhost:56787/` |
| Developer docs | `http://localhost:56787/docs/` |
| Customer workspace | `http://localhost:56787/workspace/` |
| Administrator console | `http://localhost:56787/admin/` |
| OpenAI-compatible base URL | `http://localhost:56787/v1` |
| MCP endpoint | `http://localhost:56787/mcp` |
| Liveness and readiness | `/healthz` and `/readyz` |

### Admin token

If no `ADMIN_TOKEN` is supplied, the first start generates one and prints it. Save it. You need it to sign in to the administrator console.

- Run `reseller token` to print a random administrator token.
- Run `reseller reset-admin` to replace the stored token. Stop the server first.
- The token also works as an operator credential: `curl https://HOST/v1/models -H "Authorization: Bearer $ADMIN_TOKEN"` and the same on `/v1/*` and `/mcp` generation tools. It is accepted even when `require_key` is on, sees the full model catalog, is never billed, has no limits, and its requests are logged without an account. On `/v1/keys`, `/v1/account`, and the account tools it is answered with `403 admin_token_not_customer` (use `/admin/api`), which does not count as an authentication failure.

## Configuration

`reseller init` writes a starter file (default `reseller.toml`, mode `0600`). It never overwrites an existing file. The file is generated from [`reseller.example.toml`](reseller.example.toml), which is the **full annotated reference** for every setting and default. Unknown keys are rejected.

### How settings are stored

- On **first start only**, the TOML file and the bootstrap environment variables seed SQLite. The file is not read again once the database holds settings.
- Afterwards, **Admin > Settings** is authoritative. To manage settings as a file, use `reseller config export` and `reseller config import` with the server stopped.
- Environment and file edits do not override an existing database.
- CORS changes take effect after a restart. Other settings apply to new requests.
- Secrets are redacted in API responses. A blank secret field keeps the current value.
- Changing `key_pepper` invalidates all customer credentials, unredeemed codes, and outstanding signup challenges.

### Sections at a glance

Defaults appear in parentheses.

| Section | Purpose |
|---|---|
| Top level | `public_url` (`http://localhost:56787`), `admin_token`, `key_pepper` (both at least 24 characters, generated if unset), `model_policy` and `voice_policy` (`default`, `force`, or `passthrough`, default `default`), `markup_pct` (`30`), `require_key` (`true`), `accept_any_token` (`false`), `trust_proxy` (`false`), `cors_origin` (empty), `max_json_body_bytes` (50 MiB), `max_stream_body_bytes` (512 MiB), `upstream_timeout_seconds` (300), `log_retention_days` (30), `max_log_entries` (5000) |
| `[branding]` | White-label name, copy, logos, colours, and CSS (see [Branding and themes](#branding-and-themes)) |
| `[llm]`, `[tts]`, `[stt]`, `[image]` | Upstreams: `base_url` (`https://api.openai.com/v1`), `api_key`, `model`, `fallbacks` (max 10), `timeout_seconds` (0 uses `upstream_timeout_seconds`, max 3600), and `headers` (max 20, credential and framing headers are refused). `[tts]` also has `voice`. |
| `[prices.<model>]` | Upstream prices in USD (see [Pricing and metering](#pricing-and-metering)) |
| `[signup]` | Anonymous signup: `enabled` (`true`), `free_credit_usd` (`"0.10"`), `free_credit_halving` (`true`, when true each further signup from the same IP in a calendar month gets half the previous trial credit, so with `ip_limit_per_month = 4` and `"0.1"` the signups get $0.1, $0.05, $0.025, and $0.0125), `free_credit_expiry_days` (30, 0 means never, max 3650), `ip_limit_per_month` (4, 0 turns it off), `cooldown_seconds` (30, 0 turns it off), `global_hourly_limit` (100, 0 turns it off), `pow_max_number` (100000, 0 disables proof of work, max 10000000), `pow_ttl_seconds` (300, 10 to 86400), `adaptive_pow` (`true`), `pow_max_number_ceiling` (1000000, from `pow_max_number` to 10000000), `adaptive_pow_reset_minutes` (30, 1 to 1440), and `[signup.ip_overrides]` (`"1.2.3.4" = N`, where 0 means unlimited) |
| `[keys]` | `max_per_account` (10 active keys, must be above 0), `max_new_per_day` (20 created or rotated per account in a rolling 24 hours, 0 turns it off), `rotate_cooldown_seconds` (300, 0 turns it off, max 86400), and `[keys.default_limits]` (`rpm`, `requests`, `tokens`, `spend`, `max_children`, `allowed_models`, copied onto each new account's first key, child keys inherit their parent's limits) |
| `[rate_limits]` | `management_per_minute` (120), `auth_failures_per_hour` (20), `max_concurrent_requests` (8), `checkouts_per_hour` (10), `redeem_failures_per_hour` (10), `challenges_per_minute` (10), `new_account_hours` (48) and `new_account_daily_spend_pct` (`"20"`): the new-account spending cap (see [Refund protection](#refund-protection)). 0 disables each one. |
| `[reasoning]` | `free_effort` (`"low"`) and `[reasoning.models."<upstream model>"]` with `efforts` and `default` (see [Reasoning effort](#reasoning-effort)) |
| `[billing]` | `request_estimate_usd` (`"0.001"`, the typical billed cost of one request, used to show customers approximate request counts), `default_max_output_tokens` (4096, held for LLM requests without an output limit), `min_hold_usd` (`"0.01"`, the smallest hold per request), `refund_ip_block_days` (30, 0 turns the address block off), `refund_decline_pct` (50, share of a purchase that must be used before refund answers ask the store to decline) |
| `[mcp]` | `enabled` (`true`), `oauth` (`true`, OAuth sign-in for `/mcp`, see [MCP](#mcp)), and `max_media_bytes` (16 MiB, 1 byte to 256 MiB), the largest audio or image returned inline |
| `[stripe]` | `secret_key`, `webhook_secret` |
| `[[models]]` | Public model catalog: `id`, `upstream`, `aliases`, `enabled`, `name`, `description`, `owned_by`, `created`, `context_length`, `input_modalities`, `output_modalities` (left empty, they derive from the upstream route that serves the model: chat text to text, TTS text to audio, STT audio to text, image text to image), `show_pricing` |
| `[apple]` | App Store purchases: `bundle_id`, `app_apple_id`, `environment` (`production` or `sandbox`), `issuer_id`, `key_id`, `private_key` (the `.p8` PEM), `api_url` (empty uses Apple's host for the environment), `consumption_reporting` (`false`) |
| `[google]` | Google Play purchases: `package_name`, `service_account_json`, `pubsub_audience` (empty uses `<public_url>/webhooks/google`), `pubsub_service_account`, `refund_review` (`false`), `api_url`, `certs_url` |
| `[payroof]` | Crypto payments: `deposits` (`false`), `deposit_eta_minutes` (`5`), `project`, `token`, `webhook_secret`, `stablecoins`, `native_coins`, `chains`, rate source, `quote_ttl_minutes`, and `underpayment_tolerance_pct` (`0.5`) |

### Value formats

- Use decimal **strings** for prices, percentages, and spend-limit amounts, for example `"0.5"`.
- Management endpoints also accept numeric USD amounts for grants, plans, and codes.
- Responses expose money as decimal strings and timestamps as Unix seconds.
- Expiry inputs accept Unix seconds or RFC 3339.

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

These variables seed the settings on **first start only**. They override the TOML file at that point. `ADMIN_TOKEN` and `KEY_PEPPER` must be at least 24 characters. Leave them unset to have secure values generated.

| Group | Variables |
|---|---|
| LLM upstream | `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_MODEL_FALLBACKS` |
| TTS upstream | `TTS_BASE_URL`, `TTS_API_KEY`, `TTS_MODEL`, `TTS_MODEL_FALLBACKS`, `TTS_VOICE` |
| STT upstream | `STT_BASE_URL`, `STT_API_KEY`, `STT_MODEL`, `STT_MODEL_FALLBACKS` |
| Image upstream | `IMG_BASE_URL`, `IMG_API_KEY`, `IMG_MODEL`, `IMG_MODEL_FALLBACKS` |
| Core | `ADMIN_TOKEN`, `KEY_PEPPER`, `MARKUP_PCT`, `FREE_CREDIT_USD` (sets `signup.free_credit_usd`) |
| Public site | `PUBLIC_URL`, `BRAND_NAME` |
| Stripe | `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` |
| Payroof | `PAYROOF_API_URL`, `PAYROOF_PROJECT`, `PAYROOF_TOKEN`, `PAYROOF_WEBHOOK_SECRET` |
| App Store | `APPLE_BUNDLE_ID`, `APPLE_ISSUER_ID`, `APPLE_KEY_ID`, `APPLE_PRIVATE_KEY` |
| Google Play | `GOOGLE_PACKAGE_NAME`, `GOOGLE_SERVICE_ACCOUNT_JSON`, `GOOGLE_PUBSUB_SERVICE_ACCOUNT` |

Fallback lists are comma-separated.

### Runtime environment variables

These variables apply on every start.

| Variable | Default | Purpose |
|---|---|---|
| `HOST` | `0.0.0.0` | Listen address |
| `PORT` | `56787` | Listen port |
| `DB_FILE` | `.reseller/reseller.db` | SQLite database path |
| `RESELLER_CONFIG` | `reseller.toml` | Seed file (read on first start only) |
| `RESELLER_THEME_DIR` | none | White-label theme directory |
| `RUST_LOG` | `reseller=info` | Log filter |
| `RESELLER_URL`, `RESELLER_CREDENTIALS`, `RESELLER_OUTPUT_DIR`, `RESELLER_TOKEN` | see [MCP](#mcp) | Used by `reseller mcp` only |

The application does not read `.env` files. `public_url` controls dashboard links, payment redirects, and the MCP Origin check, so set it to the externally reachable origin. Price-table model names match case-insensitively, and `default` is the fallback entry.

## Pricing and metering

Upstream cost is computed once from the exact decimal sum. It is rounded half-up to whole micro-USD. A positive cost never rounds to zero, so tiny requests cannot be repeated for free.

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

### Price fields

Each `[prices.<model>]` table takes these fields. All values are decimal strings in USD.

| Field | Unit | Default |
|---|---|---|
| `input` | Per 1M input tokens | `0.5` |
| `output` | Per 1M output tokens | `1.5` |
| `cached_input` | Per 1M cached input tokens | `input` price |
| `characters` | Per 1M TTS input characters | `0` |
| `audio_second` | Per second of audio | `0` |
| `image` | Per generated image | `0` |
| `request` | Flat fee per successful request | `0` |

Cached tokens are the tokens the provider reports in `prompt_tokens_details.cached_tokens`, or the equivalent Responses and realtime fields. Reasoning tokens are already part of the provider's output count.

### Billing rules

1. A provider-reported `usage.cost` (USD) replaces the token, audio, image, and character components. This includes an explicit zero. The flat `request` fee is still added for successful requests.
2. Markup resolves in this order: key, then account, then global `markup_pct`.
3. Credit is consumed soonest-expiring first: the current month's subscription quota and trial credit before top-ups, which never expire and come last.
4. Usage, debit, request metadata, and counters commit in one transaction.
5. For `/v1/chat/completions`, `/v1/responses`, and `/v1/completions`, tokens are **estimated** when the upstream reports no usage at all. The estimate uses about four characters per token, based on the request text and the generated text. The request row is flagged `estimated: 1`. MCP generation results report this as `usage_estimated`.
6. For other routes, if a provider returns no usage and no supported audio duration, only the flat `request` fee applies. Use providers that report usage.

Every request row records the full breakdown behind its charge: `tokens`, `input_tokens`, `cached_tokens`, `output_tokens`, `characters`, `audio_ms`, `images`, `upstream_usd`, `billed_usd`, and `estimated`. Customers audit each line with everything except `upstream_usd`: customer request and usage views never show the provider cost. Administrators see all fields.

Each key also keeps its lifetime billed spend (`billed_usd`), which outlives request log and usage retention. An account's lifetime spend is the sum over its keys.

### Holds, limits, and debt

Every admitted request **holds** its worst-case billed cost until it settles. The hold is recorded on the request row (`hold_micro`, `hold_tokens`) in the same transaction that admits it:

| Route | Held usage |
|---|---|
| `/v1/chat/completions`, `/v1/completions`, `/v1/responses` | Estimated input tokens plus `max_completion_tokens`, `max_tokens`, or `max_output_tokens` (else `billing.default_max_output_tokens`) for each of `n` choices |
| `/v1/embeddings` | Estimated input tokens |
| `/v1/images/*` | `n` images |
| `/v1/audio/speech` | The input characters |
| Uploads, realtime sessions, other routes | Nothing beyond the minimum |

The held usage is priced like real usage, at the caller's markup, and never below `billing.min_hold_usd`.

- **Admission.** The balance minus the holds of the account's requests in flight must cover the new hold. Key `requests`, `tokens`, and `spend` windows count settled usage plus in-flight holds plus the new request. So concurrent requests cannot overdraw the balance or overshoot a limit, whatever `max_concurrent_requests` allows.
- **Settlement.** Settlement debits the actual cost and closes the row, which releases the hold. Each row settles exactly once (`finished=0` guard) and is retried in the background if the database is busy. Rows left unfinished by a crash are closed at startup and release their holds.
- **Residual overrun.** A single request can still cost more than its hold:
  - when the client set no output limit and the reply is longer than `default_max_output_tokens`
  - when a provider-reported `usage.cost` is higher than the price table
  - for realtime sessions and uploads, whose usage is unknown up front.

  Such an overrun becomes account debt, and holds stop concurrency from multiplying it.
- Uncovered spend becomes account debt. Future top-ups settle that debt before they become spendable credit.
- Request admission and RPM checks are serialized in SQLite. Hourly usage windows include the entire boundary hour.
- A completed admitted request counts, even if the upstream failed.
- Model usage is aggregated for up to 367 days. Detailed request retention is bounded separately by `log_retention_days` and `max_log_entries`.

### Stream and WebSocket inspection

- SSE inspection works across chunk boundaries with bounded memory.
- JSON bodies up to 4 MiB are parsed whole. Larger bodies (for example base64 images) keep only their last 256 KiB. The trailing `usage` object is read from that tail, and inline images are counted as they stream.
- WAV and 16-bit PCM durations can be estimated. The rate and channels come from the content type, with a default of 24 kHz mono.
- WebSocket sessions are authenticated, admitted, and logged like other requests. Usage from each realtime `response.done` event is summed and billed when the session ends.
- A session holds only `billing.min_hold_usd` at admission, so a long session can run into debt. Sessions are bounded by `upstream_timeout_seconds`.
- Upstream costs for failed fallback attempts cannot be recovered unless the provider reports them in the final response.

## Security model

### Credentials

Every key has two secrets. Each is shown only once.

| Secret | Format | Access |
|---|---|---|
| API key | `sk-res-<64 hex>` | Full access: proxy and account management |
| Dashboard token | `dash_<64 hex>` | Account management only. It **cannot call the proxy**, which fails with `401 dashboard_token_not_allowed`. |

Both secrets are stored only as HMAC-SHA256 digests keyed with `key_pepper`. Redeem codes are hashed the same way.

Send credentials as `Authorization: Bearer ...` or `x-api-key`. Tokens in query strings are not supported. The administrator token goes in `x-admin-token` or the bearer header, and it is compared in constant time. Blocked or expired API keys keep read-only workspace access (and their dashboard tokens too): they cannot create, rotate, or edit keys (`403 key_inactive`). Revoked credentials get no access.

### Signup with proof of work

Anonymous signup uses `GET /v1/signup/challenge`, then `POST /v1/signup`. The challenge is an ALTCHA v1 (SHA-256) puzzle:

```json
{"algorithm":"SHA-256","challenge":"<hex>","maxnumber":100000,
 "salt":"<hex>?expires=<unix>&max=100000","signature":"<hex>"}
```

To solve it, the client finds the integer `number` in `0..=maxnumber`, where `hex(sha256(salt + decimal(number)))` equals `challenge`. It then submits `base64(JSON{algorithm, challenge, number, salt, signature})` as `altcha`.

- `signature` is an HMAC-SHA256 under a key derived from `key_pepper`, so the server keeps no challenge state.
- Each solution is single use. It is recorded in `pow_spent` and purged after expiry.
- A solution expires after `pow_ttl_seconds`.
- A solution is consumed only when the account is actually created. A rate-limited attempt does not burn it.
- With `signup.pow_max_number = 0`, the challenge endpoint returns `{"required":false}`, and `altcha` may be omitted.
- The bundled web UI and the `reseller mcp` bridge solve the puzzle for you.

**Adaptive difficulty** (`signup.adaptive_pow`, on by default) raises the cost of guessing from one address:

- **What counts.** Each business failure from a client IP doubles the `maxnumber` of that IP's next challenge, starting from `pow_max_number`: a rejected signup (an invalid proof or a validation error), an invalid API key or dashboard token, or an invalid redeem code.
- **What does not count.** Server errors, network failures, and rate limits (`429`) never count. Neither does a missing or merely outdated proof (`pow_required`, `pow_too_easy`).
- **Ceiling.** The difficulty never exceeds `pow_max_number_ceiling` (default 1000000). That stays solvable in seconds on older phones, so a real user who mistypes a few times is slowed, not locked out.
- **Reset.** A successful signup resets the IP's difficulty at once, and so do `adaptive_pow_reset_minutes` (default 30) without a new failure.
- **Enforcement.** The difficulty is part of the signed salt (`max=`). A solution is accepted only when its challenge is at least as hard as the submitting IP currently needs. Otherwise the request gets `400 pow_too_easy`, and the client fetches a new challenge. So a cheap challenge fetched earlier, or from another address, does not help.
- **State.** Counters are in memory per process, and a restart forgives them.
- **Turning it off.** Set `adaptive_pow = false`, for example for load tests or during abnormal traffic spikes, to keep the difficulty fixed.

```sh
# Solve with any ALTCHA v1 client. The commands show the HTTP shape only.
curl http://localhost:56787/v1/signup/challenge
curl http://localhost:56787/v1/signup \
  -H 'Content-Type: application/json' \
  -d '{"name":"my-app","contact":"me@example.com","altcha":"<base64 solution>"}'
```

Signup errors:

| Status | Code | Cause |
|---|---|---|
| 400 | `pow_required` | No solution sent |
| 400 | `pow_invalid` | Malformed, bad signature, expired, number out of range, or wrong solution |
| 409 | `pow_replayed` | Solution already used |
| 403 | `signup_disabled` | Signup is turned off |
| 429 | see [Rate limits](#rate-limits) | Guard limits |

### Rate limits

Every `429` response carries a `Retry-After` header in seconds, computed from the limit that was hit:

- **Throttles.** These are request rate, concurrency, auth failures, checkouts, cooldowns, and requests in flight holding the balance (`balance_reserved`). The wait is short and exact, and a retry after it succeeds.
- **Usage windows.** These are the key `requests`, `tokens`, and `spend` windows, the monthly per-IP allowance, and the new-account daily cap. `Retry-After` is the time until enough settled usage leaves the window (or until next month or the account's 48th hour). These responses also send `x-should-retry: false`, so the OpenAI and Anthropic SDKs do not retry them right away. Show the error, or schedule the request after `Retry-After`.

| Limit | Scope | Config | 429 `code` |
|---|---|---|---|
| Management requests (`/v1/signup`, `/v1/keys`, `/v1/account`, `/v1/plans`, `/mcp`, `/oauth/*`, `/admin/api/*`) | Client IP, per minute | `rate_limits.management_per_minute` | `management_rate_limited` |
| Signup challenges | Client IP, per minute | `rate_limits.challenges_per_minute` | `challenge_rate_limited` |
| Invalid credentials | Client IP, per hour | `rate_limits.auth_failures_per_hour` | `too_many_auth_failures` |
| Invalid redeem codes | Account, per hour | `rate_limits.redeem_failures_per_hour` | `redeem_rate_limited` |
| Checkout attempts | Account, per hour | `rate_limits.checkouts_per_hour` | `checkout_rate_limited` |
| In-flight proxy requests | Account | `rate_limits.max_concurrent_requests` | `concurrency_limit_exceeded` |
| Self-service key creations per calendar month | IP | `signup.ip_limit_per_month`, `signup.ip_overrides` | `ip_key_limit` |
| Gap between creations | IP | `signup.cooldown_seconds` | `creation_cooldown` |
| Key creations (signup, child keys, rotations) per hour | Global | `signup.global_hourly_limit` | `creation_rate_limited` |

| Keys created or rotated per rolling 24 hours | Account (customers, admin exempt) | `keys.max_new_per_day` | `key_creation_daily_limit` |
| Minimum key age before rotation | Key (customers, admin exempt) | `keys.rotate_cooldown_seconds` | `rotate_cooldown` |

| Requests per minute | Key | `rpm` | `rate_limit_exceeded` |
| Requests per window | Key | `requests` | `request_limit_exceeded` |
| Tokens per window | Key | `tokens` | `token_limit_exceeded` |
| Spend per window | Key | `spend` | `spend_limit_exceeded` |
| Spend per rolling 24 hours of a new account | Account | `rate_limits.new_account_hours`, `rate_limits.new_account_daily_spend_pct` | `new_account_daily_limit` |
| Balance held by requests in flight | Account | none | `balance_reserved` |

Counts that only drop when something is removed are not rate limits and are not 429s. `account_key_limit` (`keys.max_per_account`) and `child_key_limit` (`max_children`) are `403` without `Retry-After`: revoke a key to create another.

A request whose own hold can never fit a key window (for example a 4096-token hold under a 1000-token limit) is a `400` with the window's code and `param: "max_tokens"`.

### Error status changes in 0.12

| Case | Before | Now | Why |
|---|---|---|---|
| Empty balance, or a hold larger than the balance (`insufficient_quota`) | `429`, `Retry-After: 60` | `402`, no `Retry-After` | Waiting never helps. Only a top-up or a smaller request does, and SDKs do not retry 402. |
| Key `spend`, `tokens`, `requests` windows | `429`, `Retry-After: 60` | `429`, exact `Retry-After`, `x-should-retry: false` | The window frees up hours later, not in 60 s |
| Requests per minute | `429`, `Retry-After: 60` | `429`, seconds until the oldest counted request is a minute old | Exact wait |
| Monthly per-IP key allowance | `429`, `Retry-After: 60` | `429`, seconds until next month (UTC), `x-should-retry: false` | Exact wait |
| `account_key_limit`, `child_key_limit` | `429`, `Retry-After: 60` | `403`, no `Retry-After` | A count cap, not a rate |
| Upstream `insufficient_quota` (the operator's provider account is out of credit) | Passed through as `429` | `502 upstream_error` | It is not the caller's quota |
| Upstream status rewritten (for example 401 to 502) | Kept the upstream `Retry-After` | Header dropped | The hint belonged to the upstream status |

Accounts that are subscribed or have `can_create_keys` set bypass the per-IP monthly allowance and the cooldown. They do not bypass the global, daily, or account key caps. The in-memory limiter state is per process and is not persisted, so a restart only relaxes limits.

### Auth-failure lockout

Each invalid API key, dashboard token, administrator token, or redeem code charges the client IP's auth-failure budget. The budget is a token bucket. It allows a burst of `auth_failures_per_hour`, then one more attempt every `3600 / limit` seconds.

When the budget is exhausted, **every request from that IP that carries a credential** gets `429 too_many_auth_failures`. This applies to `/v1/*`, `/mcp`, and `/admin/api/*`. The check runs before any database lookup, for at most one hour. Revoked or blocked keys and other non-guessing errors do not count.

### Origin and response headers

- `POST /mcp` refuses requests whose `Origin` header does not match `public_url`, `cors_origin`, or any origin when `cors_origin = "*"`. The response is `403 origin_not_allowed`. Requests without an `Origin` header (CLI and desktop agents) are accepted. This blocks DNS-rebinding attacks through a browser.
- Every response carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, and a strict `Content-Security-Policy` with `frame-ancestors 'none'`.
- `Strict-Transport-Security` is added when `public_url` starts with `https://`.
- `/.well-known/*`, `/oauth/token`, `/oauth/register`, and `/oauth/revoke` answer CORS for any origin (`Access-Control-Allow-Origin: *`, no credentials) whatever `cors_origin` is. The consent API (`/oauth/api/authorize`) accepts same-origin requests only. OAuth codes and tokens are stored as HMAC digests, and registration is limited to 30 clients per IP per hour.
- When `cors_origin` is set (`*` or one exact origin), `WWW-Authenticate` is exposed on `/mcp`, and CORS allows the `Authorization`, `Content-Type`, `x-api-key`, `x-admin-token`, `mcp-protocol-version`, `mcp-method`, and `mcp-name` headers.
- The customer UI uses session storage, removes login tokens from URL fragments, and never embeds credentials in page HTML.
- Logs contain request metadata only. They never contain prompts, bodies, audio, auth headers, or query strings.

### Behind a proxy

Place a TLS reverse proxy in front of public deployments.

- `trust_proxy` defaults to off. The TCP peer address is then the client IP.
- Enable it **only** when direct access to the application is blocked and your edge overwrites incoming forwarding headers. It trusts `CF-Connecting-IP`, then the **last** `X-Forwarded-For` address. Earlier entries are controlled by the client.
- If `trust_proxy` is enabled wrongly, clients can spoof their IP and evade every per-IP limit.
- If it is disabled behind a proxy, all users share one IP. The per-IP limits (signup allowance, cooldown, and lockout) then apply to everyone at once.

### Database secrets

Settings contain upstream and payment secrets that must be recoverable to proxy requests. Protect the database and its backups as credentials.

## Customer API

Customer routes use `Authorization: Bearer <api key or dashboard token>` unless marked public. Errors use the OpenAI shape `{"error":{"message","type","param","code"}}`. List endpoints default to 50 items and cap at 200.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET` | `/v1/plans` | Public | Active plans and `payment_methods` |
| `GET` | `/v1/signup/challenge` | Public | Proof-of-work challenge |
| `POST` | `/v1/signup` | Public | Body `{"name"?, "contact"?, "altcha", "accept_terms"?}`. Creates an account and its first key. `accept_terms: true` records acceptance of the Terms of Service and Privacy Policy. |
| `GET` | `/terms`, `/privacy` | Public | Built-in Terms of Service and Privacy Policy pages |
| `POST` | `/v1/keys` | Key or token (not anonymous) | Body `{"name", "limits"?}`. Creates a child key on the caller's account. The child inherits the caller's limits. Limits you send replace single fields and must be no looser than the parent's (a lower or equal count, a window at least as long, a subset of its model allowlist), otherwise `400 limits_exceed_parent`. Blocked or expired callers get `403 key_inactive`. Returns `401` when anonymous. |
| `GET` | `/v1/keys?q=&status=&sort=&dir=` | Any | List the caller's keys with `billed_usd` (lifetime spend) and `usage_30d_usd`. `sort` is `created`, `name`, `status`, `last_used`, `spend`, or `usage`. |
| `GET` | `/v1/keys/:id` | Any | One of the caller's keys with its 30-day usage series |
| `PATCH` | `/v1/keys/:id` | Any | Rename or block (`{"name"?, "status":"blocked"}`). Customers cannot reactivate a key. |
| `POST` | `/v1/keys/:id/rotate` | Any | Replace the key and keep its limits, pricing, expiry, usage windows (so rotating does not reset limits), and connected OAuth apps. The old key is blocked. |
| `GET` | `/v1/models` | Key | Model catalog, local and not metered. `architecture` carries modalities and a `capability` (`chat`, `speech`, `transcription`, `image`) |
| `GET` | `/v1/models/:id` | Key | One catalog entry, or `404 model_not_found` |
| `GET` | `/v1/account/models` | Any | The `/v1/models` list for the caller's key, also with a dashboard token |
| `GET` | `/v1/account` | Any | Account, balance (`balance_usd`, split into `quota_remaining_usd` and `topup_remaining_usd`), lifetime `billed_usd`, active `subscriptions` with their monthly quota, credit lots, `consumption_consent`, and `top_up` instructions |
| `PATCH` | `/v1/account` | Any | Body `{"consumption_consent": false}` withdraws (or `true` grants again) the consent to share consumption data with app stores for refund requests |
| `GET` | `/v1/account/store` | Any | Native app purchase setup: `app_account_token` (App Store) and `obfuscated_account_id` (Google Play) for this account, store settings, and plans with store product ids |
| `POST` | `/v1/account/store/apple` | Any | Body `{"signed_transaction":"<JWS>"}` or `{"transaction_id":"..."}` after a StoreKit 2 purchase. Returns the account and `purchase: {order_id, credited}` |
| `POST` | `/v1/account/store/google` | Any | Body `{"product_id","purchase_token"}` after a Play Billing purchase. Same response |
| `GET` | `/v1/account/usage?days=30&key=` | Any | Daily and per-model usage (`days` 1 to 365) |
| `GET` | `/v1/account/requests?limit=20&offset=0&key=&model=&kind=&status=` | Any | Request history. Rows carry `key_name` and `key_prefix`. |
| `POST` | `/v1/account/redeem` | Any | Body `{"code":"RES-..."}` |
| `POST` | `/v1/account/checkout` | Any | Buy a plan or top-up by card or crypto. Body `{"plan_code":"monthly","method":"stripe"}` or `"crypto"` |
| `GET` | `/v1/account/payments` | Any | The caller's orders (latest 50) |
| `GET` | `/v1/account/payments/:id` | Any | One order, with crypto quotes and transfers |
| `GET` | `/v1/account/apps` | Any | Connected OAuth apps: `{"apps":[{id, client_id, client_name, client_uri, logo_uri, key_id, key_prefix, key_name, scope, created_at, last_used_at}]}` |
| `DELETE` | `/v1/account/apps/:id` | Any | Revokes a connected app and its tokens. Returns `{"revoked":true,"id"}`. |
| `POST` | `/webhooks/stripe` | Stripe signature | Raw Stripe event |
| `POST` | `/webhooks/payroof` | Payroof signature | Payroof transfer event |
| `POST` | `/webhooks/apple` | Apple-signed JWS | App Store Server Notifications V2 |
| `POST` | `/webhooks/google` | Google OIDC token | Google Play Real-time Developer Notifications (Pub/Sub push) |

### Keys

```sh
# Sign up after solving the challenge. The response holds key, dashboard_token,
# dashboard_url, api_base, and limits.
curl -X POST http://localhost:56787/v1/signup -H 'Content-Type: application/json' \
  -d '{"name":"my-app","altcha":"<base64 solution>","accept_terms":true}'

export RESELLER_KEY=sk-res-...

# Create another key on the same account
curl -X POST http://localhost:56787/v1/keys \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"name":"ci"}'

curl http://localhost:56787/v1/keys -H "Authorization: Bearer $RESELLER_KEY"

# Rotate. Returns 429 rotate_cooldown while the key is younger than keys.rotate_cooldown_seconds.
curl -X POST http://localhost:56787/v1/keys/KEY_ID/rotate -H "Authorization: Bearer $RESELLER_KEY"
```

A new key (from signup, a child key, or a rotation) is returned once, as `{id, account_id, key, key_prefix, dashboard_token, dashboard_url, api_base, limits, notice}`. All keys on an account share its credit.

### Account, redeem, and checkout

```sh
curl http://localhost:56787/v1/account -H "Authorization: Bearer $RESELLER_KEY"
curl 'http://localhost:56787/v1/account/usage?days=7' -H "Authorization: Bearer $RESELLER_KEY"
curl 'http://localhost:56787/v1/account/requests?limit=20&status=2xx' -H "Authorization: Bearer $RESELLER_KEY"

# Returns 400 invalid_code when the code is unknown, expired, or already used
curl -X POST http://localhost:56787/v1/account/redeem \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"code":"RES-XXXX-XXXX"}'

curl http://localhost:56787/v1/plans
# Returns {"method","url","order_id"}
curl -X POST http://localhost:56787/v1/account/checkout \
  -H "Authorization: Bearer $RESELLER_KEY" -H 'Content-Type: application/json' \
  -d '{"plan_code":"monthly","method":"stripe"}'
curl http://localhost:56787/v1/account/payments -H "Authorization: Bearer $RESELLER_KEY"
```

## Proxying

```sh
curl http://localhost:56787/v1/chat/completions \
  -H "Authorization: Bearer $RESELLER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"your-model","messages":[{"role":"user","content":"Hello"}],"stream":true}'
```

### Routing

Routing replaces `/v1` with the configured base path and keeps the query string.

| Client path | Upstream |
|---|---|
| `/v1/audio/speech` | TTS |
| `/v1/audio/transcriptions`, `/v1/audio/translations` | STT |
| `/v1/images`, `/v1/images/*` (generations, edits, variations) | Image |
| All other proxied `/v1/*` routes (chat completions, completions, embeddings, moderations, responses, realtime) | LLM |

`/v1/models`, `/v1/keys`, `/v1/account`, `/v1/plans`, and `/v1/signup` are handled locally and are never proxied. Other paths return `404 unknown_url`, and a wrong method returns `405 method_not_allowed` with an `Allow` header.

### Models

`GET /v1/models` lists the catalog in the OpenAI format. It needs a key, is not metered, and returns `402 insufficient_quota` when the balance is empty. The list never shows upstream names, URLs, keys, or raw prices. Disabled entries and models the key may not use are hidden, and aliases are not listed.

```toml
[[models]]
id = "assistant"
upstream = "vendor/large-model-2026"
aliases = ["gpt-4o"]
name = "Assistant"
context_length = 128000
show_pricing = true
```

With a catalog, a client model is resolved by id or alias and rewritten to `upstream`. An unknown, disabled, or disallowed model returns `404 model_not_found`. Filled and forced models skip this step. Without a catalog, the list shows the configured route models. Pricing, when shown, uses the caller's effective markup.

### Model and voice policies

- `default`: omit `model` to use the configured `[llm].model`. A client value is kept.
- `passthrough`: forwards the client's value unchanged. A wrong model fails upstream.
- `force`: always uses the configured model.

The same policy governs the multipart `model` field on uploads. `force` replaces it, and `default` fills it only when it is missing. The audio payload still streams without buffering. WebSocket connections apply the policy to the `?model=` query parameter before admission and forwarding.

Fallbacks apply only when the final model equals the route's configured model.

### Image requests

```sh
curl http://localhost:56787/v1/images \
  -H "Authorization: Bearer $RESELLER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"meta/muse-image","prompt":"Make the sky orange",
       "input_references":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,..."}}]}'
```

JSON requests apply model and voice policies. Chat requests stream usage reporting. Multipart uploads stream with bounded size, and only the `model` form field is rewritten by the policy.

### Reasoning effort

Clients set the reasoning level as `reasoning_effort` (Chat Completions) or `reasoning.effort` (Responses API, and the OpenRouter chat form). Supported levels differ by provider and model, and an upstream may reject an unknown level with a 400 or ignore it. So the `[reasoning]` section declares them per upstream model and enforces them before forwarding:

```toml
[reasoning]
free_effort = "low"      # cap for free accounts; "" disables

[reasoning.models."deepseek/deepseek-v4.1-flash-1017"]
efforts = ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
default = ""             # applied when a request names no level; "" keeps the provider default

[reasoning.models."my-plain-model"]
efforts = []             # no reasoning control: the parameter is removed
```

| Case | What is forwarded |
|---|---|
| Configured model, supported level | The level, in the route's own form (`reasoning_effort` for chat, `reasoning.effort` for `/v1/responses` or when the client used it) |
| Configured model, unsupported level | Nothing. The request fails with `400 unsupported_reasoning_effort`, which lists the supported levels. |
| Configured model, no level | `default`, if set |
| Model configured with `efforts = []` | The parameter is removed |
| Model not configured | The client's value, unchanged |
| Fallback model | The original level, or the nearest lower level the fallback supports (never a 400) |

**Free accounts** have no active subscription and no spendable credit beyond signup trial credit. They are capped at `free_effort` (default `"low"`): a higher level is lowered to the highest supported level at or below the cap, and a configured reasoning model gets that level even when the request names none. For an unconfigured model, only an explicitly higher level is lowered. Operator and anonymous requests are not capped.

Levels are ordered `none < minimal < low < medium < high < xhigh < max`. Configure each model's real levels: OpenRouter, for example, accepts all seven for DeepSeek models and answers others with a 400. A model-name suffix such as `model#low` is not used. Suffixes would split one model into several names for prices, allowlists, fallbacks, and `/v1/models`, while clients already have a standard field for the level.

### Fallbacks

- Fallbacks apply only to JSON requests that use the configured model, and only before forwarding response headers.
- Availability errors (`400`, `402`, `404`, `408`, `429`, and `5xx`) and connection or timeout failures can trigger the next model.
- Fallback models must also be allowed by the key.
- Client-selected models are not replaced by fallbacks.
- Multipart uploads get a single forward attempt.
- Redirects are not followed.
- Hop-by-hop headers and local credentials are removed before forwarding.

### Response headers

| Header | Meaning |
|---|---|
| `X-Request-Id` | Request identifier |
| `X-Key-Prefix` | Prefix of the key used |
| `X-Account-Balance-USD` | Balance before the request (pre-request snapshot) |
| `X-Model-Fallback` | Set when a fallback model answered |

### Proxy errors

Failures use the OpenAI shape `{ "error": { "message": "...", "type": "...", "param": null, "code": "..." } }`. The `type` is `invalid_request_error` (400, 401, 404, 405, 409, 413, 415), `permission_error` (403), `insufficient_quota` (402, code `insufficient_quota`), `rate_limit_error` (429), or `server_error` (5xx). `param` is `"model"` for model errors. Upstream errors are rebuilt in this shape: a JSON error keeps only its `message`, `type`, `param`, and `code` (other fields such as upstream user ids or provider metadata are dropped), a non-JSON body gets a generic message with code `upstream_error`, and an upstream 401, 402, or 403, or an upstream `insufficient_quota` (problems with the operator's upstream account, not the caller's key), becomes `502 upstream_error` without the upstream `Retry-After`. Other upstream 429s pass through with their `Retry-After`.

| Status | Codes |
|---|---|
| 401 | `missing_api_key`, `invalid_api_key`, `key_revoked`, `key_blocked`, `key_expired`, `dashboard_token_not_allowed` |
| 403 | `account_suspended` |
| 404 | `model_not_found`, `unknown_url` |
| 405 | `method_not_allowed` |
| 413 | Body too large |
| 402 | `insufficient_quota`: the balance is empty or smaller than the request's hold (lower `max_tokens` or top up) |
| 429 | Limits and throttles, always with `Retry-After` (see [Rate limits](#rate-limits)) |
| 502, 504 | Upstream failure |
| 503 | `proxy_configuration_error` when the route's upstream has no API key |

## MCP

Agents can use the service through the Model Context Protocol. There are two ways to connect.

- **Remote endpoint:** a stateless Streamable HTTP endpoint at `POST /mcp`. It supports protocol `2026-07-28` (current) and the legacy `2025-11-25` handshake. An `initialize` that asks for an older revision is answered with `2025-11-25`. Requests that name an older revision in `MCP-Protocol-Version` are refused. Every request needs a credential: without one the answer is `401` with a `WWW-Authenticate` header that points OAuth clients to `/.well-known/oauth-protected-resource/mcp`.
- **Local bridge:** `reseller mcp` runs next to the agent. It keeps credentials and the signup proof of work on the agent's machine.

**Connect with OAuth.** Add `https://HOST/mcp` as a remote MCP server in Claude, ChatGPT, Cursor, or VS Code. The client registers itself (RFC 7591), and a browser page on this server asks you to sign in with an API key or dashboard token (or create an account) and to allow the connection. It uses PKCE `S256`, resource indicators, one-hour access tokens (`mcp_at_...`), and rotating refresh tokens that expire after 90 days idle. Tokens are bound to your key, stop working when the key does, and are valid on `/mcp` only. Customers revoke apps in the dashboard. `[mcp].oauth = false` turns this off. **Static bearer alternative:** send an API key or dashboard token as `Authorization: Bearer ...`. The administrator token works for generation tools only.

Generation tools go through the same admission, limits, metering, and model policy as the HTTP API. Set `[mcp].enabled = false` to turn the endpoint off. It then answers `404 mcp_disabled`.

```sh
# Remote with a static key (clients without OAuth support). OAuth clients just need the URL.
claude mcp add --transport http reseller https://api.example.com/mcp \
  --header "Authorization: Bearer sk-res-..."

# Local bridge, which can sign up
claude mcp add reseller -- reseller mcp --url https://api.example.com
```

See [docs/mcp.md](docs/mcp.md) for client configuration, the tool reference, the signup walkthrough, the error codes, and security notes.

## Admin API

All routes live under `/admin/api`. They require the administrator token, sent as `x-admin-token` or as a bearer token. List endpoints default to 50 items and cap at 200. Account credit history, codes, and payments return the latest 200.

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/overview` | Lifetime totals and margin. `outstanding_usd` is the unexpired credit of active accounts. Credit of suspended accounts is `suspended_credit_usd`, uncovered spend is `debt_usd`. `active_keys` counts keys that can call the API now. |
| `GET` | `/usage?days=30` | Daily and per-model series |
| `GET` | `/events` | Server-sent snapshot every five seconds |
| `GET` | `/keys?q=&status=&account=&sort=&dir=&limit=&offset=` | List keys with `billed_usd`, `usage_30d_usd`, and `account_contact`. `sort` adds `account`, `spend`, and `usage` to the customer sorts. |
| `GET` | `/keys/:id` | One key with its account and 30-day usage |
| `PATCH` | `/keys/:id` | Update name, status, limits, `markup_pct`, or `expires_at` |
| `POST` | `/keys/:id/rotate` | Rotate (exempt from customer rotation limits) |
| `POST` | `/keys/:id/revoke` | Revoke a key |
| `GET` | `/accounts?q=&status=&sort=&dir=&limit=&offset=` | List accounts with balance, `debt_usd`, `billed_usd`, `usage_30d_usd`, and key counts. `sort` is `created`, `contact`, `status`, `balance`, `keys`, `spend`, or `usage`. |
| `GET` | `/accounts/:id?key_limit=&key_offset=&key_sort=&key_dir=` | One account with a page of its keys and 30-day usage |
| `PATCH` | `/accounts/:id` | Update status, contact, note, `markup_pct`, or `can_create_keys` |
| `POST` | `/accounts/:id/keys` | Create a key with a name and limits (bypasses creation guards) |
| `POST` | `/accounts/:id/credits` | Add credit: `amount_usd`, `source`, optional `expires_at` and `note`. Requires an `Idempotency-Key` header (see below) |
| `POST` | `/accounts/:id/subscriptions` | Grant a plan (`plan_code`): a subscription starting now, or a top-up. Requires an `Idempotency-Key` header |
| `GET` | `/plans` | List plans |
| `POST` | `/plans` | Upsert a plan: `code`, `name`, `kind` (`subscription` or `topup`), `price_usd`, `monthly_quota_usd` and `months` (1 to 12) for a subscription or `credit_usd` for a top-up, optional `apple_product_id`, `google_product_id` (null clears, `409 product_id_in_use` when taken), `active`, and `description` |
| `DELETE` | `/plans/:code` | Delete only while unreferenced. Otherwise deactivate. |

| `GET` | `/codes` | List redeem codes |
| `POST` | `/codes` | Create one code, or an array of up to 100. Each item takes `credit_usd` or `plan_code`, plus optional `expires_at` and `note`. |
| `PATCH` | `/codes/:id` | Update `expires_at` or `note` (unredeemed codes only) |
| `DELETE` | `/codes/:id` | Cancel an unredeemed code |
| `GET` | `/payments` | Orders, filtered by `q`, `status`, `account`, `sort`, `dir`, `limit`, and `offset` |
| `GET` | `/payments/:id` | One order, with quotes and transfers |
| `POST` | `/payments/:id/complete` | Mark a pending or expired order as paid (grants once) |
| `GET` | `/payroof` | Payroof configuration, catalog, and webhooks |
| `POST` | `/payroof/webhook` | Register `{public_url}/webhooks/payroof` |
| `GET` | `/requests` | Request log with `key_name`, `key_prefix`, and `account_contact`, filtered by `q`, `model`, `kind`, `status`, `path`, `ip`, `method`, `key`, `account`, `hour`, `limit`, and `offset` |
| `GET` | `/requests/groups?field=model` | Group requests by `model`, `ip`, `status`, `kind`, `path`, `key`, or `hour` |
| `GET` | `/settings` | Redacted configuration |
| `PATCH` | `/settings` | Validated partial configuration update |

**Idempotent grants.** `POST /accounts/:id/credits` and `POST /accounts/:id/subscriptions` require an `Idempotency-Key` header: 1 to 255 visible ASCII characters, unique per intended grant (a UUID works). Send the same key when retrying after a timeout or network error.

- The first request with a key applies the grant.
- A repeat with the same key and the same body applies nothing. It returns `200` with the original grant id and `"replayed": true` (in `credit` or `grant`).
- The same key with a different body is `422 idempotency_key_reused`.
- A missing key is `400 idempotency_key_required`.

Keys are scoped to the account and are kept with the credit lot or subscription. Concurrent retries are serialized by the database, so at most one of them applies.

## CLI reference

`reseller` with no subcommand runs `serve`. The global options below are valid before or after the subcommand.

### Global options

| Option | Environment variable | Default | Meaning |
|---|---|---|---|
| `--config <FILE>` | `RESELLER_CONFIG` | `reseller.toml` | Seed file, read on first start only |
| `--database <FILE>` | `DB_FILE` | `.reseller/reseller.db` | SQLite database |
| `--host <IP>` | `HOST` | `0.0.0.0` | Listen address |
| `--port <PORT>` | `PORT` | `56787` | Listen port |
| `--theme-dir <DIR>` | `RESELLER_THEME_DIR` | none | White-label files, served under `/theme/` |

### Subcommands

| Subcommand | Description |
|---|---|
| `serve` | Start the proxy and dashboards (default) |
| `init` | Write a documented starter configuration. Never overwrites a file. |
| `doctor` | Validate configuration and database. Prints upstream, signup, Stripe, Payroof, branding, and theme status. |
| `token` | Print a random administrator token |
| `reset-admin` | Replace the stored administrator token (stop the server first, then restart) |
| `backup <DEST>` | Online SQLite snapshot with `VACUUM INTO`. Refuses to overwrite. |
| `healthcheck` | Probe the local `/readyz` (used as the container health check) |
| `config export [--include-secrets]` | Print stored settings as TOML. Secrets are blanked unless requested. |
| `config import <FILE>` | Validate and replace stored settings. Blank secrets keep their stored values. Stop the server first, then restart. |
| `mcp --url <ORIGIN> [--credentials <FILE>] [--output-dir <DIR>] [--token <TOKEN>]` | Local stdio MCP bridge. See [docs/mcp.md](docs/mcp.md). |

### MCP bridge environment

| Variable | Default | Meaning |
|---|---|---|
| `RESELLER_URL` | Required | Server origin (same as `--url`) |
| `RESELLER_CREDENTIALS` | `$XDG_CONFIG_HOME/reseller/credentials.json`, else `~/.config/reseller/credentials.json` | Credential file |
| `RESELLER_OUTPUT_DIR` | `reseller-output` | Where generated audio and images are saved |
| `RESELLER_TOKEN` | none | A key or dashboard token used without storing it |

### Examples

```sh
reseller --host 127.0.0.1 --port 8080
reseller --database /srv/reseller/reseller.db serve
reseller doctor
reseller backup ./reseller-backup.db
reseller config export > settings.toml
reseller --theme-dir ./theme
```

## Branding and themes

Customer-facing pages can be rebranded without rebuilding. Edit `branding` in **Admin > Settings**, or edit `[branding]` in the seed file.

| Setting | Effect |
|---|---|
| `name`, `tagline`, `description` | Header, footer, page title (`name - tagline`), and meta description |
| `logo_url`, `favicon_url` | A site path (for example `/theme/logo.svg`) or an `http(s)` URL. Empty uses the built-in mark. |
| `theme_color` | Browser UI colour |
| `color_scheme`, `theme_toggle` | Default theme for visitors who have not chosen one (`system` follows the device, or `dark` or `light`, default `system`), and whether the header shows the light/dark toggle (default `true`). The choice is kept in the browser's local storage. |
| `legal_name`, `contact_email` | Operator name and contact used in the built-in Terms of Service (`/terms`) and Privacy Policy (`/privacy`) |
| `terms_url`, `privacy_url` | External legal pages. These replace the built-in ones in links. |
| `css_vars` | Overrides of the built-in CSS custom properties, written without `--`. Common keys include `accent`, `accent-2`, `accent-ink`, `bg`, `bg-soft`, `panel`, `panel-2`, `border`, `text`, `muted`, and `radius`. |
| `custom_css` | Extra stylesheet (up to 64 KiB), appended after the built-in theme |
| `hero_eyebrow`, `hero_title`, `hero_text` | Landing page copy |
| `footer_text`, `footer_links` | Footer text and up to ten `{label, url}` links |

```json
{"branding": {"name": "Acme AI", "logo_url": "/theme/logo.svg", "favicon_url": "/theme/favicon.png",
  "css_vars": {"accent": "#ff7a1a", "accent-2": "#ff9f4a", "accent-ink": "#1d0d00"}}}
```

### Theme directory

Start with `--theme-dir ./theme`, or set `RESELLER_THEME_DIR`.

- Images, icons, and fonts in the directory are served at `/theme/<path>`.
- Dot-files, path traversal, symlinks that leave the directory, and unknown file types are refused.
- A `theme.css` file is appended to `/assets/theme.css`.
- An `index.html` file replaces the page template. It can use these placeholders: `{{title}}`, `{{name}}`, `{{description}}`, `{{theme_color}}`, `{{color_scheme}}`, `{{logo_url}}`, `{{favicon_links}}`, `{{footer_text}}`, `{{footer_links}}`, and `{{branding_json}}`.
- Keep these references in a custom template: `<script type="application/json" id="branding">`, `/assets/style.css`, `/assets/theme.css`, and `/assets/app.js`.

All placeholder values are escaped. The Content-Security-Policy allows same-origin scripts, styles, and fonts, and `https:` images.

## Payments

### Plans: subscriptions with a monthly quota, and top-ups

Reseller sells two kinds of plans, the model that App Store Review Guideline 3.1.1 allows for AI usage:

- **Subscriptions** (`kind = "subscription"`). A subscription runs for `months` (1 to 12) and includes `monthly_quota_usd` of usage **every month**. Each month of the paid period grants one quota lot that expires at the end of that month, so unused quota does not carry over. The quota resets with the next month, or with a store renewal. Spending uses the current monthly quota first.
- **Top-ups** (`kind = "topup"`). A top-up is an optional add-on of `credit_usd` of extra usage. Purchased top-up credit **never expires**, carries over to later months, and is spent after the monthly quota. Guideline 3.1.1 says that credits bought with in-app purchase may not expire.

A new database starts with these plans (all editable in **Admin > Plans**):

| Code | Kind | Price | Usage |
|---|---|---|---|
| `monthly` | subscription, 1 month | $20 | $20 per month |
| `halfyear` | subscription, 6 months | $96 | $17 per month |
| `year` | subscription, 12 months | $144 | $12.50 per month |

No top-ups are seeded. Create them in **Admin > Plans** (`kind = "topup"`) when you want to offer extra usage, for example as App Store consumables.

Customers see quotas and trial credit as approximate request counts, not dollar amounts. For example, $20 per month is shown as "~20,000 requests per month" with the default `billing.request_estimate_usd` of `"0.001"`. Set this to the typical billed cost of one request with your models and markup. Counts are rounded to two significant digits and shown as one figure (`~50`), never as a range. Plan prices and the administrator console stay in USD.

Quota is billed usage at the caller's markup, so a fully used quota still earns `markup_pct` over upstream cost.

Where plans come from:

- **Card and crypto.** A subscription bought by card or crypto is prepaid for its whole term and does not renew automatically. The customer buys again when it ends.
- **App Store and Google Play.** Subscriptions bought in the native apps renew through the store. Each renewal starts a new paid period with a fresh monthly quota.
- **Administrators.** Administrators can grant plans, add credit, or issue single-use credit or plan codes. Credit from a code never expires.
- **Trial credit.** Signup trial credit is promotional and expires after `signup.free_credit_expiry_days`.

Monthly quota lots are granted when an account is used or viewed, and every minute for all accounts. Months that pass without activity are skipped, since their quota would already have expired.

Moving from the earlier model (0.11, prepaid credit that expired with its plan): there is no data migration. The single schema starts fresh. `duration_days` and the bonus credit are gone. Prepaid plans map to subscriptions whose quota is the old credit divided by the months, and expiring credit packs map to non-expiring top-ups.

### Orders

Every checkout creates a server-side order. The order snapshots the plan's kind, price, credit or monthly quota, and months. A verified provider event (or an administrator) grants exactly that snapshot, once. Browser redirects never grant credit. Store purchases are recorded as paid orders when the store confirms them (see below).

`GET /v1/plans` lists the enabled `payment_methods` (`stripe`, `crypto`). `POST /v1/account/checkout` with `{"plan_code","method"}` starts a checkout. When `method` is omitted, the first enabled method is used.

### Cards (Stripe Checkout)

1. Set `stripe.secret_key`, `stripe.webhook_secret`, and your external `public_url` in Settings. On first start, you can use the bootstrap environment instead.
2. Register `https://api.example.com/webhooks/stripe` in Stripe for `checkout.session.completed` and `checkout.session.async_payment_succeeded`.
3. Make sure Stripe prices are at least `$0.50`, in whole cents, and in USD.

The webhook verifies the signature over the raw body, with a five-minute tolerance. It requires a paid session whose currency, amount, order, and session match. Event IDs and paid-order state prevent duplicate grants, including several event types for one session. Delayed payment methods are credited only once they are confirmed paid.

To test locally, use Stripe test mode and the Stripe CLI:

```sh
stripe listen --forward-to localhost:56787/webhooks/stripe
# Save the whsec_ secret that the command prints in Settings, then make a test checkout in the UI.
```

### Crypto (Payroof)

[Payroof](https://payroof.top) watches EVM chains for payments to deterministic per-order addresses. It derives them from your watch-only key, so it can never move funds.

Reseller accepts:

- **USD stablecoins** (`stablecoins`, default USDC and USDT), valued 1:1.
- **Native coins** (ETH, POL, BNB, and others), valued at the current USD rate.

Both are accepted on every chain that your Payroof project watches, or on the `chains` allowlist.

1. Create a Payroof project. Then set `payroof.project` and `payroof.token` in Settings. The token must be a `prf_...` token with the `references` and `webhooks` permissions.
2. Set `public_url` to your public **https** origin. Open **Admin > Payments > Crypto payments setup** and click **Register webhook**. This subscribes `{public_url}/webhooks/payroof` to `seen` and `confirmed` events, and stores the signing secret. You can also paste the `whsec_...` secret of an existing webhook into `payroof.webhook_secret`.
3. Crypto checkout is offered once the project, token, and webhook secret are set.

At checkout, Reseller allocates the order's Payroof reference. It stores a quote for each accepted asset.

- Stablecoins are quoted at the plan price, in the token's own decimals.
- Native coins are quoted at the current rate, rounded up to 8 decimals.
- Rates come from a CoinGecko-compatible `rate_url`. `rate_ids` maps symbols to coin IDs, and rates are cached for two minutes.
- Fixed `native_rates_usd` override the feed. Set `rate_source = "static"` to disable the feed.
- Quotes hold for `quote_ttl_minutes`.

With `payroof.deposits = true` (off by default), each account also has a standing deposit reference `acct-<account_id>`. `GET /v1/account` returns `top_up` with the addresses, assets, and `eta_minutes` (`payroof.deposit_eta_minutes`, default 5), plus `networks` (per chain: address, chain id, confirmations, accepted `tokens` with contract and decimals, explorer link templates) and `recent` deposits with a `state` of `confirming`, `credited`, or `not_credited`. A confirmed stablecoin transfer on an allowed chain credits the account once and never expires. Other assets are noted as `unsupported_asset` and not credited. An empty balance returns `402 insufficient_quota` with these instructions.

The payment dialog shows each asset, network, amount, and deposit address. It updates itself until the order is paid.

Webhooks are verified before parsing. The check is HMAC-SHA256 with the decoded `whsec_` key, within 300 seconds. Each event is deduplicated by `Payroof-Id`.

- `seen` records the transfer.
- `confirmed` values the transfer against its quote (`amount / quoted amount * price`).
- Several transfers or assets add up. The order is paid once the confirmed value covers the price minus `underpayment_tolerance_pct`.
- Transfers in unquoted assets, transfers to a different address, and native coins first seen after the quote expired are recorded with a note. They are not counted. Stablecoins are honoured even after expiry.
- Review these in **Admin > Payments > Details**. There, **Mark as paid** completes an order by hand.
- Events for other projects or unknown references are acknowledged and ignored.

Sweep received funds from your own wallet.

### App Store (StoreKit 2)

Server-side setup:

1. In App Store Connect, create the in-app purchases:
   - an auto-renewable subscription for each subscription plan, in one subscription group
   - a consumable for each top-up.
2. Put the product ids into the plans' `apple_product_id` field (**Admin > Plans**).
3. Set `apple.bundle_id`, `apple.app_apple_id`, and `apple.environment`. Use `sandbox` for TestFlight and Xcode testing, and `production` for the live app.
4. To look up transactions by id and to answer refund requests, create an In-App Purchase key in App Store Connect (Users and Access > Integrations). Set `apple.issuer_id`, `apple.key_id`, and `apple.private_key` (the `.p8` text).
5. Set the App Store Server Notifications **V2** URL to `https://api.example.com/webhooks/apple`. Use one server per environment, because a server ignores notifications for the other one.

How a purchase reaches the account:

1. The app gets the account's `app_account_token` from `GET /v1/account/store`. It passes this UUID as `appAccountToken` in `Product.purchase(options:)`.
2. After a successful purchase, the app sends `transaction.jwsRepresentation` to `POST /v1/account/store/apple` as `signed_transaction`, or `transaction.id` as `transaction_id` (this needs the API key). Then it calls `transaction.finish()`.
3. The server verifies the purchase before crediting it:
   - **JWS chain.** The `x5c` chain must lead to the pinned Apple Root CA G3. The intermediate must be a CA carrying Apple's WWDR marker OID, the leaf must carry the App Store receipt-signing OID, and every link's signature and validity (at `signedDate`) are checked. The JWS is ES256, signed by the leaf key.
   - **Transaction fields.** `bundleId` and `environment` must match the configuration. `appAccountToken` must be this account's token, so a transaction cannot be replayed onto another account. The product must map to a plan of the matching type, and the transaction must not be revoked.
4. The server records the charge as a paid order and grants it: a subscription period from `purchaseDate` to `expiresDate`, or top-up credit times `quantity`.

Notifications keep the account current:

- `SUBSCRIBED`, `DID_RENEW`, and `ONE_TIME_CHARGE` record the new charge. The account is found by `appAccountToken`, or by the subscription's `originalTransactionId`.
- `DID_CHANGE_RENEWAL_STATUS` updates `auto_renew`.
- `REFUND` applies the refund protection described below.
- `REVOKE` (Family Sharing) ends that subscription's quota without blocking the account.
- `REFUND_REVERSED` is logged. The account stays suspended until an operator reviews it.
- `CONSUMPTION_REQUEST` is answered as described in [Refund protection](#refund-protection).

Each notification is verified like a transaction and deduplicated by `notificationUUID`. A failure returns an error, so Apple retries the notification.

### Google Play

Server-side setup:

1. In Play Console, create a subscription for each subscription plan and a one-time (consumable) product for each top-up. Put the product ids into the plans' `google_product_id`.
2. Create a Google Cloud service account, grant it access to the app in Play Console (financial data and order management), and set `google.service_account_json` to its JSON key. Set `google.package_name`.
3. Create a Pub/Sub topic and select it in Play Console > Monetization setup > Real-time developer notifications.
4. Add a **push** subscription to `https://api.example.com/webhooks/google` with authentication enabled. Set `google.pubsub_service_account` to the push service account's email. If you set a custom audience, also set it as `google.pubsub_audience`.

How a purchase reaches the account:

1. The app gets `obfuscated_account_id` from `GET /v1/account/store` and passes it to `BillingFlowParams.Builder.setObfuscatedAccountId`.
2. After the purchase, the app sends `{"product_id","purchase_token"}` to `POST /v1/account/store/google`.
3. The server checks the purchase with the Google Play Developer API:
   - **Subscriptions** use `purchases.subscriptionsv2.get`. The state must be active or in grace, and the order id is `latestSuccessfulOrderId`.
   - **One-time products** use `purchases.products.get`. The purchase state must be purchased.
   - In both cases, the obfuscated account id must match the account.
4. After the charge is committed, the server acknowledges the subscription or consumes the one-time product. Google refunds purchases that are not acknowledged within three days, so failed calls are retried in the background, and later notifications acknowledge again if needed.

Notifications (Real-time Developer Notifications through Pub/Sub push):

- **Authentication.** Every push must carry a Google-signed OIDC token: RS256 against Google's published keys, issuer `accounts.google.com`, the configured audience, and the configured service account email (verified). Other requests are refused with 401. Messages are deduplicated by Pub/Sub `messageId`.
- **Subscription charges.** `PURCHASED`, `RENEWED`, `RECOVERED`, and `RESTARTED` record the charge. `CANCELED` turns `auto_renew` off, and `REVOKED` is a refund.
- **One-time purchases.** `ONE_TIME_PRODUCT_PURCHASED` records and consumes the purchase.
- **Refunds and chargebacks.** `voidedPurchaseNotification` (refunds and chargebacks, full or quantity-based) applies the refund protection. `pendingRefundReviewNotification` (chargebacks) is answered as described below.

### Store data and duplicates

Store orders are `payments` rows with `provider` set to `apple` or `google` and `platform` set to `ios` or `android`. They also record `product_id` and `environment`, plus `original_transaction_id` (App Store) or `purchase_token` (Google Play). Stripe and Payroof orders keep their own path.

Several unique indexes stop the same charge from crediting twice, whether the app, a retry, or a store notification delivers it, and in any order:

- `(provider, external_id)`, where `external_id` is the App Store `transactionId` or the Google Play `orderId` (each renewal has its own)
- the Google Play one-time `purchase_token`
- the subscription's `original_transaction_id` and `purchase_token`.

### Refund protection

Apple and Google decide refunds, not the customer, and the usage behind a refunded purchase has already been paid upstream. Even so, some losses cannot be prevented, for example chargebacks on stolen cards. These guards reduce that exposure:

- **Daily cap for new accounts.** For the first `rate_limits.new_account_hours` (48) hours, an account may spend at most `rate_limits.new_account_daily_spend_pct` (20%) of its paid allowance per rolling 24 hours. The allowance is the monthly quota of its active subscriptions plus its purchased top-ups. Requests over the cap get `429 new_account_daily_limit`.

  20% per day means at most about 40% of a month's allowance can be used before the window closes. That is still far more than a typical day (a uniform month uses about 3.3% per day), so honest new customers rarely hit it. It also keeps a buy-burn-refund evening well below a full allowance. Accounts with only trial credit are already bounded by that credit.
- **Refund or voided purchase.** The following happens at once:
  - every credit lot is zeroed
  - subscriptions end
  - the account is suspended (`suspended_reason: "refund"`) and its keys are blocked
  - the signup address may not open new accounts for `billing.refund_ip_block_days` (30, 0 turns this off)
  - for the App Store, the subscription's `originalTransactionId` is blocked, so a later purchase by the same Apple ID on a new account suspends that account too.

  Google Play exposes no stable customer id across accounts, so the address block is the only cross-account guard there. Reactivate a wrongly blocked account in **Admin > Accounts**.
- **Apple Consumption Information.** The server can answer Apple's `CONSUMPTION_REQUEST` with **Send Consumption Information v2** (`PUT /inApps/v2/transactions/consumption/{transactionId}`). Apple expects an answer within 12 hours, and the server sends it at once. To enable it, set `apple.consumption_reporting = true`, which needs the API key. The body has these fields:
  - `customerConsented: true`
  - `consumptionPercentage`, the share of the purchase used in thousandths of a percent (omitted for auto-renewable subscriptions, as Apple requires)
  - `deliveryStatus: "DELIVERED"`
  - `sampleContentProvided`, true when signup grants trial credit
  - `refundPreference`: `GRANT_FULL` when nothing was used, `DECLINE` from `billing.refund_decline_pct` (50%) used, and `GRANT_PRORATED` in between.

  Apple accepts consumption data only with the customer's consent. The server answers only for accounts with `consumption_consent` set.

  Consent is given through a clickwrap at signup: the form states that clicking **Create key** accepts the Terms of Service and the Privacy Policy, and it sends `"accept_terms": true`. The server then records `consumption_consent` and `consent_at`. The Privacy Policy has its own section on this sharing, and customers withdraw with `PATCH /v1/account` `{"consumption_consent": false}`. Without consent, nothing is sent.

  In the EU, consent bundled into the Terms is not a freely given GDPR consent. The built-in policy therefore names fraud prevention (legitimate interest) as the legal basis and the acceptance as the customer's agreement. If you need a stricter reading, add a separate opt-in.
- **Google Play chargebacks.** With `google.refund_review = true`, a `pendingRefundReviewNotification` (chargebacks only) is answered through the Review Refund API (`orders.reviewrefund`) within Google's 24-hour window. The answer carries the same consumption share (`consumptionPercentageMilliunits`), `sampleContentProvided`, and a `refundPreference` of `APPROVE`, `DECLINE`, or `NEUTRAL`. Google has no consumption API for ordinary refund requests.

Both stores also issue partial refunds: Apple prorated refunds (`revocationPercentage`) and Google quantity-based refunds. These are treated like full refunds and block the account.

Known limitations:

- Apple certificate revocation (OCSP) is not checked. The chain, the marker OIDs, the pinned root, and the signatures are.
- Store prices are recorded at the plan's USD price, not the local price the customer paid.

## Operations

- **Health:** `GET /healthz` checks liveness. `GET /readyz` runs `SELECT 1`. `reseller healthcheck` probes `/readyz` for containers.
- **Backups:** `reseller backup ./backup.db` takes an online snapshot that includes committed WAL data. To restore, stop the server and replace the database file. Backups contain upstream and payment secrets, so store them as credentials.
- **Retention:** request logs are pruned every minute to `log_retention_days` (30) and `max_log_entries` (5000). Usage aggregates are kept for up to 367 days. Spent proof-of-work records are purged after their challenges expire.
- **Single instance:** do not share one database between running server instances. Rate-limiter state is in memory and per process.
- **Schema:** the schema is created automatically on first start.
- **Shutdown:** SIGTERM or Ctrl-C drains connections and billing tasks. Allow up to the upstream timeout.
- **Crashes:** an abrupt process or machine failure can lose usage that an upstream has not yet reported. Interrupted requests remain identifiable in the database.

## Development

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

### Source layout

| Module | Responsibility |
|---|---|
| `main.rs` | CLI: `serve`, `init`, `doctor`, `token`, `reset-admin`, `backup`, `healthcheck`, `mcp`, and `config export` or `import` |
| `web.rs` | Router, security headers, CORS, static assets, rate-limit gating, and top-level dispatch |
| `theme.rs` | Branded page template, generated theme CSS, and theme-directory files |
| `proxy/` | Upstream transport: JSON policies and fallbacks (`mod.rs`), streamed uploads (`multipart.rs`), usage inspection (`inspect.rs`), and realtime relay (`websocket.rs`) |
| `meter.rs` | Admission with per-request holds (status, balance, RPM, usage windows, new-account cap), pricing (`Usage::cost`), and exactly-once settlement |
| `api/` | Customer workspace (`customer.rs`) and administrator console (`admin.rs`) JSON APIs |
| `mcp/` | `POST /mcp` server (`mod.rs`, `tools.rs`, `generate.rs`) and the stdio bridge (`bridge.rs`) |
| `security/` | In-memory rate limiter (`limiter.rs`), ALTCHA proof of work (`pow.rs`), and per-IP and account guards |
| `identity.rs`, `keys.rs` | Credential hashing and authentication, key limits, creation guards, and rotation |
| `billing.rs`, `money.rs` | Credit ledger, subscriptions with monthly quotas, top-ups, idempotent grants, refund blocks, redeem codes, and micro-USD arithmetic |
| `payments/` | Checkout and store orders (`mod.rs`), Stripe Checkout (`stripe.rs`), Payroof quotes, rates, and webhooks (`payroof.rs`), and App Store and Google Play purchases and notifications (`store/`) |
| `stats.rs` | Overview, usage series, and log retention |
| `config/` | Settings (`mod.rs`), `branding.rs`, `billing.rs`, `payments.rs`, `stores.rs`, `signup.rs`, `keys.rs`, `rate_limits.rs`, and `mcp.rs` |
| `db.rs`, `error.rs` | SQLite access and the API error type |

The embedded UI (`assets/`) is plain HTML, CSS, and JavaScript, served under a strict Content-Security-Policy. There is no frontend build step. Integration tests in `tests/` drive the real router against mock upstreams.

## License

MIT. See [LICENSE](LICENSE).

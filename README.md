<p align="center">
  <img src="assets/logo.png" width="96" height="96" alt="reseller logo">
</p>

# reseller

**Your API. Your pricing. One binary.**

reseller is a self-hosted, OpenAI-compatible API reseller written in Rust. Point it at your upstream model providers, then sell access as subscriptions with a monthly usage quota plus top-ups. It proxies language, speech, transcription, and image requests, issues customer keys, meters every request, and serves a white-label web workspace from the same binary.

## Features

- **Proxy:** streaming JSON, SSE, audio, multipart uploads, and WebSocket (realtime) transport.
- **Providers and routing:** any number of OpenAI-compatible upstream providers, a public model catalog with several routes per model, per-route and per-provider budgets, balanced or ordered selection, automatic failover, circuit breakers, and cooldowns learned from upstream rate-limit hints. See [docs/routing.md](docs/routing.md).
- **Free tier without a key:** models marked `tier = "free"` work with no credential, are never billed, and are rate limited per client (IPv4 address or IPv6 /64). Over-limit requests fail at once with `429` or `503` and exact retry hints. Paid models need a key and a balance.
- **MCP for AI agents:** a remote `POST /mcp` endpoint with OAuth 2.1 sign-in (add the URL in Claude, ChatGPT, Cursor, or VS Code) and a local `reseller mcp` stdio bridge. Agents can generate content and manage their account. See [docs/mcp.md](docs/mcp.md).
- **Signup and abuse control:** proof-of-work signup (ALTCHA-compatible), a signup cooldown and global hourly cap, rotation limits, free-tier limits that extra accounts cannot multiply, and an auth-failure lockout.
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
- [Free tier](#free-tier)
- [Routing guide](docs/routing.md) (architecture, free providers, capacity planning)
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

3. Edit `reseller.toml`. Set the `api_key` (and `base_url`) of the `[[providers]]` entry, and the upstream `model` of the route under `[[models]]`.
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

Pin a release tag instead of `latest` to keep deployments reproducible. Compose passes only the bootstrap variables. For several providers, free models, or routing limits, mount a settings file and set `RESELLER_CONFIG` (see the comments in `compose.yaml`), or edit the settings in the admin console after the first start.

### Try a free model without a key

Free models need no account. Add a provider and a free model (see [Free tier](#free-tier) for the settings), then call it with no credential:

```sh
curl http://localhost:56787/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"free","messages":[{"role":"user","content":"Hello"}]}'
```

An SDK that insists on a key can send any placeholder, such as `free`. Paid models answer `401 api_key_required` until the caller sends a key.

### Run the starter configuration with your keys

`reseller.example.toml` lists ready providers (OpenRouter with two keys, Gemini, NVIDIA, Groq, and Cloudflare Workers AI), a balanced free model `free`, and a paid model `gpt-6-luna`. It reads every key from the environment, so put the keys you have in an `.env` file next to `compose.yaml`:

```sh
# .env (only the keys you have, the other providers stay disabled)
OPENROUTER_API_KEY=sk-or-v1-...
OPENROUTER2_API_KEY=sk-or-v1-...
GEMINI_API_KEY=...
NVIDIA_API_KEY=nvapi-...
GROQ_API_KEY=gsk_...
CLOUDFLARE_ACCOUNT_ID=...
CLOUDFLARE_AUTH_TOKEN=...
# Optional route model overrides: OPENROUTER_MODEL, OPENROUTER2_MODEL,
# GEMINI_MODEL, NVIDIA_MODEL, GROQ_MODEL, CLOUDFLARE_MODEL
```

`compose.yaml` mounts the starter file and passes these variables, and Docker Compose reads `.env` from the project directory:

```sh
docker compose up -d
```

With plain `docker run`, pass the same variables with `-e` (or `--env-file`, which takes values literally, so write them without quotes) and mount the file at the path in `RESELLER_CONFIG`.

The file seeds the database on the first start only (see [Environment references](#environment-references)).

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
- The token also works as an operator credential: `curl https://HOST/v1/models -H "Authorization: Bearer $ADMIN_TOKEN"` and the same on `/v1/*` and `/mcp` generation tools. It sees the full model catalog (free and paid), is never billed, has no free-tier limits, and its requests are logged without an account. On `/v1/keys`, `/v1/account`, and the account tools it is answered with `403 admin_token_not_customer` (use `/admin/api`), which does not count as an authentication failure.

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
| Top level | `public_url` (`http://localhost:56787`), `admin_token`, `key_pepper` (both at least 24 characters, generated if unset), `markup_pct` (`30`), `client_ip` (`"peer"`), `cors_origin` (empty), `max_json_body_bytes` (50 MiB), `max_stream_body_bytes` (512 MiB), `upstream_timeout_seconds` (300), `log_retention_days` (30), `max_log_entries` (5000) |
| `[branding]` | White-label name, copy, logos, colours, and CSS (see [Branding and themes](#branding-and-themes)) |
| `[[providers]]` | Upstream connections, one per API key: `id`, `base_url`, `api_key`, `enabled` (`true`), `timeout_seconds` (0 uses `upstream_timeout_seconds`, max 3600), `headers` (max 20, credential and framing headers are refused), and `limits` (see [Providers, models, and routing](#providers-models-and-routing)) |
| `[[models]]` | Public model catalog and the routes that serve each model (see [Providers, models, and routing](#providers-models-and-routing)) |
| `[routing]` | Failover and cooldowns: `max_attempts` (3, 1 to 10), `failure_threshold` (3, 1 to 1000), `cooldown_seconds` (30, at least 1), `max_cooldown_seconds` (600, at most 86400) |
| `[free]` | Keyless access: `enabled` (`true`), `ipv6_prefix` (64, 32 to 128), `rpm` (10), `rpd` (300), `tpd` (300000), `max_concurrent` (2), `max_concurrent_total` (64), `max_output_tokens` (4096), `max_body_bytes` (1 MiB). 0 turns a limit off. |
| `[prices.<model>]` | Upstream prices in USD (see [Pricing and metering](#pricing-and-metering)) |
| `[signup]` | Anonymous signup: `enabled` (`true`), `cooldown_seconds` (30, 0 turns it off), `global_hourly_limit` (100, 0 turns it off), `pow_max_number` (100000, 0 disables proof of work, max 10000000), `pow_ttl_seconds` (300, 10 to 86400), `adaptive_pow` (`true`), `pow_max_number_ceiling` (1000000, from `pow_max_number` to 10000000), and `adaptive_pow_reset_minutes` (30, 1 to 1440). New accounts start with no credit. |
| `[keys]` | `max_per_account` (10 active keys, must be above 0), `max_new_per_day` (20 created or rotated per account in a rolling 24 hours, 0 turns it off), `rotate_cooldown_seconds` (300, 0 turns it off, max 86400), and `[keys.default_limits]` (`rpm`, `requests`, `tokens`, `spend`, `max_children`, `allowed_models`, copied onto each new account's first key, child keys inherit their parent's limits) |
| `[rate_limits]` | `management_per_minute` (120), `auth_failures_per_hour` (20), `max_concurrent_requests` (8), `checkouts_per_hour` (10), `redeem_failures_per_hour` (10), `challenges_per_minute` (10), `new_account_hours` (48) and `new_account_daily_spend_pct` (`"20"`): the new-account spending cap (see [Refund protection](#refund-protection)). 0 disables each one. |
| `[reasoning]` | `free_effort` (`"low"`, the cap for requests to free models) and `[reasoning.models."<upstream model>"]` with `efforts` and `default` (see [Reasoning effort](#reasoning-effort)) |
| `[billing]` | `request_estimate_usd` (`"0.001"`, the typical billed cost of one request, used to show customers approximate request counts), `default_max_output_tokens` (4096, held for LLM requests without an output limit), `min_hold_usd` (`"0.01"`, the smallest hold per request), `refund_ip_block_days` (30, 0 turns the address block off), `refund_decline_pct` (50, share of a purchase that must be used before refund answers ask the store to decline) |
| `[mcp]` | `enabled` (`true`), `oauth` (`true`, OAuth sign-in for `/mcp`, see [MCP](#mcp)), and `max_media_bytes` (16 MiB, 1 byte to 256 MiB), the largest audio or image returned inline |
| `[stripe]` | `secret_key`, `webhook_secret` |
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

### Providers, models, and routing

Three lists describe what you sell and where it comes from. A **provider** is one upstream connection (one API key). A **model** is a public catalog entry that clients name in requests. Each model has one or more **routes**, and each route points at a provider and the model name that provider knows.

```toml
[[providers]]
id = "openai"                    # 1-32 characters of a-z, 0-9, _ and -
base_url = "https://api.openai.com/v1"
api_key = "sk-..."
# enabled = true
# timeout_seconds = 0            # 0 uses upstream_timeout_seconds, max 3600
# headers = { "HTTP-Referer" = "https://api.example.com" }
# limits = { rpm = 0, rpd = 0, tpm = 0, tpd = 0, concurrent = 0 }   # budget of this key, 0 = unlimited

[[models]]
id = "gpt-6-luna"                # the name clients send
capability = "chat"              # required: chat, speech, transcription, or image
tier = "paid"                    # "paid" (default) or "free"
default = true                   # used when a request names no model
strategy = "ordered"             # "balanced" (default) or "ordered"
aliases = ["luna"]
name = "GPT-6 Luna"
context_length = 128000
show_pricing = true              # paid models only

[[models.routes]]
provider = "openai"
model = "gpt-6-luna"             # the upstream name
# weight = 1                     # 1-100, the share in "balanced"
# limits = { rpm = 0, rpd = 0, tpm = 0, tpd = 0, concurrent = 0 }   # budget of this provider and model pair

[routing]
# max_attempts = 3
# failure_threshold = 3
# cooldown_seconds = 30
# max_cooldown_seconds = 600
```

#### Environment references

String values in a settings file may reference environment variables, which keeps credentials out of the file. `"${NAME}"` is the value of `NAME` (an unset variable is a startup error), `"${NAME:-fallback}"` is the value or `fallback` when `NAME` is unset or empty, and `"$${"` is a literal `${`. A provider whose `api_key` references variables that are unset or empty is loaded **disabled**, and a model without an enabled provider is hidden from `/v1/models` and refused like a disabled one. One file can therefore list every provider you might use. References are expanded when the file is read, on the first start and by `reseller config import`. `reseller config export` writes a literal `${` as `$${`, so an export imports unchanged.

| Setting | Meaning |
|---|---|
| `providers.limits`, `models.routes.limits` | Budgets: requests per minute (`rpm`) and per day (`rpd`), tokens per minute (`tpm`) and per day (`tpd`), and requests in flight (`concurrent`). 0 is unlimited, the maximum is 1000000000. A route is skipped while its own or its provider's budget is empty. Token budgets are charged after a request finishes. |
| `models.capability` | `chat` also covers embeddings, moderations, responses, completions, and realtime. `speech`, `transcription`, and `image` map to `/v1/audio/speech`, `/v1/audio/transcriptions` and translations, and `/v1/images/*`. |
| `models.default` | At most one servable default (enabled, with an enabled provider) per capability and tier. A request without a model gets the paid default of its capability when the caller has a key (else the free default), and the free default when the caller is anonymous. |
| `models.strategy` | `balanced` spreads traffic by `weight`, remaining budget, success rate, and load. `ordered` takes the first available route in list order, so the later routes are fallbacks. |
| `models.voice` | Speech only. The voice sent when the client names none. |
| `models.routes` | 1 to 20 routes. Each provider and model pair may appear once per model. |
| `models.routes.fallback` | A last resort: tried, in listed order, only after every other route of the model is unavailable or has failed the request, with its own attempts beyond `routing.max_attempts`. At least one route must not be a fallback. Use it for an inexpensive paid model behind free routes, and cap its cost with route `limits` (free requests stay unbilled for the caller). |
| `routing.*` | `max_attempts` routes are tried per request, a route's circuit opens after `failure_threshold` failures in a row, and cooldowns start at `cooldown_seconds`, double on each reopen with 20% jitter, and stop at `max_cooldown_seconds`. |

There is no catalog-free mode: a model that is not in `[[models]]` is `404 model_not_found`. Upstream names, provider ids, and base URLs are never shown to customers. Prices stay keyed by **upstream** model name in `[prices.<model>]`, and a paid request holds the highest price among the model's routes. [docs/routing.md](docs/routing.md) explains selection, failover, and capacity planning.

### Environment bootstrap

These variables seed the settings on **first start only**. They override the TOML file at that point. `ADMIN_TOKEN` and `KEY_PEPPER` must be at least 24 characters. Leave them unset to have secure values generated.

| Group | Variables |
|---|---|
| LLM provider | `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `OPENAI_MODEL`, `OPENAI_MODEL_FALLBACKS`: creates the provider `llm` and a paid default chat model named `OPENAI_MODEL` |
| TTS provider | `TTS_BASE_URL`, `TTS_API_KEY`, `TTS_MODEL`, `TTS_MODEL_FALLBACKS`, `TTS_VOICE`: provider `tts` and a paid speech model with that voice |
| STT provider | `STT_BASE_URL`, `STT_API_KEY`, `STT_MODEL`, `STT_MODEL_FALLBACKS`: provider `stt` and a paid transcription model |
| Image provider | `IMG_BASE_URL`, `IMG_API_KEY`, `IMG_MODEL`, `IMG_MODEL_FALLBACKS`: provider `image` and a paid image model |
| Core | `ADMIN_TOKEN`, `KEY_PEPPER`, `MARKUP_PCT` |
| Public site | `PUBLIC_URL`, `BRAND_NAME` |
| Stripe | `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` |
| Payroof | `PAYROOF_API_URL`, `PAYROOF_PROJECT`, `PAYROOF_TOKEN`, `PAYROOF_WEBHOOK_SECRET` |
| App Store | `APPLE_BUNDLE_ID`, `APPLE_ISSUER_ID`, `APPLE_KEY_ID`, `APPLE_PRIVATE_KEY` |
| Google Play | `GOOGLE_PACKAGE_NAME`, `GOOGLE_SERVICE_ACCOUNT_JSON`, `GOOGLE_PUBSUB_SERVICE_ACCOUNT` |

Fallback lists are comma-separated. Each fallback becomes one more route of the same model on the same provider, tried in order (the model's strategy is `ordered`). There are no variables for free models, other providers, limits, or routing, so use the settings file (`RESELLER_CONFIG`) or the admin console for those. Providers from the file and the variables are merged by `id`. The variables take precedence for the ids `llm`, `tts`, `stt`, and `image`.

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
3. Credit is consumed soonest-expiring first: the current month's subscription quota before top-ups, which never expire and come last.
4. Usage, debit, request metadata, and counters commit in one transaction.
5. For `/v1/chat/completions`, `/v1/responses`, and `/v1/completions`, tokens are **estimated** when the upstream reports no usage at all. The estimate uses about four characters per token, based on the request text and the generated text. The request row is flagged `estimated: 1`. MCP generation results report this as `usage_estimated`.
6. For other routes, if a provider returns no usage and no supported audio duration, only the flat `request` fee applies. Use providers that report usage.

Requests to free models are never billed: `billed_usd` is always 0, no balance is needed, and the provider's reported `usage.cost`, if any, is recorded as `upstream_usd`. Every request row records the full breakdown behind its charge: `tokens`, `input_tokens`, `cached_tokens`, `output_tokens`, `characters`, `audio_ms`, `images`, `upstream_usd`, `billed_usd`, and `estimated`. Customers audit each line with everything except `upstream_usd`: customer request and usage views never show the provider cost. Administrators see all fields.

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

### Anonymous access and caller classes

`/v1` serves four kinds of caller. The class is decided from the credential alone:

| Caller | Credential | May use | Limits |
|---|---|---|---|
| Anonymous | None, or any bearer or `x-api-key` value that starts with neither `sk-res-` nor `dash_` and is not the administrator token (a placeholder such as `free` works for SDKs that demand a key) | Free models only. A paid model answers `401 api_key_required`. | Free-tier limits per client address |
| Customer | A valid `sk-res-` key | Free and paid models the key may use | Key limits and balance for paid models. Free models also pass the free-tier limits. |
| Operator | The administrator token | Everything, never billed | No free-tier limits |
| Invalid | An `sk-res-` or `dash_` value that is not a known credential | Nothing: `401 invalid_api_key`, and it counts toward the auth-failure lockout | |

A valid `dash_` token on the proxy is `401 dashboard_token_not_allowed`. With `free.enabled = false`, an anonymous request answers `401 missing_api_key` and free models are used with a key only.

**Who is one client.** The free-tier limits count per client, where a client is:

- an IPv4 address, or an IPv6 address grouped by its `free.ipv6_prefix` (default `/64`, the block one host controls, so rotating addresses inside it buys nothing),
- or, for a key of a **paying or trusted** account, the account itself. That means the account has an order that was paid, an active subscription, a spendable balance, or `can_create_keys`.

A key of any other account is counted under its client address like an anonymous caller, so opening more accounts adds no free allowance. Several people behind one address share one allowance until they pay.

**Admission order.** A free request is checked before the database is touched: free requests in flight overall (`503 capacity_exhausted`), then in flight for the client (`429 concurrency_limit_exceeded`), then requests per minute (`429 rate_limit_exceeded`), requests per day, and tokens per day (`429 free_quota_exceeded`). The rate limits (`rpm`, `rpd`, `tpd`) are GCRA allowances, not strict rolling counts: a client may use the full `rpd` at once, then the allowance refills continuously at `rpd` per 24 hours, so over any 24 hours it can use at most about twice `rpd`, and `Retry-After` gives the exact wait. Nothing waits in a queue.

**`client_ip` matters here.** The client address is the TCP peer unless `client_ip` names the header your proxy sets (see [Behind a proxy](#behind-a-proxy)). Behind a reverse proxy with `"peer"`, every anonymous caller shares the proxy's address and one allowance. With a header your proxy does not set (for example `"cf-connecting-ip"` without Cloudflare), clients choose their own address and evade the limits.

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

- **Throttles.** These are request rate, concurrency, auth failures, checkouts, cooldowns, free-tier request rate and concurrency, and requests in flight holding the balance (`balance_reserved`). The wait is short and exact, and a retry after it succeeds. Free-tier answers also carry `retry-after-ms`.
- **Usage windows.** These are the key `requests`, `tokens`, and `spend` windows, the free daily request and token allowances (`free_quota_exceeded`), and the new-account daily cap. `Retry-After` is the time until enough settled usage leaves the window (or until next month or the account's 48th hour). These responses also send `x-should-retry: false`, so the OpenAI and Anthropic SDKs do not retry them right away. Show the error, or schedule the request after `Retry-After`.

| Limit | Scope | Config | 429 `code` |
|---|---|---|---|
| Management requests (`/v1/signup`, `/v1/keys`, `/v1/account`, `/v1/plans`, `/mcp`, `/oauth/*`, `/admin/api/*`) | Client IP, per minute | `rate_limits.management_per_minute` | `management_rate_limited` |
| Signup challenges | Client IP, per minute | `rate_limits.challenges_per_minute` | `challenge_rate_limited` |
| Invalid credentials | Client IP, per hour | `rate_limits.auth_failures_per_hour` | `too_many_auth_failures` |
| Invalid redeem codes | Account, per hour | `rate_limits.redeem_failures_per_hour` | `redeem_rate_limited` |
| Checkout attempts | Account, per hour | `rate_limits.checkouts_per_hour` | `checkout_rate_limited` |
| In-flight proxy requests | Account | `rate_limits.max_concurrent_requests` | `concurrency_limit_exceeded` |
| Free requests per minute | Client address, or paying account | `free.rpm` | `rate_limit_exceeded` |
| Free requests per day (GCRA) | Client address, or paying account | `free.rpd` | `free_quota_exceeded` |
| Free tokens per day (GCRA) | Client address, or paying account | `free.tpd` | `free_quota_exceeded` |
| Free requests in flight | Client address, or paying account | `free.max_concurrent` | `concurrency_limit_exceeded` |
| Gap between two signups | IP | `signup.cooldown_seconds` | `signup_cooldown` |
| Key creations (signup, child keys, rotations) per hour | Global | `signup.global_hourly_limit` | `creation_rate_limited` |

| Keys created or rotated per rolling 24 hours | Account (customers, admin exempt) | `keys.max_new_per_day` | `key_creation_daily_limit` |
| Minimum key age before rotation | Key (customers, admin exempt) | `keys.rotate_cooldown_seconds` | `rotate_cooldown` |

| Requests per minute | Key | `rpm` | `rate_limit_exceeded` |
| Requests per window | Key | `requests` | `request_limit_exceeded` |
| Tokens per window | Key | `tokens` | `token_limit_exceeded` |
| Spend per window | Key | `spend` | `spend_limit_exceeded` |
| Spend per rolling 24 hours of a new account | Account | `rate_limits.new_account_hours`, `rate_limits.new_account_daily_spend_pct` | `new_account_daily_limit` |
| Balance held by requests in flight | Account | none | `balance_reserved` |
| Free requests in flight overall (a `503`, not a `429`) | Global | `free.max_concurrent_total` | `capacity_exhausted` |

Counts that only drop when something is removed are not rate limits and are not 429s. `account_key_limit` (`keys.max_per_account`) and `child_key_limit` (`max_children`) are `403` without `Retry-After`: revoke a key to create another.

A request whose own hold can never fit a key window (for example a 4096-token hold under a 1000-token limit) is a `400` with the window's code and `param: "max_tokens"`.

### Error status changes in 0.12

| Case | Before | Now | Why |
|---|---|---|---|
| Empty balance, or a hold larger than the balance (`insufficient_quota`) | `429`, `Retry-After: 60` | `402`, no `Retry-After` | Waiting never helps. Only a top-up or a smaller request does, and SDKs do not retry 402. |
| Key `spend`, `tokens`, `requests` windows | `429`, `Retry-After: 60` | `429`, exact `Retry-After`, `x-should-retry: false` | The window frees up hours later, not in 60 s |
| Requests per minute | `429`, `Retry-After: 60` | `429`, seconds until the oldest counted request is a minute old | Exact wait |
| `account_key_limit`, `child_key_limit` | `429`, `Retry-After: 60` | `403`, no `Retry-After` | A count cap, not a rate |
| Upstream `insufficient_quota` (the operator's provider account is out of credit) | Passed through as `429` | `502 upstream_error` | It is not the caller's quota |
| Upstream status rewritten (for example 401 to 502) | Kept the upstream `Retry-After` | Header dropped | The hint belonged to the upstream status |

Trusted accounts (`can_create_keys`) skip the daily key creation budget, but not the account key cap or the global hourly cap. Accounts carry no signup credit, so the signup guards are only the gap between signups (`signup.cooldown_seconds`), the global hourly cap, proof of work, and the refund blocks. A household sharing one connection can open as many accounts as it likes, and each person can hold several keys on their own account (`keys.max_per_account`, `keys.max_new_per_day`).

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

- `client_ip` names the one source of the client address: `"peer"` (the TCP peer, the default), `"x-forwarded-for"` (the **last** `X-Forwarded-For` entry, which your proxy appended, for Caddy, nginx, Traefik, or a load balancer), or `"cf-connecting-ip"` (only behind Cloudflare). No other header is read.
- Choose a header only when direct access to the application is blocked and your proxy sets that header. Earlier `X-Forwarded-For` entries, and a `CF-Connecting-IP` that no Cloudflare edge set, come from the client.
- With the wrong source, clients can spoof their IP and evade every per-IP limit, including the free-tier limits. For example, `"cf-connecting-ip"` behind a proxy other than Cloudflare lets any client claim a new address per request.
- With `"peer"` behind a proxy, all users share one IP. The per-IP limits (free-tier allowance, signup cooldown, and lockout) then apply to everyone at once.

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
| `GET` | `/v1/` | Public | Discovery: links and `routes` per capability (`chat`, `speech`, `transcription`, `image`) |
| `GET` | `/v1/models` | Optional | Model catalog, local and not metered. Anonymous callers see free models only, keys see the free and paid models they may use. Each entry has a `tier` (`free` or `paid`), and `architecture` carries modalities and a `capability` (`chat`, `speech`, `transcription`, `image`) |
| `GET` | `/v1/models/:id` | Optional | One catalog entry, or `404 model_not_found` |
| `GET` | `/v1/account/models` | Any | The `/v1/models` list for the caller's key, also with a dashboard token |
| `GET` | `/v1/account` | Any | Account, balance (`balance_usd`, split into `quota_remaining_usd` and `topup_remaining_usd`), lifetime `billed_usd`, active `subscriptions` with their monthly quota, credit lots, `consumption_consent`, and `top_up` instructions |
| `PATCH` | `/v1/account` | Any | Body `{"consumption_consent": false}` withdraws (or `true` grants again) the consent to share consumption data with app stores for refund requests |
| `GET` | `/v1/account/store` | Any | Native app purchase setup: `app_account_token` (App Store) and `obfuscated_account_id` (Google Play) for this account, store settings, and plans with store product ids |
| `POST` | `/v1/account/store/apple` | Any | Body `{"signed_transaction":"<JWS>"}` or `{"transaction_id":"..."}` after a StoreKit 2 purchase. Returns the account and `purchase: {order_id, credited}` |
| `POST` | `/v1/account/store/google` | Any | Body `{"product_id","purchase_token"}` after a Play Billing purchase. Same response |
| `GET` | `/v1/account/usage?days=30&key=` | Any | Daily and per-model usage (`days` 1 to 365) |
| `GET` | `/v1/account/requests?limit=20&offset=0&key=&model=&kind=&status=` | Any | Request history. Rows carry `key_name` and `key_prefix`, the public `model`, a `tier`, and the number of `attempts`. They never name the provider or the upstream model, and carry no upstream cost. |
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

### White-labeled responses

Successful JSON and `text/event-stream` responses of the chat, completions, responses, embeddings, moderations, transcription (JSON), and image routes name the public model, never the upstream's: `model` (and `response.model` in streamed Responses events) is replaced with the public id, and a top-level `provider` key is dropped. Other fields, SSE framing, `data: [DONE]`, keep-alives, and non-JSON lines pass through untouched. Event streams use a bounded line buffer (a single huge line is passed through unmodified), plain JSON bodies are buffered up to `max_json_body_bytes` (larger ones pass through unmodified), and binary bodies are never touched. Usage metering sees the upstream's original bytes, `bytes_out` counts what was sent, and the operator gets the same rewriting.

### Routing to providers

Routing replaces `/v1` with the provider's base path and keeps the query string. The path decides the capability, and the capability decides which models can answer:

| Client path | Capability |
|---|---|
| `/v1/audio/speech` | `speech` |
| `/v1/audio/transcriptions`, `/v1/audio/translations` | `transcription` |
| `/v1/images`, `/v1/images/*` (generations, edits, variations) | `image` |
| All other proxied `/v1/*` routes (chat completions, completions, embeddings, moderations, responses, realtime) | `chat` |

`/v1/models`, `/v1/keys`, `/v1/account`, `/v1/plans`, and `/v1/signup` are handled locally and are never proxied. Other paths return `404 unknown_url`, and a wrong method returns `405 method_not_allowed` with an `Allow` header.

For each request the server:

1. Identifies the caller (see [Anonymous access and caller classes](#anonymous-access-and-caller-classes)).
2. Resolves the model by id or alias, or takes the default of the capability and tier. An unknown, disabled, or disallowed model is `404 model_not_found`, a model of another capability is `400 unsupported_model`, no model and no default is `400 model_required`, and a paid model for an anonymous caller is `401 api_key_required`.
3. Admits it: a free model passes the free-tier limits, a paid model passes the key's limits and holds its worst-case cost.
4. Picks a route (see [Failover](#failover)), rewrites `model` to the route's upstream name, and forwards.

### Models

`GET /v1/models` lists the catalog in the OpenAI format. It does not need a key and is not metered. Anonymous callers see free models only. With a key, the list holds the free models and the paid models the key may use, and it returns `402 insufficient_quota` when the balance is empty and no free model exists. The list never shows upstream names, provider ids, URLs, keys, or raw prices. Disabled entries and models the key may not use are hidden, and aliases are not listed. Pricing, when `show_pricing` is on for a paid model, uses the caller's effective markup.

```sh
curl http://localhost:56787/v1/models
```

### Choosing a model

- A client model is matched against ids and aliases without regard to case and rewritten to the route's upstream name.
- Omitting `model` uses the default entry of the endpoint's capability: the paid default for a keyed caller (else the free one), the free default for an anonymous caller. Without a default the answer is `400 model_required`.
- The operator credential may name any model.
- Speech requests that name no `voice` get the model's `voice`.

For uploads, the provider is chosen before the file streams. The server buffers the start of the multipart body (bounded by `max_json_body_bytes`) until the `model` part completes, resolves the route, and then streams the rest without buffering. A request with no `model` part by the end of that prefix uses the capability default. A later `model` part that differs is `400 model_must_precede_file`, so **send `model` before the file**. WebSocket connections resolve the `?model=` query parameter before admission and forwarding. Uploads and WebSocket sessions get a single attempt.

### Image requests

```sh
curl http://localhost:56787/v1/images \
  -H "Authorization: Bearer $RESELLER_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"meta/muse-image","prompt":"Make the sky orange",
       "input_references":[{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,..."}}]}'
```

The `model` of a JSON request is resolved against the catalog and rewritten for the route. Chat requests stream usage reporting. Multipart uploads stream with bounded size, and only the `model` form field is rewritten.

### Reasoning effort

Clients set the reasoning level as `reasoning_effort` (Chat Completions) or `reasoning.effort` (Responses API, and the OpenRouter chat form). Supported levels differ by provider and model, and an upstream may reject an unknown level with a 400 or ignore it. So the `[reasoning]` section declares them per upstream model and enforces them before forwarding:

```toml
[reasoning]
free_effort = "low"      # cap for requests to free models; "" disables

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
| Another route (failover) | The original level, or the nearest lower level that route's model supports (never a 400) |

**Free models** are capped at `free_effort` (default `"low"`) for every caller: a higher level is lowered to the highest supported level at or below the cap, and a configured reasoning model gets that level even when the request names none. For an unconfigured model, only an explicitly higher level is lowered. Requests to paid models and operator requests are not capped. The settings are keyed by the **upstream** model name of a route.

Levels are ordered `none < minimal < low < medium < high < xhigh < max`. Configure each model's real levels: OpenRouter, for example, accepts all seven for DeepSeek models and answers others with a 400. A model-name suffix such as `model#low` is not used. Suffixes would split one model into several names for prices, allowlists, fallbacks, and `/v1/models`, while clients already have a standard field for the level.

### Failover

Each model lists its routes, and the router keeps in-memory health and budget state for every provider and route. See [docs/routing.md](docs/routing.md) for the full model.

- A route is a candidate when its provider is enabled and not cooling down, the route is not cooling down, its circuit is closed (or half open with no probe in flight), every configured budget has room, and concurrency is below the caps.
- `ordered` takes the first candidate. `balanced` takes the candidate with the best score, `weight * headroom * success rate / (1 + in flight)`.
- A failed attempt moves to the next route while attempts remain (`routing.max_attempts`). Only JSON bodies that the server holds in memory are replayed, so multipart uploads and WebSocket sessions get a single attempt.
- `400`, `401`, `402`, `403`, `404`, `408`, `409`, `429`, and `5xx` responses and connection or timeout errors move on to the next route. A `429` cools the route down until the upstream's own hint (`Retry-After`, `retry-after-ms`, `x-ratelimit-*`, or Google's `retryDelay`), else `routing.cooldown_seconds`. A `401`, `402`, or `403`, or an upstream `insufficient_quota`, cools the whole provider down for `routing.max_cooldown_seconds`. A `404` (model not found at that provider) cools the route down for `routing.cooldown_seconds` without counting toward the circuit breaker. A `400` that a later route then serves cools the refusing route down the same way, since some providers answer an unknown model with `400`. A `5xx`, a timeout, or a connection error counts toward the circuit breaker, and a `400` does not.
- When no route is available the caller gets `503 capacity_exhausted` with `Retry-After` and `retry-after-ms` set to the time until the soonest route is. Requests never wait for capacity.
- Redirects are not followed. Hop-by-hop headers and local credentials are removed before forwarding.
- Each request records its `tier` and the number of `attempts`. The provider and the upstream model that answered are visible to administrators only.

### Response headers

| Header | Meaning |
|---|---|
| `X-Request-Id` | Request identifier |
| `X-Key-Prefix` | Prefix of the key used |
| `X-Account-Balance-USD` | Balance before the request (pre-request snapshot) |

Free responses add `x-ratelimit-limit-requests`, `x-ratelimit-remaining-requests`, and `x-ratelimit-reset-requests` (the per-minute window, or the daily one when there is no per-minute limit), and the IETF draft fields `RateLimit-Policy` and `RateLimit` that list every window, for example `RateLimit-Policy: "free";q=10;w=60, "free-day";q=300;w=86400` and `RateLimit: "free";a=9;w=12, "free-day";a=299;w=40000`. Errors from the free tier carry the same headers plus `Retry-After` and `retry-after-ms`.

### Proxy errors

Failures use the OpenAI shape `{ "error": { "message": "...", "type": "...", "param": null, "code": "..." } }`. The `type` is `invalid_request_error` (400, 401, 404, 405, 409, 413, 415), `permission_error` (403), `insufficient_quota` (402, code `insufficient_quota`), `rate_limit_error` (429), or `server_error` (5xx). `param` is `"model"` for model errors. Upstream errors are rebuilt in this shape: a JSON error keeps only its `message`, `type`, `param`, and `code` (other fields such as upstream user ids or provider metadata are dropped), a non-JSON body gets a generic message with code `upstream_error`, and an upstream 401, 402, or 403, or an upstream `insufficient_quota` (problems with the operator's upstream account, not the caller's key), becomes `502 upstream_error` without the upstream `Retry-After`. Other upstream 429s pass through with their `Retry-After`.

| Status | Codes |
|---|---|
| 400 | `invalid_request`, `unsupported_model` (the model cannot serve this endpoint), `model_required` (no model and no default), `model_must_precede_file` (a multipart `model` part came after the file or changed), `unsupported_reasoning_effort` |
| 401 | `missing_api_key` (free access is off), `invalid_api_key`, `api_key_required` (a paid model without a key), `key_revoked`, `key_blocked`, `key_expired`, `dashboard_token_not_allowed` |
| 403 | `account_suspended` |
| 404 | `model_not_found`, `unknown_url` |
| 405 | `method_not_allowed` |
| 413 | `request_too_large`: the body is too big (free requests have the smaller `free.max_body_bytes`) |
| 402 | `insufficient_quota`: the balance is empty or smaller than the request's hold (lower `max_tokens` or top up) |
| 429 | Limits and throttles, always with `Retry-After` (see [Rate limits](#rate-limits)). Free-tier codes are `rate_limit_exceeded`, `concurrency_limit_exceeded`, and `free_quota_exceeded` (daily, with `x-should-retry: false`). |
| 502, 504 | Upstream failure |
| 503 | `capacity_exhausted`: no route or free slot has room, with `Retry-After` and `retry-after-ms`. `proxy_configuration_error`: the route's provider has no API key. |

## Free tier

The free tier lets anyone call `tier = "free"` models with no key. It is meant for trying the service and for light use, and it costs the operator nothing extra when the free models are backed by free upstream plans (see the provider list in [docs/routing.md](docs/routing.md)).

```toml
[[providers]]
id = "gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
api_key = "${GEMINI_API_KEY:-}"   # read from the environment, disabled when unset

[[providers]]
id = "openrouter"
base_url = "https://openrouter.ai/api/v1"
api_key = "${OPENROUTER_API_KEY:-}"

[[models]]
id = "free"
tier = "free"
capability = "chat"
default = true

[[models.routes]]
provider = "gemini"
model = "gemini-3.5-flash-lite"
weight = 2

[[models.routes]]
provider = "openrouter"
model = "baidu/ernie-5.5-flash-preview:free"
limits = { rpm = 20 }                 # the upstream's free-model budget

[free]
enabled = true
rpm = 10
rpd = 300
```

How it behaves:

- **No key needed.** A free model answers without a credential, or with any placeholder that is not an `sk-res-` or `dash_` value. Paid models answer `401 api_key_required`. Free models are never billed and never need a balance.
- **Limits per client.** `free.rpm`, `free.rpd`, `free.tpd` (tokens, charged after the request), `free.max_concurrent` per client and `free.max_concurrent_total` overall. Clients are grouped by IPv4 address, IPv6 `/64` prefix (`free.ipv6_prefix`), or the account for paying and trusted accounts. See [Anonymous access and caller classes](#anonymous-access-and-caller-classes).
- **Bounded requests.** `max_tokens`, `max_completion_tokens`, and `max_output_tokens` are lowered to `free.max_output_tokens`, and the cap is injected when a chat, responses, or completions request has none. Reasoning models spend output tokens on reasoning, so a very small cap can yield empty answers. JSON bodies over `free.max_body_bytes` answer `413`. Reasoning effort is capped by `reasoning.free_effort`.
- **Fail fast.** Over-limit requests are never held open or queued. They answer at once with `429` (rate, concurrency, or the daily allowance) or `503 capacity_exhausted`, each with `Retry-After` and `retry-after-ms`. The OpenAI SDKs honor both and retry on their own. `free_quota_exceeded` carries `x-should-retry: false` because waiting seconds will not help.
- **Headers.** Every free response, including errors, reports the remaining allowance in `x-ratelimit-*-requests` and the IETF `RateLimit-Policy` and `RateLimit` fields (see [Response headers](#response-headers)).
- **The model list.** `GET /v1/models` without a key lists the free models, and the landing and docs pages show a keyless quick start when at least one is enabled.
- **Turning it off.** `free.enabled = false` makes anonymous requests answer `401 missing_api_key`. Keys still use free models.

Free upstream tiers often forbid reselling, may train on prompts, or may change without notice. Read each provider's terms before you route anonymous traffic to it, and tell your users that free-model prompts leave your server.

## MCP

Agents can use the service through the Model Context Protocol. There are two ways to connect.

- **Remote endpoint:** a stateless Streamable HTTP endpoint at `POST /mcp`. It supports protocol `2026-07-28` (current) and the legacy `2025-11-25` handshake. An `initialize` that asks for an older revision is answered with `2025-11-25`. Requests that name an older revision in `MCP-Protocol-Version` are refused. Every request needs a credential: without one the answer is `401` with a `WWW-Authenticate` header that points OAuth clients to `/.well-known/oauth-protected-resource/mcp`.
- **Local bridge:** `reseller mcp` runs next to the agent. It keeps credentials and the signup proof of work on the agent's machine.

**Connect with OAuth.** Add `https://HOST/mcp` as a remote MCP server in Claude, ChatGPT, Cursor, or VS Code. The client registers itself (RFC 7591), and a browser page on this server asks you to sign in with an API key or dashboard token (or create an account) and to allow the connection. It uses PKCE `S256`, resource indicators, one-hour access tokens (`mcp_at_...`), and rotating refresh tokens that expire after 90 days idle. Tokens are bound to your key, stop working when the key does, and are valid on `/mcp` only. Customers revoke apps in the dashboard. `[mcp].oauth = false` turns this off. **Static bearer alternative:** send an API key or dashboard token as `Authorization: Bearer ...`. The administrator token works for generation tools only.

Generation tools go through the same admission, limits, metering, and model routing as the HTTP API. Set `[mcp].enabled = false` to turn the endpoint off. It then answers `404 mcp_disabled`.

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
| `PATCH` | `/accounts/:id` | Update status, contact, note, `markup_pct`, or `can_create_keys` (trusted: skips the daily key budget and gets its own free-tier allowance) |
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
| `GET` | `/routing` | Live router state: `providers` and `routes` with in-flight counts, circuit state, cooldowns, remaining budgets, latency, success rate, and 24-hour request log totals, plus the free tier's in-flight count and tracked clients (see [Routing endpoint](#routing-endpoint)) |
| `GET` | `/requests` | Request log. Each row has `model` (the upstream model that answered), `public_model`, `provider`, `tier`, `attempts`, `key_name`, `key_prefix`, and `account_contact`. Filtered by `q`, `model`, `kind`, `status`, `path`, `ip`, `method`, `key`, `account`, `hour`, `limit`, and `offset` |
| `GET` | `/requests/groups?field=model` | Group requests by `model`, `provider`, `tier`, `ip`, `status`, `kind`, `path`, `key`, or `hour` |
| `GET` | `/settings` | Redacted configuration |
| `PATCH` | `/settings` | Validated partial configuration update |

### Routing endpoint

`GET /admin/api/routing` returns the state the router keeps in memory, joined with the last 24 hours of the request log. The Routing tab of the admin console renders it.

```json
{
  "providers": [{"id": "gemini", "enabled": true, "in_flight": 1, "cooldown_until": null,
    "limits": {"rpm": {"limit": 20, "remaining": 14, "reset_seconds": 31}},
    "usage_24h": {"requests": 120, "failures": 2, "rate_limited": 5, "tokens": 90000,
                  "upstream_cost_usd": "0.000000", "avg_latency_ms": 840, "free_requests": 118}}],
  "routes": [{"provider": "gemini", "model": "gemini-3.5-flash-lite", "models": ["free"], "in_flight": 1,
    "circuit": "closed", "failures": 0, "cooldown_until": null, "latency_ms": 790, "success_rate": 0.987,
    "limits": {}, "usage_24h": {}}],
  "free": {"in_flight": 3, "subjects": 41}
}
```

- `circuit` is `closed`, `open`, or `half_open`. `cooldown_until` is Unix seconds or `null`.
- `limits` lists each configured window with its `limit`, `remaining`, and `reset_seconds`, and a `concurrent` entry with its `limit`.
- `latency_ms` and `success_rate` are moving averages. `usage_24h.avg_latency_ms` is the mean over the last 24 hours of the log.
- `free.subjects` counts clients with a live free-tier window.
- The state is per process and starts fresh after a restart. Daily budgets are re-seeded from the log at boot (see [docs/routing.md](docs/routing.md)).

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

Customers see quotas as approximate request counts, not dollar amounts. For example, $20 per month is shown as "~20,000 requests per month" with the default `billing.request_estimate_usd` of `"0.001"`. Set this to the typical billed cost of one request with your models and markup. Counts are rounded to two significant digits and shown as one figure (`~50`), never as a range. Plan prices and the administrator console stay in USD.

Quota is billed usage at the caller's markup, so a fully used quota still earns `markup_pct` over upstream cost.

Where plans come from:

- **Card and crypto.** A subscription bought by card or crypto is prepaid for its whole term and does not renew automatically. The customer buys again when it ends.
- **App Store and Google Play.** Subscriptions bought in the native apps renew through the store. Each renewal starts a new paid period with a fresh monthly quota.
- **Administrators.** Administrators can grant plans, add credit, or issue single-use credit or plan codes. Credit from a code never expires.
- **Free models.** Free models are never billed and do not touch quota or balance (see [Free tier](#free-tier)). New accounts start with no credit.

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

  20% per day means at most about 40% of a month's allowance can be used before the window closes. That is still far more than a typical day (a uniform month uses about 3.3% per day), so honest new customers rarely hit it. It also keeps a buy-burn-refund evening well below a full allowance. Free models are never billed, so they are outside this cap.
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
  - `sampleContentProvided`, true when the operator offers a free tier (`free.enabled` and at least one enabled free model)
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
- **Single instance:** do not share one database between running server instances. Rate-limiter, free-tier, and router state (circuits, cooldowns, in-flight counts) is in memory and per process. After a restart, daily provider and route budgets are re-seeded from the last 24 hours of the request log, while circuits and per-client free allowances start fresh.
- **Routing health:** watch the Routing tab (or `GET /admin/api/routing`) for open circuits, cooling providers, and routes whose remaining budget is near zero. A route whose `rpd` keeps reaching zero needs a second provider key or a lower per-client `free.rpd`. See [docs/routing.md](docs/routing.md) for capacity planning.
- **Behind a proxy:** set `client_ip` to the header your proxy sets (`"x-forwarded-for"` for Caddy or nginx, `"cf-connecting-ip"` for Cloudflare) and block direct access to the app. Otherwise every anonymous caller shares one free-tier allowance, or clients can forge many.
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
| `proxy/` | Upstream transport: model resolution (`models.rs`), JSON requests with failover (`mod.rs`), streamed uploads (`multipart.rs`), usage inspection (`inspect.rs`), reasoning effort (`reasoning.rs`), and realtime relay (`websocket.rs`) |
| `router/` | Route selection and health (`mod.rs`), free-tier admission (`free.rs`), and upstream rate-limit hint parsing (`hints.rs`) |
| `meter.rs` | Admission with per-request holds (status, balance, RPM, usage windows, new-account cap), pricing (`Usage::cost`), and exactly-once settlement |
| `api/` | Customer workspace (`customer.rs`) and administrator console (`admin.rs`) JSON APIs |
| `mcp/` | `POST /mcp` server (`mod.rs`, `tools.rs`, `generate.rs`) and the stdio bridge (`bridge.rs`) |
| `security/` | In-memory rate limiter (`limiter.rs`), ALTCHA proof of work (`pow.rs`), and per-IP and account guards |
| `identity.rs`, `keys.rs` | Credential hashing and authentication, key limits, creation guards, and rotation |
| `billing.rs`, `money.rs` | Credit ledger, subscriptions with monthly quotas, top-ups, idempotent grants, refund blocks, redeem codes, and micro-USD arithmetic |
| `payments/` | Checkout and store orders (`mod.rs`), Stripe Checkout (`stripe.rs`), Payroof quotes, rates, and webhooks (`payroof.rs`), and App Store and Google Play purchases and notifications (`store/`) |
| `stats.rs` | Overview, usage series, and log retention |
| `config/` | Settings (`mod.rs`), `providers.rs`, `models.rs`, `routing.rs`, `free.rs`, `branding.rs`, `billing.rs`, `payments.rs`, `stores.rs`, `signup.rs`, `keys.rs`, `rate_limits.rs`, `reasoning.rs`, and `mcp.rs` |
| `db.rs`, `error.rs` | SQLite access and the API error type |

The embedded UI (`assets/`) is plain HTML, CSS, and JavaScript, served under a strict Content-Security-Policy. There is no frontend build step. Integration tests in `tests/` drive the real router against mock upstreams.

## License

MIT. See [LICENSE](LICENSE).

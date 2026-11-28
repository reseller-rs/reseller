# Routing, the free tier, and capacity planning

This guide explains how reseller picks an upstream for each request, how the keyless free tier is admitted and limited, and how to size both. For the settings themselves, see the annotated [`reseller.example.toml`](../reseller.example.toml) and the [README](../README.md#providers-models-and-routing).

## Contents

- [Concepts](#concepts)
- [Caller classes](#caller-classes)
- [Free-tier admission](#free-tier-admission)
- [Why over-limit requests fail fast](#why-over-limit-requests-fail-fast)
- [Route selection](#route-selection)
- [Circuit breaker and cooldowns](#circuit-breaker-and-cooldowns)
- [Response rewriting](#response-rewriting)
- [Upstream hints](#upstream-hints)
- [Failover rules](#failover-rules)
- [The multipart rule](#the-multipart-rule)
- [Warm start](#warm-start)
- [Statistics and the routing endpoint](#statistics-and-the-routing-endpoint)
- [Free upstream providers](#free-upstream-providers)
- [Capacity planning](#capacity-planning)
- [Operator checklist](#operator-checklist)

## Concepts

| Term | Meaning |
|---|---|
| Provider | One OpenAI-compatible upstream connection: a base URL and one API key. It has an optional account-wide budget (`limits`). |
| Model | A public catalog entry that clients name. It has a `tier` (`free` or `paid`), a `capability`, a `strategy`, and 1 to 20 routes. |
| Route | A provider and the model name that provider uses. It has a `weight` and an optional budget of its own. |
| Budget | `rpm`, `rpd`, `tpm`, `tpd`, and `concurrent`. 0 means unlimited. The rate budgets are GCRA allowances, not rolling counts (see [Free-tier admission](#free-tier-admission)). |

One route can serve several public models, and a provider serves every model that lists it. Budgets are therefore tracked per provider (shared by all its routes) and per route (shared by all models that use it). Router state is keyed by provider id and upstream model name, so editing settings keeps the state of every route that survives the edit.

## Caller classes

The class comes from the credential alone.

| Caller | Credential | Free models | Paid models |
|---|---|---|---|
| Anonymous | None, or any value that starts with neither `sk-res-` nor `dash_` and is not the administrator token (for example the placeholder `free`) | Yes, with free-tier limits | `401 api_key_required` |
| Customer | A valid `sk-res-` key | Yes. Limits are per account if it pays or is trusted, else per client address. | Yes, with key limits and a balance |
| Operator | The administrator token | Yes, no limits | Yes, never billed |
| Invalid | An `sk-res-` or `dash_` value that is not valid | `401 invalid_api_key` (counts toward the auth-failure lockout) | Same |

With `free.enabled = false`, an anonymous request answers `401 missing_api_key`. Keys still use free models.

**Who is one client.** The free-tier counters are keyed by a subject:

- an IPv4 address (`ip:203.0.113.7`),
- an IPv6 prefix of `free.ipv6_prefix` bits, `/64` by default (`ip:2001:db8:1:2::/64`),
- the account, for a key whose account has a paid order, an active subscription, a spendable balance, or `can_create_keys` (`acct:<id>`).

A key of any other account uses its client address. Opening more accounts therefore adds no free allowance, and a household or an office behind one address shares one allowance until someone pays. The client address is the TCP peer unless `client_ip` names the header your own proxy sets (`"x-forwarded-for"` behind Caddy or nginx, `"cf-connecting-ip"` behind Cloudflare). Never name a header your proxy does not set: clients would choose their own address and get a fresh allowance per request.

## Free-tier admission

A request to a free model is admitted in memory, before any database write and before an upstream is chosen. The checks run in this order and the first failure answers:

| Order | Check | Setting | Answer |
|---|---|---|---|
| 1 | Free requests in flight overall | `free.max_concurrent_total` | `503 capacity_exhausted`, `Retry-After` and `retry-after-ms` of 1 to 2 seconds |
| 2 | Requests in flight for this client | `free.max_concurrent` | `429 concurrency_limit_exceeded` |
| 3 | Requests per minute | `free.rpm` | `429 rate_limit_exceeded` |
| 4 | Requests per day (GCRA) | `free.rpd` | `429 free_quota_exceeded`, `x-should-retry: false` |
| 5 | Tokens per day (GCRA) | `free.tpd` | `429 free_quota_exceeded`, `x-should-retry: false` |

Notes:

- The per-minute and per-day checks use a generic cell rate algorithm (GCRA), so the retry hint is the exact time until the next request fits, not a fixed guess. The per-minute limit allows a burst of `free.rpm` requests.
- `rpd` and `tpd` are not strict rolling 24 hour counts. A client may use the full `free.rpd` at once, and the allowance then refills continuously at `free.rpd` per 24 hours. Over any 24 hours a client can therefore use at most about twice `free.rpd` (the burst plus the refill), and `Retry-After` gives the exact wait for the next request. The same holds for `tpd`, and for the `rpm`, `rpd`, `tpm`, and `tpd` budgets of providers and routes.
- Tokens are charged when a request finishes, from the usage the upstream reports. A client that spent its day of tokens waits for 1% of the allowance to refill, not for a single token.
- The in-flight counters are released when the exchange ends, even if the client disconnects.
- The request body is limited to `free.max_body_bytes` (`413` over it), and output length is clamped to `free.max_output_tokens`: `max_tokens`, `max_completion_tokens`, and `max_output_tokens` are lowered to the cap, and the cap is added to chat, responses, and completions requests that have none. Reasoning models spend output tokens on reasoning, so a very small cap can leave no room for the answer and yield an empty one. `reasoning.free_effort` caps the reasoning level.
- Free requests are never billed and need no balance. If the upstream reports `usage.cost`, it is recorded as the upstream cost.

Every free response (including these errors) carries the allowance state:

```
x-ratelimit-limit-requests: 10
x-ratelimit-remaining-requests: 9
x-ratelimit-reset-requests: 6s
ratelimit-policy: "free";q=10;w=60, "free-day";q=300;w=86400
ratelimit: "free";a=9;w=12, "free-day";a=299;w=40000
```

The `x-ratelimit-*-requests` trio follows the OpenAI convention and describes the per-minute window (or the daily one when there is no per-minute limit). `RateLimit-Policy` and `RateLimit` follow `draft-ietf-httpapi-ratelimit-headers`: `q` is the quota, `w` in the policy is the window in seconds, `a` is the units still available, and `w` in `RateLimit` is the seconds until the window resets. The draft is not an RFC yet and may change.

After admission, the request chooses a route. If no route has room, it answers `503 capacity_exhausted` with the time until the soonest route is available.

## Why over-limit requests fail fast

reseller never queues or holds an over-limit request. It answers at once with `429` or `503` and a `Retry-After` the client can trust. The reasons:

- **Clients already know how to wait.** The OpenAI SDKs retry `429`, `503`, `408`, `409`, and `5xx`. They honor `retry-after-ms` first and `Retry-After` (seconds or an HTTP date) second, as long as the value is at most 120 seconds. Without a usable header they back off from 0.5 seconds up to 8 seconds with jitter. A longer value is not retried. An accurate hint under 120 seconds therefore makes a standard SDK throttle itself. A `x-should-retry: false` header, which the daily-allowance error sends, tells the SDK not to retry at all.
- **Held connections are expensive.** Every waiting request keeps a socket, a task, and often a buffered body in memory for the whole wait. A burst of anonymous callers would turn the wait queue into a memory and file-descriptor exhaustion path, which is the opposite of what a limit is for. Failing fast keeps the cost of an over-limit request close to zero.
- **Proxies and edges time out first.** Many deployments sit behind a CDN or load balancer with its own limit, for example 100 seconds at Cloudflare. A request held longer than that is cut by the edge, and the client sees an opaque `524` or `504` with no retry hint.
- **Client timeouts are unknown.** A browser fetch, a mobile app, a script, and an SDK each have a different timeout, and many have none. The server cannot tell how long a client is willing to wait, but it can always tell it how long to wait itself.
- **Fairness is simpler.** With no queue there is nothing to order, so one heavy client cannot occupy the line, and the per-client concurrency and rate limits are the whole fairness story.

A short wait can make sense when capacity is expected back within a second or two, such as a token refill. reseller does not hold requests even then. It returns a one-second hint instead, which an SDK turns into a retry.

For clients that are not SDKs, retry with exponential backoff and full jitter, honor `retry-after-ms` or `Retry-After` when present, and never loop on `free_quota_exceeded`.

## Route selection

For each attempt, the router builds the list of candidates for the resolved model, leaving out routes already tried in this request. A route is a candidate when all of these hold:

- its provider is enabled and not cooling down,
- the route itself is not cooling down,
- its circuit is closed, or half open with no probe in flight,
- every configured budget (the route's and the provider's `rpm`, `rpd`, `tpm`, `tpd`) has room,
- the in-flight counts are below the `concurrent` caps.

The strategy then picks one:

- **`ordered`** takes the first candidate in list order. Use it for a primary route with fallbacks, which is also what the `OPENAI_MODEL_FALLBACKS` bootstrap creates.
- **`balanced`** takes the candidate with the highest score:

  ```
  score = weight * headroom * success_rate / (1 + in_flight)
  ```

  - `weight` is the route's `weight` (1 to 100).
  - `headroom` is the smallest remaining fraction over the route's and provider's configured or learned windows, with a floor of 0.05 so a nearly full route still gets a trickle.
  - `success_rate` is a moving average of recent outcomes (each new outcome counts for 10%).
  - `in_flight` is the number of requests the route is serving now.
  - A tie goes to the route picked least recently.

**Fallback routes** (`fallback = true`) are left out while any other route is a candidate. When none is, or when the request has spent its `routing.max_attempts` on the other routes, the fallback routes are tried in listed order, each once, with their own attempts. A typical use is an inexpensive paid model behind the free routes of a free model: callers keep getting answers while every free upstream is down or rate limited, and the operator pays only for those requests. Free requests stay unbilled for the caller, so cap the fallback's cost with route `limits` (for example `tpd`), and watch its `usage_24h.upstream_cost_usd` in the Routing tab.

Picking a route consumes the request budgets and takes an in-flight permit that is held until the response has been relayed. Token budgets (`tpm`, `tpd`) are charged after the request finishes. If no route is a candidate, the caller gets `503 capacity_exhausted` with `Retry-After` set to the soonest availability, capped at `routing.max_cooldown_seconds`. The router never waits.

Latency is tracked (a moving average of the time to the response head) and shown in the admin console, but it does not enter the score. Scoring on health, headroom, and load keeps the behavior predictable.

## Circuit breaker and cooldowns

Each route has a circuit breaker.

| State | Meaning |
|---|---|
| `closed` | Normal. Failures are counted. |
| `open` | Out of rotation until the cooldown ends. |
| `half_open` | The cooldown ended. One probe request is let through. A success closes the circuit and resets the failure count. A failure reopens it with a doubled cooldown. |

What counts, per attempt:

| Upstream answer | Effect |
|---|---|
| `2xx` | Success. Closes a half-open circuit and resets the failure count. |
| `5xx`, timeout, connection error | A breaker failure. After `routing.failure_threshold` in a row the circuit opens. |
| `429` | The route cools down until the upstream's hint, else `routing.cooldown_seconds`. It is not a breaker failure. |
| `401`, `402`, `403`, or an `insufficient_quota` body | A problem with the operator's account. The whole provider cools down for `routing.max_cooldown_seconds`. |
| `404` | The upstream does not know the model. The route cools down for `routing.cooldown_seconds` and the next route is tried. It is not a breaker failure, so a permanently broken route of an `ordered` model is not tried first on every request. |
| `400` | Says nothing about health on its own. When a later route then serves the same request, the refusal was about the route (OpenRouter, for example, answers an unknown model with `400`), so that route cools down for `routing.cooldown_seconds` like a `404`. |
| `408`, `409` | The next route is tried, but the route's health is not affected. |

Cooldown lengths start at `routing.cooldown_seconds`, double on each reopen, get 20% random jitter in either direction so retries do not synchronize, and never exceed `routing.max_cooldown_seconds`. Cooldowns from upstream hints are capped the same way.

## Response rewriting

Customers never see upstream names. A successful JSON or `text/event-stream` response of the chat, completions, responses, embeddings, moderations, transcription (JSON), and image routes is rewritten on the way out:

- `model` becomes the public model id the client resolved. For the Responses API this includes `response.model` in streamed events.
- A top-level `provider` key (OpenRouter adds one) is dropped.
- Everything else is untouched: other fields, SSE event boundaries, `data: [DONE]`, comments and keep-alives, and lines that are not JSON.

Event streams are rewritten line by line with a bounded line buffer, so a single huge line is passed through unmodified. A plain JSON body is buffered up to `max_json_body_bytes` and, if it is larger, passed through unmodified. Binary bodies (audio, image bytes), compressed bodies, and error bodies are not rewritten (errors are rebuilt separately). Usage metering reads the upstream's original bytes, and `bytes_out` counts the bytes actually sent. The operator gets the same rewriting.

## Upstream hints

Providers often tell you exactly how long to wait, and reseller uses it instead of guessing. On every upstream response it reads, leniently (an unparsable value is ignored):

- `retry-after-ms` (milliseconds), then `retry-after` (seconds, possibly fractional, or an HTTP date),
- `x-ratelimit-limit-requests`, `x-ratelimit-remaining-requests`, `x-ratelimit-limit-tokens`, `x-ratelimit-remaining-tokens`,
- `x-ratelimit-reset-requests` and `x-ratelimit-reset-tokens`, as seconds or Go-style durations such as `1s`, `250ms`, `6m0s`, `2m59.56s`,
- in error bodies, Google's `error.details[].retryDelay` (`"30s"`) and "try again in 6.5s" sentences.

How the hints are used:

- A `429` cools the route down until the hint. A window that reports `remaining = 0` is treated as a hint of its reset time (a minute when it gives none).
- The reported limits and remaining counts are learned as windows and enter the `headroom` of the score even when you configured no limit. A window without a reset time is believed for one minute.
- No hint is ever believed for more than 24 hours, so a broken upstream cannot park a route for longer.

Hints from the upstream are never forwarded to the client. The client gets its own `Retry-After`.

## Failover rules

- A request tries up to `routing.max_attempts` routes that are not fallbacks, then each fallback route once. Each attempt uses a route that has not been tried in this request.
- Only requests whose body is held in memory (JSON) can be replayed. Multipart uploads and WebSocket sessions get a single attempt.
- Failover happens only before response headers reach the client. A stream that has started is never switched.
- The reasoning level is mapped again for the model of each attempt, never producing a `400` on a later route.
- Prices are keyed by upstream model name. A paid request holds the highest price among its model's routes, then settles at the price of the route that answered.
- The cost of failed attempts is not billed to the customer.

## The multipart rule

Speech-to-text and image edits upload a file, and the provider has to be chosen before the file streams. reseller therefore reads the start of the multipart body into memory (up to `max_json_body_bytes`) until the `model` part is complete, resolves the route, sends what it has read, and streams the rest without buffering.

- Send `model` before the file. A client that sends the file first and the model last may exceed the bound.
- No `model` part before the bound, or by the end of the body, selects the capability's default model for the caller.
- A later `model` part that names a different model than the one already chosen is `400 model_must_precede_file`.
- The upload gets one attempt. WebSocket sessions (realtime) take the model from the `?model=` query parameter and also get one attempt.

## Warm start

Daily budgets would otherwise reset to full on every restart, which can push a free provider past its daily limit right after a deploy. At boot, the router reads the last 24 hours of the per-route usage table and records the requests and tokens already used into the `rpd` and `tpd` windows of each route and provider that is still configured. Per-minute windows, circuits, cooldowns, in-flight counts, and the per-client free-tier counters start fresh, since they recover within minutes or are not worth persisting.

Because the log is kept in hourly buckets, the restored count is approximate. It errs toward counting earlier usage as recent.

## Statistics and the routing endpoint

Every settled request, including anonymous and free ones, adds to an hourly per-route table: requests, failures, rate-limited answers, tokens, upstream cost, and summed latency, by provider, upstream model, and tier. The request log also records, per request, the public model, the upstream model that answered, the provider, the tier, and the number of attempts. Customers see the public model, the tier, and the attempts, but never the provider or the upstream model.

`GET /admin/api/routing` joins that table (the last 24 hours) with the live in-memory state, and the admin console's Routing tab renders it:

- per provider: enabled, in flight, `cooldown_until`, remaining budget per window (`limit`, `remaining`, `reset_seconds`), and `usage_24h`,
- per route: provider, upstream model, the public models that use it, in flight, `circuit`, consecutive `failures`, `cooldown_until`, `latency_ms`, `success_rate`, remaining budgets, and `usage_24h`,
- for the free tier: requests in flight and the number of clients with a live window.

`usage_24h` has `requests`, `failures`, `rate_limited`, `tokens`, `upstream_cost_usd`, `avg_latency_ms`, and `free_requests`. Request logs can be grouped by `provider` or `tier` with `GET /admin/api/requests/groups?field=provider`.

## Free upstream providers

### Live check of the starter configuration (2026-11-28)

The providers in `reseller.example.toml` were called with real keys, first directly (chat, streaming with usage, model list) and then through reseller in Docker with the starter file and `docker compose`. Results:

| Provider id | Base URL | Route model | Direct chat / stream | Through reseller | Rate-limit headers seen |
|---|---|---|---|---|---|
| `openrouter`, `openrouter-2` | `https://openrouter.ai/api/v1` | `baidu/ernie-5.5-flash-preview:free`, `nvidia/nemotron-3-ultra-550b-a55b:free`, paid `openai/gpt-6-luna` | 200 / 200, usage with `cost: 0` on `:free` | 200, balanced across both keys, paid `gpt-6-luna` billed from the reported cost | None on successful answers |
| `gemini` | `https://generativelanguage.googleapis.com/v1beta/openai` | `gemini-3.5-flash-lite` | 200 / 200 | 200, two upstream 429s during a burst of about 15 requests per minute were absorbed by cooldown and failover | `retryDelay` in 429 bodies |
| `nvidia` | `https://integrate.api.nvidia.com/v1` | `minimaxai/minimax-m3.1-flash` | 200 / 200 | 200 | None |
| `cloudflare` | `https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1` | `@cf/google/gemma-4-26b-a4b-it` | 200 / 200 (`GET /models` is 405) | 200, slowest route (about 2.5 s) | None |
| `groq` | `https://api.groq.com/openai/v1` | `qwen/qwen3.8-27b` | 403 `Access denied. Please check your network settings.` for every request, also with an invalid key | Provider cooled down for `max_cooldown_seconds` after the first 403, traffic moved to the other routes | n/a |

Groq refuses some networks before it checks the key, so the 403 says nothing about the key. Run the check from your server's network. Several free models are reasoning models: they return a `reasoning` field and spend output tokens on it, so keep `free.max_output_tokens` well above the answer length you expect.

### Comparison

Free tiers change often. The table below records what was found on **2026-11-28**. **V** means checked against an official page or a live endpoint. **U** means unverified, taken from third-party lists, or the page could not be read. Check the provider's current terms and limits before you rely on any row.

| Provider | Base URL | Free tier | Documented limits | Card | Data and training | Notes |
|---|---|---|---|---|---|---|
| Google Gemini API (AI Studio key) | `https://generativelanguage.googleapis.com/v1beta/openai` (V) | Permanent (V) | Per project and per model: requests per minute, tokens per minute, and requests per day (reset at midnight Pacific). The numbers are on Google's rate-limit page. | Not for the free tier (inferred) | **Free-tier content is used to improve Google's products (V)** | Flash-family models only on the free tier (U). 429 `RESOURCE_EXHAUSTED`. No rate-limit headers documented. |
| OpenRouter `:free` models | `https://openrouter.ai/api/v1` (V) | Permanent per model (U) | About 20 requests per minute and 50 per day without credits, 1000 per day after a $10 top-up (U) | No (U) | Not verified | A negative credit balance gives `402` even on free models (V). Documents `X-RateLimit-*` headers on free-model errors, with `X-RateLimit-Reset` in milliseconds since epoch. Ten `:free` models were listed on the check date (V). |
| NVIDIA NIM (build.nvidia.com) | `https://integrate.api.nvidia.com/v1` (V) | Prototyping access for Developer Program members (V) | About 40 per minute (U) | Not stated (U) | Prototyping-only terms (V) | Production use needs an NVIDIA AI Enterprise license (V). |
| Cloudflare Workers AI | `https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1` (V) | Permanent daily allowance (V) | 10,000 Neurons per day (V). Neurons are not tokens, so the router cannot count them | Not for the free allocation (U) | Not checked | Rely on the upstream's `429` answers. |
| Groq | `https://api.groq.com/openai/v1` (U) | Unverified | Sources disagree: 30 per minute and 1,000 per day for gpt-oss-120b, or per-model daily token caps | Unverified | Unverified | The documentation was not reachable when checked. Read your own account's limits. |
| Cerebras Inference | `https://api.cerebras.ai/v1` (U) | **Trial only (V)** | $5 credit that expires 30 days after the grant (V) | **Yes (V)** | Not checked | Not a permanent free tier. Do not use for anonymous routing. |
| Together AI | `https://api.together.xyz/v1` (V) | Treat as paid (U) | The rate-limit page gives no free numbers | Unverified | Not checked | Documents `429` and `503` under high demand. |
| Hugging Face Inference Providers | `https://router.huggingface.co/v1` (V) | No free monthly credits (V) | n/a | n/a | n/a | Free users must buy credits or upgrade. |
| GitHub Models | n/a | Retired on 2026-07-30 (V) | n/a | n/a | n/a | Do not use. |
| Mistral La Plateforme | `https://api.mistral.ai/v1` | No API access in free mode (V) | n/a | Billing required (V) | n/a | Do not use for a free tier. |
| Chutes | `https://llm.chutes.ai/v1` | Ended (U) | n/a | n/a | n/a | No model with a zero input price was listed (V). |

Some providers are not suitable for anonymous routing at all: some log or train on prompts (Gemini's free tier, per the table, and others described in third-party lists), some require a consumer login, a browser cookie, or an IDE client, and some prohibit proxying or resale.

**Terms of service.** Whether a provider's terms allow you to serve third parties from a free key is **your responsibility to check**. Several providers' terms are reported to prohibit reselling or sublicensing API access, prototyping-only plans may not allow public services, and some terms change. The research behind this table could not settle OpenRouter's terms on resale, and found that Gemini's terms restrict only grounded results. Get legal review before you offer anonymous access, and prefer providers whose terms explicitly allow it. Tell your users that free-model prompts are sent to third parties and may be kept or used for training.

Operational notes for free providers:

- Set each route's `limits` to the documented budget (see the commented presets in `reseller.example.toml`). The router will then stop sending traffic before the provider refuses it and will not burn the budget on retries.
- Prefer providers that document hard stops (they refuse instead of billing). Never attach a card to a free key that could be billed past the free allowance, unless you accept the risk.
- Use a separate provider entry (a separate key) for paid traffic. A free route and a paid route that share one provider also share its budget, and free traffic can use up what paid customers need.
- Pool several keys of one provider by listing each as its own `[[providers]]` entry and adding a route per key. Check first that the terms allow more than one account.

## Capacity planning

Two kinds of numbers have to line up: what each client may use (`[free]`), and what the upstreams will give you (provider and route `limits`).

**Per-client limits** are set by `free.*`. If `N` clients are active in a day and each uses its full allowance:

```
requests per day needed = N * free.rpd
tokens per day needed   = N * free.tpd
requests per minute at peak <= (clients active in that minute) * free.rpm
requests in flight at peak   <= min(free.max_concurrent_total, clients * free.max_concurrent)
```

**Provider budgets** are the sums over the routes of the free model, limited further by each provider's `limits`:

```
pool requests per day  = sum over routes of min(route.rpd, provider share of provider.rpd)
pool tokens per day    = sum over routes of min(route.tpd, provider share of provider.tpd)
pool requests per min  = sum over routes of min(route.rpm, provider share of provider.rpm)
```

Rules of thumb:

1. **Daily pool first.** Choose `free.rpd` so that `expected_daily_active_clients * free.rpd * utilization <= pool requests per day`. Most clients use a small part of their allowance, so a utilization of 0.1 to 0.3 is a typical planning figure, but measure it from `usage_24h.free_requests` and the number of tracked clients in the Routing tab instead of guessing.
2. **Tokens, not just requests.** A request can use thousands of tokens. A pool of 200,000 tokens per day serves 200 requests of 1,000 tokens, whatever the request counter says. Use `free.max_output_tokens` and `free.max_body_bytes` to bound the cost of one request, and set `free.tpd` so that `free.rpd * average tokens per request` does not hide a larger allowance than the pool has.
3. **Minute limit.** A pool of `P` requests per minute serves roughly `P / free.rpm` clients that all burst in the same minute. The per-minute limit is a burst control. The daily limit is the capacity control.
4. **Global cap.** Set `free.max_concurrent_total` to about the number of simultaneous requests the free pool can serve (the sum of route `concurrent` caps, or `P * average seconds per request / 60`). When it is full, callers get a quick `503` instead of a pile-up.
5. **Headroom for failure.** Plan for one provider being out. If losing the largest provider leaves the pool too small, add a route, lower `free.rpd`, or accept `503 capacity_exhausted` at peak.
6. **Keep paid separate.** Give paid models their own providers (keys) so that anonymous load cannot use up the budget that paying customers depend on. The free tier has no reserved share on a provider that both tiers use.

**Example.** A free OpenRouter account allows about 50 requests per day on `:free` models (UNVERIFIED, it depends on purchased credits). With `free.rpd = 300` a single client could use that in minutes, so the provider's budget is the real limit and every other client sees `503 capacity_exhausted` once it is spent. Either add routes and keys until the pool covers the audience (for 100 daily clients at 5 requests each you need about 500 requests per day), lower `free.rpd` to a handful, or back the model with providers that have larger budgets. Put the provider's documented limit on the route so the router never sends a request the upstream would refuse.

Tune from data: the Routing tab shows remaining daily budgets per provider and route, `usage_24h.rate_limited` (how often the upstream refused), and the number of free clients. A route with `rate_limited` above zero but a configured budget means your numbers are higher than the provider's real limits.

## Operator checklist

- [ ] Every free route has `limits` copied from the provider's current documentation.
- [ ] You read each free provider's terms about serving third parties and about training on prompts.
- [ ] `client_ip` names the header your own proxy sets (`"x-forwarded-for"` behind Caddy or nginx, `"cf-connecting-ip"` behind Cloudflare), and direct access to the app is blocked.
- [ ] Free and paid models use different providers, or you accept shared budgets.
- [ ] `free.max_concurrent_total`, `free.rpd`, and `free.tpd` fit the pool's real capacity.
- [ ] `free.max_output_tokens` and `reasoning.free_effort` bound the cost of one free request.
- [ ] You watch the Routing tab for open circuits, cooling providers, and empty budgets.
- [ ] Your users know free-model prompts leave your server (see the Privacy Policy text and your own notice).

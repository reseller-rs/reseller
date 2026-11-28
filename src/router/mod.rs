//! Route selection and failover for the model catalog.
//!
//! Every public model lists routes (a provider and an upstream model name).
//! The router keeps in-memory health and budget state per provider and per
//! route, picks the route for each attempt, and learns from the outcomes:
//!
//! - A route is a candidate when its provider is enabled and not cooling
//!   down, the route is not cooling down, its circuit is not open, every
//!   configured limit has headroom and concurrency is below the caps.
//! - `ordered` models take the first candidate. `balanced` models take the
//!   candidate with the best `weight * headroom * success / (1 + in_flight)`
//!   score, and the least recently picked one on a tie.
//! - Picking consumes the request budgets and takes an in-flight [`Permit`].
//!   Token budgets are charged when the request has settled.
//! - Each answer is classified from its status and error body together
//!   ([`Outcome::of`]). A rate limit (429, or a rate-limit body under another
//!   status) cools the route down until the upstream's hint, else
//!   `routing.cooldown_seconds`. Authentication, credit and region failures
//!   cool the whole provider down. A missing or retired model (404) cools the
//!   route down for `routing.cooldown_seconds` without counting as a circuit
//!   failure, so a broken route of an `ordered` model is not retried first
//!   on every request. A request too large for the route (a token window
//!   below the request, a context window), a moderation refusal, and the
//!   other client errors say nothing about health on their own. A route that
//!   refused a request with 400 or 422 while a later route served the same
//!   request is cooled down like a 404 (some providers answer an unknown
//!   model with 400). 5xx, timeouts and connection errors count
//!   toward the route's circuit breaker. The circuit opens after
//!   `routing.failure_threshold` consecutive failures, stays open for a
//!   doubling, jittered cooldown, then lets one probe request through.
//!
//! Nothing here ever waits: when no route is available the caller gets the
//! time until the soonest one is.
//!
//! State is keyed by provider id and upstream model, so a settings change
//! keeps the state of every route that survives it.
pub mod free;
pub mod hints;
pub mod outcome;

use crate::{
    config::{Budget, Config, Model, Provider, Route, Routing, Strategy},
    db::{DAY as DAY_SECS, now as unix_now},
    security::limiter::{Limiter, seconds},
};
use hints::Hints;
pub use outcome::Outcome;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MINUTE: Duration = Duration::from_secs(60);
const DAY: Duration = Duration::from_secs(86_400);
/// Share of the success rate kept per new outcome.
const SUCCESS_ALPHA: f64 = 0.1;
/// Share of the latency average kept per new response.
const LATENCY_ALPHA: f64 = 0.2;
/// Smallest headroom factor, so a nearly full route still gets some traffic.
const HEADROOM_FLOOR: f64 = 0.05;
/// How long a learned window stays believed when the upstream gives no reset.
const LEARNED_DEFAULT: Duration = Duration::from_secs(60);
/// Retry hint when the only obstacle is concurrency or a probe in flight.
const BUSY_RETRY: Duration = Duration::from_secs(1);

/// In-memory router state, shared by all requests.
#[derive(Default)]
pub struct Router {
    state: Mutex<State>,
    limiter: Limiter,
    /// Admission of anonymous and free-tier requests.
    pub free: Arc<free::Gate>,
}

#[derive(Default)]
struct State {
    providers: HashMap<String, ProviderState>,
    routes: HashMap<(String, String), RouteState>,
}

#[derive(Default)]
struct ProviderState {
    in_flight: u32,
    cooldown_until: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Circuit {
    Closed,
    Open(Instant),
    HalfOpen { probing: bool },
}

/// A rate-limit window an upstream reported.
#[derive(Clone, Copy)]
struct Window {
    limit: Option<u64>,
    remaining: u64,
    until: Instant,
}

impl Window {
    /// Remaining share of the window while it is still believed.
    fn fraction(&self, now: Instant) -> Option<f64> {
        let limit = self.limit.filter(|l| *l > 0)?;
        (now < self.until).then(|| (self.remaining as f64 / limit as f64).clamp(0.0, 1.0))
    }
}

struct RouteState {
    in_flight: u32,
    failures: u32,
    reopens: u32,
    circuit: Circuit,
    cooldown_until: Option<Instant>,
    requests: Option<Window>,
    tokens: Option<Window>,
    latency_ms: Option<f64>,
    success: f64,
    last_picked: Option<Instant>,
}

impl Default for RouteState {
    fn default() -> Self {
        Self {
            in_flight: 0,
            failures: 0,
            reopens: 0,
            circuit: Circuit::Closed,
            cooldown_until: None,
            requests: None,
            tokens: None,
            latency_ms: None,
            success: 1.0,
            last_picked: None,
        }
    }
}

fn provider_key(provider: &str, window: &str) -> String {
    format!("p\u{0}{provider}\u{0}{window}")
}

fn route_key(provider: &str, model: &str, window: &str) -> String {
    format!("r\u{0}{provider}\u{0}{model}\u{0}{window}")
}

/// The four windows of a budget: name, limit and length.
fn windows(budget: &Budget) -> [(&'static str, u32, Duration); 4] {
    [
        ("rpm", budget.rpm, MINUTE),
        ("rpd", budget.rpd, DAY),
        ("tpm", budget.tpm, MINUTE),
        ("tpd", budget.tpd, DAY),
    ]
}

/// +/-20% of `d`.
fn jitter(d: Duration) -> Duration {
    let mut byte = [0u8; 2];
    let _ = getrandom::fill(&mut byte);
    let unit = f64::from(u16::from_le_bytes(byte)) / f64::from(u16::MAX);
    d.mul_f64(0.8 + 0.4 * unit)
}

/// The retry hint for `soonest`: `routing.cooldown_seconds` when nothing is
/// known, never below [`BUSY_RETRY`] or above `routing.max_cooldown_seconds`.
fn clamp_wait(config: &Config, soonest: Option<Duration>) -> Duration {
    let cap = Duration::from_secs(config.routing.max_cooldown_seconds);
    let fallback = Duration::from_secs(config.routing.cooldown_seconds);
    soonest
        .unwrap_or(fallback)
        .clamp(BUSY_RETRY, cap.max(BUSY_RETRY))
}

/// A route chosen for one attempt.
pub struct Picked {
    /// Index into the model's `routes`.
    pub index: usize,
    pub permit: Permit,
}

/// An attempt in flight on one route. Dropping it releases the in-flight
/// slots, so keep it until the response has been relayed.
pub struct Permit {
    router: Arc<Router>,
    pub provider: String,
    pub model: String,
    routing: Routing,
    provider_limits: Budget,
    route_limits: Budget,
    probe: bool,
    started: Instant,
    finished: bool,
}

impl Permit {
    /// Reports how the upstream answered (see [`Outcome::of`]) and its hints.
    pub fn finish(&mut self, outcome: Outcome, hints: &Hints) {
        self.finish_at(outcome, hints, Instant::now());
    }

    fn finish_at(&mut self, outcome: Outcome, hints: &Hints, now: Instant) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let latency = now.saturating_duration_since(self.started);
        let mut st = self.router.lock();
        let route = st
            .routes
            .entry((self.provider.clone(), self.model.clone()))
            .or_default();
        route.learn(hints, now);
        let mut provider_until = None;
        match outcome {
            Outcome::Success => {
                route.record(true, Some(latency));
                route.close();
            }
            Outcome::RateLimited => {
                route.record(false, Some(latency));
                // The upstream answered, so the route is alive.
                route.close();
                let wait = hints
                    .cooldown()
                    .unwrap_or(Duration::from_secs(self.routing.cooldown_seconds))
                    .min(Duration::from_secs(self.routing.max_cooldown_seconds));
                route.cool_until(now + wait);
            }
            Outcome::NotFound => {
                // The upstream answered, so the circuit is left alone.
                route.release_probe();
                route.cool_until(now + Duration::from_secs(self.routing.cooldown_seconds));
            }
            Outcome::ProviderDown => {
                route.record(false, Some(latency));
                route.release_probe();
                provider_until = Some(now + Duration::from_secs(self.routing.max_cooldown_seconds));
            }
            Outcome::Failure => {
                route.record(false, None);
                route.fail(&self.routing, now);
            }
            // The route answered, about this request only.
            Outcome::TooLarge
            | Outcome::Refused
            | Outcome::Rejected
            | Outcome::Transient
            | Outcome::Final => route.release_probe(),
        }
        if let Some(until) = provider_until {
            st.providers
                .entry(self.provider.clone())
                .or_default()
                .cooldown_until = Some(until);
        }
    }

    /// Charges the tokens of a settled request to the token budgets of the
    /// provider and the route.
    pub fn charge_tokens(&self, tokens: u64) {
        let limiter = &self.router.limiter;
        for (name, limit, period) in windows(&self.provider_limits) {
            if name.starts_with('t') {
                limiter.record_n(&provider_key(&self.provider, name), limit, period, tokens);
            }
        }
        for (name, limit, period) in windows(&self.route_limits) {
            if name.starts_with('t') {
                limiter.record_n(
                    &route_key(&self.provider, &self.model, name),
                    limit,
                    period,
                    tokens,
                );
            }
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut st = self.router.lock();
        if let Some(p) = st.providers.get_mut(&self.provider) {
            p.in_flight = p.in_flight.saturating_sub(1);
        }
        if let Some(r) = st
            .routes
            .get_mut(&(self.provider.clone(), self.model.clone()))
        {
            r.in_flight = r.in_flight.saturating_sub(1);
            if self.probe && !self.finished {
                r.release_probe();
            }
        }
    }
}

impl RouteState {
    /// Folds one outcome into the success rate (and the latency, when a
    /// response head arrived).
    fn record(&mut self, ok: bool, latency: Option<Duration>) {
        self.success += SUCCESS_ALPHA * (f64::from(u8::from(ok)) - self.success);
        if let Some(latency) = latency {
            let ms = latency.as_secs_f64() * 1000.0;
            self.latency_ms = Some(match self.latency_ms {
                Some(old) => old + LATENCY_ALPHA * (ms - old),
                None => ms,
            });
        }
    }

    /// Closes the circuit after a sign of life.
    fn close(&mut self) {
        self.circuit = Circuit::Closed;
        self.failures = 0;
        self.reopens = 0;
    }

    /// Lets the next request probe again after an inconclusive probe.
    fn release_probe(&mut self) {
        if let Circuit::HalfOpen { probing: true } = self.circuit {
            self.circuit = Circuit::HalfOpen { probing: false };
        }
    }

    fn cool_until(&mut self, until: Instant) {
        if self.cooldown_until.is_none_or(|old| old < until) {
            self.cooldown_until = Some(until);
        }
    }

    /// Counts a failure and opens the circuit when it should open.
    fn fail(&mut self, routing: &Routing, now: Instant) {
        self.failures += 1;
        let probe = matches!(self.circuit, Circuit::HalfOpen { .. });
        if probe || self.failures >= routing.failure_threshold {
            let base = Duration::from_secs(routing.cooldown_seconds);
            let cap = Duration::from_secs(routing.max_cooldown_seconds);
            let doubled = base.saturating_mul(1u32 << self.reopens.min(20));
            self.circuit = Circuit::Open(now + jitter(doubled.min(cap)).min(cap));
            self.reopens += 1;
            self.failures = 0;
        }
    }

    /// Remembers what the upstream said about its limits. An empty window
    /// parks the route until it resets.
    fn learn(&mut self, hints: &Hints, now: Instant) {
        let mut park: Option<Instant> = None;
        for (slot, quota) in [
            (&mut self.requests, hints.requests),
            (&mut self.tokens, hints.tokens),
        ] {
            let Some(remaining) = quota.remaining else {
                continue;
            };
            let until = now + quota.reset.unwrap_or(LEARNED_DEFAULT);
            *slot = Some(Window {
                limit: quota.limit.or_else(|| slot.and_then(|w| w.limit)),
                remaining,
                until,
            });
            if remaining == 0 {
                park = park.max(Some(until));
            }
        }
        if let Some(until) = park {
            self.cool_until(until);
        }
    }

    fn view(&self, now: Instant) -> &'static str {
        match self.circuit {
            Circuit::Closed => "closed",
            Circuit::Open(until) if now < until => "open",
            _ => "half_open",
        }
    }
}

impl Router {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Plain counters and instants: a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How long until `route` can take a request, or its headroom (the
    /// smallest remaining share of any budget, 1 when nothing is limited).
    fn evaluate(
        &self,
        st: &State,
        provider: &Provider,
        route: &Route,
        now: Instant,
    ) -> Result<f64, Duration> {
        let until = |t: Instant| {
            t.saturating_duration_since(now)
                .max(Duration::from_millis(1))
        };
        let p = st.providers.get(&provider.id);
        if let Some(t) = p.and_then(|p| p.cooldown_until).filter(|t| *t > now) {
            return Err(until(t));
        }
        let r = st.routes.get(&(provider.id.clone(), route.model.clone()));
        if let Some(r) = r {
            if let Some(t) = r.cooldown_until.filter(|t| *t > now) {
                return Err(until(t));
            }
            match r.circuit {
                Circuit::Open(t) if t > now => return Err(until(t)),
                Circuit::HalfOpen { probing: true } => return Err(BUSY_RETRY),
                _ => {}
            }
        }
        let caps = [
            (p.map_or(0, |p| p.in_flight), provider.limits.concurrent),
            (r.map_or(0, |r| r.in_flight), route.limits.concurrent),
        ];
        let mut headroom = 1.0f64;
        for (in_flight, cap) in caps {
            if cap > 0 {
                if in_flight >= cap {
                    return Err(BUSY_RETRY);
                }
                headroom = headroom.min(1.0 - f64::from(in_flight) / f64::from(cap));
            }
        }
        self.check_budget(
            &provider.limits,
            |w| provider_key(&provider.id, w),
            &mut headroom,
        )?;
        self.check_budget(
            &route.limits,
            |w| route_key(&provider.id, &route.model, w),
            &mut headroom,
        )?;
        if let Some(r) = r {
            for window in [r.requests, r.tokens].into_iter().flatten() {
                if let Some(f) = window.fraction(now) {
                    headroom = headroom.min(f);
                }
            }
        }
        Ok(headroom.max(HEADROOM_FLOOR))
    }

    /// Fails with the wait when a configured window of `budget` is spent,
    /// else lowers `headroom` to the smallest remaining share.
    fn check_budget(
        &self,
        budget: &Budget,
        key: impl Fn(&str) -> String,
        headroom: &mut f64,
    ) -> Result<(), Duration> {
        for (name, limit, period) in windows(budget).into_iter().filter(|w| w.1 > 0) {
            let key = key(name);
            if let Some(wait) = self.limiter.blocked_for(&key, limit, period, 1) {
                return Err(wait);
            }
            let left = self.limiter.remaining(&key, limit, period);
            *headroom = headroom.min(f64::from(left.remaining) / f64::from(limit));
        }
        Ok(())
    }

    /// Picks the route for one attempt at `model`, skipping the routes
    /// (indexes into `model.routes`) already `tried`. Fallback routes are
    /// considered only when no other route is a candidate, or with
    /// `fallback_only` (the caller spent its attempts on the others), and
    /// then in listed order. Consumes the request budgets of the pick.
    /// Without a candidate it returns how long until the soonest route
    /// becomes available.
    pub fn pick(
        self: &Arc<Self>,
        config: &Config,
        model: &Model,
        tried: &[usize],
        fallback_only: bool,
    ) -> Result<Picked, Duration> {
        self.pick_at(config, model, tried, fallback_only, Instant::now())
    }

    fn pick_at(
        self: &Arc<Self>,
        config: &Config,
        model: &Model,
        tried: &[usize],
        fallback_only: bool,
        now: Instant,
    ) -> Result<Picked, Duration> {
        let mut st = self.lock();
        let mut best: Option<(usize, f64, Option<Instant>)> = None;
        let mut soonest: Option<Duration> = None;
        let passes: &[bool] = if fallback_only {
            &[true]
        } else {
            &[false, true]
        };
        for &fallback in passes {
            if best.is_some() {
                break;
            }
            for (index, route) in model.routes.iter().enumerate() {
                if tried.contains(&index) || route.fallback != fallback {
                    continue;
                }
                let Some(provider) = config.provider(&route.provider).filter(|p| p.enabled) else {
                    continue;
                };
                match self.evaluate(&st, provider, route, now) {
                    Err(wait) => soonest = Some(soonest.map_or(wait, |s| s.min(wait))),
                    Ok(headroom) => {
                        let r = st.routes.get(&(provider.id.clone(), route.model.clone()));
                        let in_flight = r.map_or(0, |r| r.in_flight);
                        let success = r.map_or(1.0, |r| r.success);
                        let last = r.and_then(|r| r.last_picked);
                        let score = f64::from(route.weight) * headroom * success
                            / (1.0 + f64::from(in_flight));
                        let better = match &best {
                            None => true,
                            Some(_) if fallback || model.strategy == Strategy::Ordered => false,
                            Some((_, top, top_last)) => {
                                score > top + 1e-9
                                    || ((score - top).abs() <= 1e-9 && last < *top_last)
                            }
                        };
                        if better {
                            best = Some((index, score, last));
                        }
                    }
                }
            }
        }
        let Some((index, _, _)) = best else {
            return Err(clamp_wait(config, soonest));
        };
        let route = &model.routes[index];
        let provider = config
            .provider(&route.provider)
            .expect("the candidate's provider exists");
        // Every limit was peeked under this lock, so the consumption below
        // cannot be refused.
        for (name, limit, period) in windows(&provider.limits).into_iter().take(2) {
            let _ = self
                .limiter
                .try_acquire(&provider_key(&provider.id, name), limit, period, 1);
        }
        for (name, limit, period) in windows(&route.limits).into_iter().take(2) {
            let _ = self.limiter.try_acquire(
                &route_key(&provider.id, &route.model, name),
                limit,
                period,
                1,
            );
        }
        st.providers
            .entry(provider.id.clone())
            .or_default()
            .in_flight += 1;
        let state = st
            .routes
            .entry((provider.id.clone(), route.model.clone()))
            .or_default();
        state.in_flight += 1;
        state.last_picked = Some(now);
        let probe = match state.circuit {
            Circuit::Closed => false,
            Circuit::Open(_) | Circuit::HalfOpen { .. } => {
                state.circuit = Circuit::HalfOpen { probing: true };
                true
            }
        };
        Ok(Picked {
            index,
            permit: Permit {
                router: Arc::clone(self),
                provider: provider.id.clone(),
                model: route.model.clone(),
                routing: config.routing,
                provider_limits: provider.limits,
                route_limits: route.limits,
                probe,
                started: now,
                finished: false,
            },
        })
    }

    /// How long until some route of `model` can take a request, without
    /// taking it: the retry hint for a client whose attempts all failed.
    pub fn soonest(&self, config: &Config, model: &Model) -> Duration {
        let st = self.lock();
        let now = Instant::now();
        let mut soonest: Option<Duration> = None;
        for route in &model.routes {
            let Some(provider) = config.provider(&route.provider).filter(|p| p.enabled) else {
                continue;
            };
            let wait = self
                .evaluate(&st, provider, route, now)
                .err()
                .unwrap_or(BUSY_RETRY);
            soonest = Some(soonest.map_or(wait, |s| s.min(wait)));
        }
        clamp_wait(config, soonest)
    }

    /// Cools down a route that refused a request (400) which a later route
    /// of the same model then served: the refusal was about the route (for
    /// example an unknown model at that provider), not about the request.
    pub fn refused_where_others_served(&self, routing: &Routing, provider: &str, model: &str) {
        self.refused_at(routing, provider, model, Instant::now());
    }

    fn refused_at(&self, routing: &Routing, provider: &str, model: &str, now: Instant) {
        self.lock()
            .routes
            .entry((provider.to_owned(), model.to_owned()))
            .or_default()
            .cool_until(now + Duration::from_secs(routing.cooldown_seconds));
    }

    /// Drops the state of providers and routes that the settings no longer
    /// mention (and that have nothing in flight).
    pub fn retain(&self, config: &Config) {
        let pairs = config.route_pairs();
        let mut st = self.lock();
        st.routes.retain(|(provider, model), r| {
            r.in_flight > 0 || pairs.iter().any(|(p, m, _)| p == provider && m == model)
        });
        st.providers
            .retain(|id, p| p.in_flight > 0 || config.provider(id).is_some());
    }

    /// Seeds the daily budgets from the last 24 hours of `route_usage`, so a
    /// restart does not hand every free quota back.
    pub async fn warm_start(&self, pool: &sqlx::SqlitePool, config: &Config) -> sqlx::Result<()> {
        let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
            "SELECT provider,model,SUM(requests),SUM(tokens) FROM route_usage \
             WHERE hour>=? GROUP BY provider,model",
        )
        .bind((unix_now() - DAY_SECS) / 3600 * 3600)
        .fetch_all(pool)
        .await?;
        let mut per_provider: HashMap<&str, (i64, i64)> = HashMap::new();
        for (provider, model, requests, tokens) in &rows {
            let Some(budget) = config
                .models
                .iter()
                .flat_map(|m| &m.routes)
                .find(|r| &r.provider == provider && &r.model == model)
                .map(|r| r.limits)
            else {
                continue;
            };
            let used = per_provider.entry(provider).or_default();
            used.0 += requests;
            used.1 += tokens;
            self.seed(&route_key(provider, model, "rpd"), budget.rpd, *requests);
            self.seed(&route_key(provider, model, "tpd"), budget.tpd, *tokens);
        }
        for (provider, (requests, tokens)) in per_provider {
            if let Some(p) = config.provider(provider) {
                self.seed(&provider_key(&p.id, "rpd"), p.limits.rpd, requests);
                self.seed(&provider_key(&p.id, "tpd"), p.limits.tpd, tokens);
            }
        }
        Ok(())
    }

    fn seed(&self, key: &str, limit: u32, used: i64) {
        if limit > 0 && used > 0 {
            self.limiter
                .record_n(key, limit, DAY, u64::try_from(used).unwrap_or(0));
        }
    }

    /// Live state of every provider and route, for the administrator API.
    pub fn snapshot(&self, config: &Config) -> Value {
        let now = Instant::now();
        let unix = unix_now();
        let at = |t: Option<Instant>| -> Value {
            t.filter(|t| *t > now).map_or(Value::Null, |t| {
                json!(unix + i64::try_from(seconds(t - now)).unwrap_or(i64::MAX / 2))
            })
        };
        let limits = |budget: &Budget, key: &dyn Fn(&str) -> String| -> Value {
            let mut out = serde_json::Map::new();
            for (name, limit, period) in windows(budget).into_iter().filter(|w| w.1 > 0) {
                let r = self.limiter.remaining(&key(name), limit, period);
                out.insert(
                    name.into(),
                    json!({
                        "limit": limit,
                        "remaining": r.remaining,
                        "reset_seconds": if r.reset.is_zero() { 0 } else { seconds(r.reset) },
                    }),
                );
            }
            if budget.concurrent > 0 {
                out.insert("concurrent".into(), json!({"limit": budget.concurrent}));
            }
            Value::Object(out)
        };
        let st = self.lock();
        let providers: Vec<Value> = config
            .providers
            .iter()
            .map(|p| {
                let s = st.providers.get(&p.id);
                let mut limits = limits(&p.limits, &|w| provider_key(&p.id, w));
                if let Some(c) = limits.get_mut("concurrent") {
                    c["in_flight"] = s.map_or(0, |s| s.in_flight).into();
                }
                json!({
                    "id": p.id,
                    "enabled": p.enabled,
                    "in_flight": s.map_or(0, |s| s.in_flight),
                    "cooldown_until": at(s.and_then(|s| s.cooldown_until)),
                    "limits": limits,
                })
            })
            .collect();
        let routes: Vec<Value> = config
            .route_pairs()
            .into_iter()
            .map(|(provider, model, users)| {
                let budget = config
                    .models
                    .iter()
                    .flat_map(|m| &m.routes)
                    .find(|r| r.provider == provider && r.model == model)
                    .map(|r| r.limits)
                    .unwrap_or_default();
                let s = st.routes.get(&(provider.to_owned(), model.to_owned()));
                json!({
                    "provider": provider,
                    "model": model,
                    "models": users,
                    "in_flight": s.map_or(0, |s| s.in_flight),
                    "circuit": s.map_or("closed", |s| s.view(now)),
                    "failures": s.map_or(0, |s| s.failures),
                    "cooldown_until": at(s.and_then(|s| s.cooldown_until)),
                    "latency_ms": s.and_then(|s| s.latency_ms).map(|ms| ms.round()),
                    "success_rate": s.map_or(1.0, |s| (s.success * 1000.0).round() / 1000.0),
                    "limits": limits(&budget, &|w| route_key(provider, model, w)),
                })
            })
            .collect();
        let (free_in_flight, free_subjects) = self.free.stats();
        json!({
            "providers": providers,
            "routes": routes,
            "free": {"in_flight": free_in_flight, "subjects": free_subjects},
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Capability, Provider};

    fn config(models: Vec<Model>) -> Config {
        Config {
            providers: ["a", "b", "c"]
                .into_iter()
                .map(|id| Provider {
                    id: id.into(),
                    base_url: "https://x.example/v1".into(),
                    ..Provider::default()
                })
                .collect(),
            models,
            routing: Routing {
                max_attempts: 3,
                failure_threshold: 2,
                cooldown_seconds: 10,
                max_cooldown_seconds: 100,
            },
            ..Config::default()
        }
    }

    fn route(provider: &str, model: &str) -> Route {
        Route {
            provider: provider.into(),
            model: model.into(),
            ..Route::default()
        }
    }

    fn model(strategy: Strategy, routes: Vec<Route>) -> Model {
        Model {
            id: "m".into(),
            capability: Some(Capability::Chat),
            strategy,
            routes,
            ..Model::default()
        }
    }

    fn router() -> Arc<Router> {
        Arc::new(Router::default())
    }

    #[test]
    fn ordered_takes_the_first_available_route() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let c = config(vec![m.clone()]);
        for _ in 0..4 {
            assert_eq!(r.pick(&c, &m, &[], false).ok().unwrap().index, 0);
        }
        // Skipping a tried route moves on, and nothing is left after both.
        assert_eq!(r.pick(&c, &m, &[0], false).ok().unwrap().index, 1);
        assert!(r.pick(&c, &m, &[0, 1], false).is_err());
    }

    #[test]
    fn balanced_spreads_equal_routes_and_favours_weight() {
        let r = router();
        let m = model(Strategy::Balanced, vec![route("a", "x"), route("b", "y")]);
        let c = config(vec![m.clone()]);
        let picks: Vec<usize> = (0..6)
            .map(|_| r.pick(&c, &m, &[], false).ok().unwrap().index)
            .collect();
        assert_eq!(
            picks,
            [0, 1, 0, 1, 0, 1],
            "least recently picked breaks ties"
        );
        let heavy = model(
            Strategy::Balanced,
            vec![
                Route {
                    weight: 1,
                    ..route("a", "x2")
                },
                Route {
                    weight: 5,
                    ..route("b", "y2")
                },
            ],
        );
        let c = config(vec![heavy.clone()]);
        assert_eq!(r.pick(&c, &heavy, &[], false).ok().unwrap().index, 1);
        // While the heavy route is busy, in-flight work shifts the score.
        let held: Vec<Picked> = (0..5)
            .map(|_| r.pick(&c, &heavy, &[], false).ok().unwrap())
            .collect();
        assert!(held.iter().any(|p| p.index == 0));
    }

    #[test]
    fn budgets_make_routes_unavailable_and_lower_headroom() {
        let r = router();
        let limited = Route {
            limits: Budget {
                rpm: 2,
                ..Budget::default()
            },
            ..route("a", "x")
        };
        let m = model(Strategy::Balanced, vec![limited, route("b", "y")]);
        let c = config(vec![m.clone()]);
        let picks: Vec<usize> = (0..4)
            .map(|_| r.pick(&c, &m, &[], false).ok().unwrap().index)
            .collect();
        // Route a starts first, but its shrinking headroom sends the rest to b.
        assert_eq!(picks, [0, 1, 1, 1]);
        // Once a's two requests a minute are spent it is skipped entirely.
        let spent = model(
            Strategy::Ordered,
            vec![
                Route {
                    limits: Budget {
                        rpm: 1,
                        ..Budget::default()
                    },
                    ..route("c", "q")
                },
                route("b", "y"),
            ],
        );
        let c = config(vec![spent.clone()]);
        assert_eq!(r.pick(&c, &spent, &[], false).ok().unwrap().index, 0);
        assert_eq!(r.pick(&c, &spent, &[], false).ok().unwrap().index, 1);
        let only = model(
            Strategy::Ordered,
            vec![Route {
                limits: Budget {
                    rpm: 1,
                    ..Budget::default()
                },
                ..route("a", "z")
            }],
        );
        let c = config(vec![only.clone()]);
        assert!(r.pick(&c, &only, &[], false).is_ok());
        let wait = r.pick(&c, &only, &[], false).err().unwrap();
        assert!(
            wait >= Duration::from_secs(1) && wait <= Duration::from_secs(60),
            "{wait:?}"
        );
    }

    #[test]
    fn concurrency_caps_apply_per_provider_and_route() {
        let r = router();
        let m = model(
            Strategy::Ordered,
            vec![Route {
                limits: Budget {
                    concurrent: 1,
                    ..Budget::default()
                },
                ..route("a", "x")
            }],
        );
        let c = config(vec![m.clone()]);
        let held = r.pick(&c, &m, &[], false).ok().unwrap();
        assert!(r.pick(&c, &m, &[], false).is_err());
        drop(held);
        assert!(r.pick(&c, &m, &[], false).is_ok());
        let mut c = config(vec![m.clone()]);
        c.providers[0].limits.concurrent = 1;
        let free_route = model(Strategy::Ordered, vec![route("a", "other")]);
        let held = r.pick(&c, &free_route, &[], false).ok().unwrap();
        assert!(r.pick(&c, &free_route, &[], false).is_err(), "provider cap");
        drop(held);
    }

    #[test]
    fn circuit_opens_after_the_threshold_then_half_opens_for_one_probe() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        let fail = |r: &Arc<Router>, t: Instant| {
            let mut p = r.pick_at(&c, &m, &[], false, t).ok().unwrap().permit;
            p.finish_at(Outcome::of(Some(503), None), &Hints::default(), t);
        };
        fail(&r, t0);
        // One failure is below the threshold of two.
        assert!(r.pick_at(&c, &m, &[], false, t0).is_ok());
        fail(&r, t0);
        // Open: the wait is the first cooldown (10 s, +/-20%).
        let wait = r.pick_at(&c, &m, &[], false, t0).err().unwrap();
        assert!(
            wait >= Duration::from_millis(7900) && wait <= Duration::from_millis(12_100),
            "{wait:?}"
        );
        assert_eq!(r.snapshot(&c)["routes"][0]["circuit"], "open");
        // Half-open once the cooldown has passed: exactly one probe.
        let later = t0 + Duration::from_secs(13);
        let probe = r.pick_at(&c, &m, &[], false, later).ok().unwrap();
        assert!(probe.permit.probe);
        assert!(
            r.pick_at(&c, &m, &[], false, later).is_err(),
            "one probe at a time"
        );
        // A failed probe reopens at once with a doubled cooldown.
        let mut permit = probe.permit;
        permit.finish_at(Outcome::of(Some(500), None), &Hints::default(), later);
        drop(permit);
        let wait = r.pick_at(&c, &m, &[], false, later).err().unwrap();
        assert!(
            wait >= Duration::from_millis(15_900) && wait <= Duration::from_millis(24_100),
            "{wait:?}"
        );
        // A successful probe closes the circuit and resets the backoff.
        let much_later = later + Duration::from_secs(30);
        let mut probe = r
            .pick_at(&c, &m, &[], false, much_later)
            .ok()
            .unwrap()
            .permit;
        probe.finish_at(Outcome::of(Some(200), None), &Hints::default(), much_later);
        drop(probe);
        for _ in 0..3 {
            assert!(r.pick_at(&c, &m, &[], false, much_later).is_ok());
        }
        let st = r.lock();
        let s = &st.routes[&("a".to_owned(), "x".to_owned())];
        assert_eq!((s.circuit, s.failures, s.reopens), (Circuit::Closed, 0, 0));
    }

    #[test]
    fn an_abandoned_probe_does_not_wedge_the_circuit() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        for _ in 0..2 {
            let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
            p.finish_at(Outcome::of(None, None), &Hints::default(), t0);
        }
        let later = t0 + Duration::from_secs(20);
        drop(r.pick_at(&c, &m, &[], false, later).ok().unwrap());
        assert!(
            r.pick_at(&c, &m, &[], false, later).is_ok(),
            "probe released on drop"
        );
    }

    #[test]
    fn rate_limits_cool_the_route_for_the_hinted_time() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
        let hints = Hints {
            retry_after: Some(Duration::from_secs(40)),
            ..Hints::default()
        };
        p.finish_at(Outcome::of(Some(429), None), &hints, t0);
        drop(p);
        // Route a is parked, b takes the traffic, a returns after the hint.
        assert_eq!(r.pick_at(&c, &m, &[], false, t0).ok().unwrap().index, 1);
        assert_eq!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(39))
                .ok()
                .unwrap()
                .index,
            1
        );
        assert_eq!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(41))
                .ok()
                .unwrap()
                .index,
            0
        );
        // Without a hint the default cooldown applies, capped by the maximum.
        let mut p = r.pick_at(&c, &m, &[0], false, t0).ok().unwrap().permit;
        p.finish_at(Outcome::of(Some(429), None), &Hints::default(), t0);
        drop(p);
        let only_b = model(Strategy::Ordered, vec![route("b", "y")]);
        let wait = r.pick_at(&c, &only_b, &[], false, t0).err().unwrap();
        assert_eq!(wait, Duration::from_secs(10));
        // A 429 is not a breaker failure.
        let st = r.lock();
        assert_eq!(st.routes[&("b".to_owned(), "y".to_owned())].failures, 0);
    }

    #[test]
    fn an_empty_upstream_window_parks_the_route_until_its_reset() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
        let hints = Hints::parse(
            &{
                let mut h = axum::http::HeaderMap::new();
                h.insert("x-ratelimit-remaining-requests", "0".parse().unwrap());
                h.insert("x-ratelimit-reset-requests", "45s".parse().unwrap());
                h
            },
            None,
        );
        p.finish_at(Outcome::of(Some(200), None), &hints, t0);
        drop(p);
        let wait = r.pick_at(&c, &m, &[], false, t0).err().unwrap();
        assert_eq!(wait, Duration::from_secs(45));
        assert!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(46))
                .is_ok()
        );
    }

    #[test]
    fn auth_and_credit_failures_cool_the_whole_provider() {
        let r = router();
        let m1 = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let m2 = model(Strategy::Ordered, vec![route("a", "z")]);
        let c = config(vec![m1.clone(), m2.clone()]);
        let t0 = Instant::now();
        let mut p = r.pick_at(&c, &m1, &[], false, t0).ok().unwrap().permit;
        p.finish_at(Outcome::of(Some(401), None), &Hints::default(), t0);
        drop(p);
        // Provider a is out for the maximum cooldown, for every model.
        assert_eq!(r.pick_at(&c, &m1, &[], false, t0).ok().unwrap().index, 1);
        assert_eq!(
            r.pick_at(&c, &m2, &[], false, t0).err().unwrap(),
            Duration::from_secs(100)
        );
        assert!(
            r.pick_at(&c, &m2, &[], false, t0 + Duration::from_secs(101))
                .is_ok()
        );
        // An exhausted account (a 429 saying insufficient_quota) does the same.
        let t1 = t0 + Duration::from_secs(101);
        let mut p = r.pick_at(&c, &m1, &[0], false, t1).ok().unwrap().permit;
        p.finish_at(Outcome::ProviderDown, &Hints::default(), t1);
        drop(p);
        assert_eq!(r.pick_at(&c, &m1, &[], false, t1).ok().unwrap().index, 0);
        let only_b = model(Strategy::Ordered, vec![route("b", "y")]);
        assert_eq!(
            r.pick_at(&c, &only_b, &[], false, t1).err().unwrap(),
            Duration::from_secs(100)
        );
    }

    #[test]
    fn client_errors_say_nothing_about_health() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        for status in [400, 408, 409] {
            for _ in 0..3 {
                let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
                p.finish_at(Outcome::of(Some(status), None), &Hints::default(), t0);
            }
        }
        assert!(r.pick_at(&c, &m, &[], false, t0).is_ok());
    }

    #[test]
    fn a_missing_model_cools_the_route_without_tripping_the_breaker() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
        p.finish_at(Outcome::of(Some(404), None), &Hints::default(), t0);
        drop(p);
        // The route is skipped for the cooldown, then tried first again.
        assert_eq!(r.pick_at(&c, &m, &[], false, t0).ok().unwrap().index, 1);
        assert_eq!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(9))
                .ok()
                .unwrap()
                .index,
            1
        );
        assert_eq!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(11))
                .ok()
                .unwrap()
                .index,
            0
        );
        // Repeated 404s never open the circuit.
        let mut t = t0;
        for _ in 0..5 {
            t += Duration::from_secs(11);
            let mut p = r.pick_at(&c, &m, &[], false, t).ok().unwrap().permit;
            p.finish_at(Outcome::of(Some(404), None), &Hints::default(), t);
        }
        let st = r.lock();
        let route = &st.routes[&("a".to_owned(), "x".to_owned())];
        assert_eq!(route.failures, 0);
        assert!(matches!(route.circuit, Circuit::Closed));
    }

    #[test]
    fn a_route_refusing_what_another_served_is_cooled_down() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let c = config(vec![m.clone()]);
        let t0 = Instant::now();
        // A bare 400 says nothing: the route stays first.
        let mut p = r.pick_at(&c, &m, &[], false, t0).ok().unwrap().permit;
        p.finish_at(Outcome::of(Some(400), None), &Hints::default(), t0);
        drop(p);
        assert_eq!(r.pick_at(&c, &m, &[], false, t0).ok().unwrap().index, 0);
        // Once another route served the same request, it is skipped for the
        // cooldown, then tried first again.
        r.refused_at(&c.routing, "a", "x", t0);
        assert_eq!(r.pick_at(&c, &m, &[], false, t0).ok().unwrap().index, 1);
        assert_eq!(
            r.pick_at(&c, &m, &[], false, t0 + Duration::from_secs(11))
                .ok()
                .unwrap()
                .index,
            0
        );
        assert!(matches!(
            r.lock().routes[&("a".to_owned(), "x".to_owned())].circuit,
            Circuit::Closed
        ));
    }

    #[test]
    fn disabled_providers_are_skipped_and_retain_drops_stale_state() {
        let r = router();
        let m = model(Strategy::Ordered, vec![route("a", "x"), route("b", "y")]);
        let mut c = config(vec![m.clone()]);
        c.providers[0].enabled = false;
        assert_eq!(r.pick(&c, &m, &[], false).ok().unwrap().index, 1);
        c.providers[1].enabled = false;
        let wait = r.pick(&c, &m, &[], false).err().unwrap();
        assert_eq!(wait, Duration::from_secs(10), "falls back to the cooldown");
        let c = config(vec![m.clone()]);
        drop(r.pick(&c, &m, &[], false).ok().unwrap());
        assert_eq!(r.lock().routes.len(), 2);
        let trimmed = config(vec![model(Strategy::Ordered, vec![route("b", "y")])]);
        r.retain(&trimmed);
        let st = r.lock();
        assert_eq!(st.routes.len(), 1);
        assert!(st.routes.contains_key(&("b".to_owned(), "y".to_owned())));
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..200 {
            let d = jitter(Duration::from_secs(100));
            assert!(
                d >= Duration::from_secs(80) && d <= Duration::from_secs(120),
                "{d:?}"
            );
        }
    }
}

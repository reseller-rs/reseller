//! Helpers shared by the integration tests: providers, catalog models, and
//! funded accounts.
#![allow(dead_code)]

use reseller::{
    App,
    config::{Capability, Config, Model, Provider, Route, Strategy, Tier},
    db,
};

/// A base URL nothing listens on, for providers a test never reaches.
pub const UNREACHABLE: &str = "http://127.0.0.1:9/v1";

/// Adds provider `id`, or updates its URL and key.
pub fn provider(config: &mut Config, id: &str, base_url: &str, api_key: &str) {
    match config.providers.iter_mut().find(|p| p.id == id) {
        Some(p) => {
            p.base_url = base_url.into();
            p.api_key = api_key.into();
        }
        None => config.providers.push(Provider {
            id: id.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            ..Provider::default()
        }),
    }
}

/// Points an existing provider at a mock server (`base_url` includes `/v1`).
pub fn point(config: &mut Config, id: &str, base_url: &str, api_key: &str) {
    provider(config, id, base_url, api_key);
}

/// Adds (or replaces) the catalog model `id` with one route to the upstream
/// model `upstream` of `provider`. A provider the test has not configured is
/// added unreachable, so the model is servable (listed and resolved).
pub fn model<'a>(
    config: &'a mut Config,
    id: &str,
    capability: Capability,
    tier: Tier,
    provider: &str,
    upstream: &str,
) -> &'a mut Model {
    if !config.providers.iter().any(|p| p.id == provider) {
        self::provider(config, provider, UNREACHABLE, "upstream-secret");
    }
    config.models.retain(|m| m.id != id);
    config.models.push(Model {
        id: id.into(),
        tier,
        capability: Some(capability),
        routes: vec![Route {
            provider: provider.into(),
            model: upstream.into(),
            ..Route::default()
        }],
        ..Model::default()
    });
    config.models.last_mut().unwrap()
}

/// The provider `provider_id` (unreachable until pointed at a mock) serving
/// the paid default model `name` of `capability`, whose public and upstream
/// names are both `name`.
pub fn serve(config: &mut Config, provider_id: &str, capability: Capability, name: &str) {
    if !config.providers.iter().any(|p| p.id == provider_id) {
        provider(config, provider_id, UNREACHABLE, "upstream-secret");
    }
    model(config, name, capability, Tier::Paid, provider_id, name).default = true;
}

/// A paid default chat model `name` on provider `llm`.
pub fn llm(config: &mut Config, name: &str) {
    serve(config, "llm", Capability::Chat, name);
}

/// Adds a route to model `id` on `provider` for the upstream model `upstream`.
pub fn add_route(config: &mut Config, id: &str, provider: &str, upstream: &str) {
    let model = config.models.iter_mut().find(|m| m.id == id).unwrap();
    model.routes.push(Route {
        provider: provider.into(),
        model: upstream.into(),
        ..Route::default()
    });
}

/// Makes model `id` try its routes in order.
pub fn ordered(config: &mut Config, id: &str) {
    config
        .models
        .iter_mut()
        .find(|m| m.id == id)
        .unwrap()
        .strategy = Strategy::Ordered;
}

/// Gives the account `micro` micro-USD of non-expiring credit.
pub async fn fund(app: &App, account: &str, micro: i64) {
    let mut tx = db::write(&app.db).await.unwrap();
    reseller::billing::credit(&mut tx, account, micro, "admin", None, "")
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// 0.10 USD, what the signup used to grant.
pub const DIME: i64 = 100_000;

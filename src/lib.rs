//! Self-hosted OpenAI-compatible API reseller.
//!
//! # Module map
//!
//! - [`web`]: HTTP router, static assets, and request dispatch.
//! - [`proxy`]: streaming upstream transport (JSON, SSE, multipart, WebSocket).
//! - [`meter`]: request admission, usage accounting, and settlement.
//! - [`api`]: customer (`/v1/keys`, `/v1/account`) and administrator APIs.
//! - [`identity`] / [`keys`]: authentication and API key lifecycle.
//! - [`mcp`]: Model Context Protocol server (`/mcp`) and stdio bridge.
//! - [`oauth`]: OAuth 2.1 authorization server for `/mcp`.
//! - [`security`]: rate limiting and proof-of-work signup challenges.
//! - [`billing`] / [`money`]: credit ledger, plans, redeem codes, and micro-USD.
//! - [`payments`]: plan checkout orders via Stripe (cards) and Payroof (crypto).
//! - [`theme`]: white-label branding, the page template, and theme files.
//! - [`stats`]: dashboards, usage series, and log retention.
//! - [`config`] / [`db`] / [`error`]: settings, persistence, and the error type.
pub mod api;
pub mod billing;
pub mod config;
pub mod db;
pub mod error;
pub mod identity;
pub mod keys;
pub mod mcp;
pub mod meter;
pub mod money;
pub mod oauth;
pub mod payments;
pub mod proxy;
pub mod security;
pub mod stats;
pub mod theme;
pub mod web;

use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{RwLock, RwLockMappedWriteGuard, RwLockWriteGuard};

/// Shared application state. Cloning is cheap.
#[derive(Clone)]
pub struct App {
    pub db: sqlx::SqlitePool,
    config: Arc<RwLock<Arc<config::Config>>>,
    pub client: reqwest::Client,
    /// Background settlement tasks that must finish before shutdown.
    pub tasks: tokio_util::task::TaskTracker,
    /// Payroof catalog and exchange-rate caches.
    pub payroof: Arc<payments::payroof::Cache>,
    /// In-memory abuse limiter (per IP and per account).
    pub limiter: Arc<security::limiter::Limiter>,
    /// Optional directory of theme files served under `/theme/`.
    pub theme_dir: Option<Arc<PathBuf>>,
}

impl App {
    /// Creates the shared state with an HTTP client that never follows redirects.
    pub fn new(db: sqlx::SqlitePool, config: config::Config) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            config: Arc::new(RwLock::new(Arc::new(config))),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .build()?,
            tasks: tokio_util::task::TaskTracker::new(),
            payroof: Arc::default(),
            limiter: Arc::default(),
            theme_dir: None,
        })
    }

    /// Sets the directory of white-label files (logo, favicon, fonts,
    /// `theme.css`, and `index.html`).
    #[must_use]
    pub fn with_theme_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.theme_dir = dir.map(Arc::new);
        self
    }

    /// Immutable snapshot of the current settings.
    pub async fn config(&self) -> Arc<config::Config> {
        self.config.read().await.clone()
    }

    /// Returns exclusive, in-memory write access to the settings.
    ///
    /// Requests that already hold a snapshot keep their copy. Persist changes
    /// with [`db::save_settings`].
    pub async fn config_mut(&self) -> RwLockMappedWriteGuard<'_, config::Config> {
        RwLockWriteGuard::map(self.config.write().await, Arc::make_mut)
    }
}

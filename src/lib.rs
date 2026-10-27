//! Self-hosted OpenAI-compatible API reseller.
//!
//! Module map:
//! - [`web`]: HTTP router, static assets, and top-level dispatch.
//! - [`proxy`]: streaming upstream transport (JSON, SSE, multipart, WebSocket).
//! - [`meter`]: request admission, usage accounting, and settlement.
//! - [`api`]: customer (`/v1/keys`, `/v1/account`) and administrator APIs.
//! - [`identity`] / [`keys`]: authentication and API-key lifecycle.
//! - [`billing`] / [`money`]: credit ledger, plans, redeem codes, micro-USD.
//! - [`payments`]: plan checkout orders via Stripe (cards) and Payroof (crypto).
//! - [`theme`]: white-label branding, the page template, and theme files.
//! - [`stats`]: dashboards, usage series, and log retention.
//! - [`config`] / [`db`] / [`error`]: settings, persistence, error type.
pub mod api;
pub mod billing;
pub mod config;
pub mod db;
pub mod error;
pub mod identity;
pub mod keys;
pub mod meter;
pub mod money;
pub mod payments;
pub mod proxy;
pub mod stats;
pub mod theme;
pub mod web;

use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{RwLock, RwLockMappedWriteGuard, RwLockWriteGuard};

/// Shared application state; cheap to clone.
#[derive(Clone)]
pub struct App {
    pub db: sqlx::SqlitePool,
    config: Arc<RwLock<Arc<config::Config>>>,
    pub client: reqwest::Client,
    /// Background settlement tasks that must finish before shutdown.
    pub tasks: tokio_util::task::TaskTracker,
    /// Payroof catalog and exchange-rate caches.
    pub payroof: Arc<payments::payroof::Cache>,
    /// Optional directory of theme files served under `/theme/`.
    pub theme_dir: Option<Arc<PathBuf>>,
}

impl App {
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
            theme_dir: None,
        })
    }

    /// Serves white-label files (logo, favicon, fonts, `theme.css`,
    /// `index.html`) from `dir`.
    #[must_use]
    pub fn with_theme_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.theme_dir = dir.map(Arc::new);
        self
    }

    /// Immutable snapshot of the current settings.
    pub async fn config(&self) -> Arc<config::Config> {
        self.config.read().await.clone()
    }

    /// Exclusive, in-memory access to the settings. Requests already holding
    /// a snapshot keep their copy; persist changes with [`db::save_settings`].
    pub async fn config_mut(&self) -> RwLockMappedWriteGuard<'_, config::Config> {
        RwLockWriteGuard::map(self.config.write().await, Arc::make_mut)
    }
}

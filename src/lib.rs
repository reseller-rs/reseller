//! Self-hosted OpenAI-compatible API reseller.
//!
//! Module map:
//! - [`web`]: HTTP router, static assets, and top-level dispatch.
//! - [`proxy`]: streaming upstream transport (JSON, SSE, multipart, WebSocket).
//! - [`meter`]: request admission, usage accounting, and settlement.
//! - [`api`]: customer (`/v1/keys`, `/v1/account`) and administrator APIs.
//! - [`identity`] / [`keys`]: authentication and API-key lifecycle.
//! - [`billing`] / [`payments`] / [`money`]: credit ledger, plans, Stripe.
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
pub mod web;

use std::{sync::Arc, time::Duration};
use tokio::sync::{RwLock, RwLockMappedWriteGuard, RwLockWriteGuard};

/// Shared application state; cheap to clone.
#[derive(Clone)]
pub struct App {
    pub db: sqlx::SqlitePool,
    config: Arc<RwLock<Arc<config::Config>>>,
    pub client: reqwest::Client,
    /// Background settlement tasks that must finish before shutdown.
    pub tasks: tokio_util::task::TaskTracker,
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
        })
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

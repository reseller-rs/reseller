pub mod api;
pub mod billing;
pub mod config;
pub mod db;
pub mod error;
pub mod identity;
pub mod money;
pub mod payments;
pub mod proxy;
pub mod stats;
pub mod web;

use std::sync::Arc;
use tokio::sync::RwLock;
#[derive(Clone)]
pub struct App {
    pub db: sqlx::SqlitePool,
    pub config: Arc<RwLock<config::Config>>,
    pub client: reqwest::Client,
    pub tasks: tokio_util::task::TaskTracker,
}
impl App {
    pub fn new(db: sqlx::SqlitePool, config: config::Config) -> anyhow::Result<Self> {
        Ok(Self {
            db,
            config: Arc::new(RwLock::new(config)),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(15))
                .build()?,
            tasks: tokio_util::task::TaskTracker::new(),
        })
    }
}

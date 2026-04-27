pub mod chat;
pub mod error;
pub mod models;

use crate::config::Config;
use crate::pool::AccountPool;
use crate::usage::UsageDb;
use anyhow::Result;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<AccountPool>,
    pub http: reqwest::Client,
    pub usage: Option<Arc<UsageDb>>,
    pub usage_enabled: bool,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(models::list_models))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/healthz", get(health))
        .with_state(state)
}

async fn health() -> &'static str { "ok" }

pub fn build_state(cfg: &Config) -> Result<AppState> {
    let pool = AccountPool::from_config(cfg)?;
    let http = crate::util::http::build_client(&cfg.proxy)?;
    let usage = if cfg.usage.enabled {
        let path = cfg
            .usage
            .db_path
            .clone()
            .map(Ok)
            .unwrap_or_else(crate::config::paths::default_usage_db)?;
        Some(Arc::new(UsageDb::open(&path)?))
    } else {
        None
    };
    Ok(AppState {
        pool,
        http,
        usage,
        usage_enabled: cfg.usage.enabled,
    })
}

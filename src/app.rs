use std::{env, net::SocketAddr};

use anyhow::Context;
use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
use axum::{routing::get, routing::post, Json, Router};
use rand::rngs::OsRng;
use reqwest::Client;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use crate::{
    controllers::{customers, invoices, payments, webhooks},
    services,
    state::AppState,
};

pub async fn run() -> anyhow::Result<()> {
    let state = build_state().await?;

    tokio::spawn(services::payments::recovery_worker(state.clone()));
    tokio::spawn(services::webhooks::delivery_worker(state.clone()));

    let address: SocketAddr = "0.0.0.0:8080".parse()?;
    info!(%address, "invoice API listening");

    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, routes(state)).await?;
    Ok(())
}

fn routes(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/customers", post(customers::create).get(customers::list))
        .route("/customers/:id", get(customers::get))
        .route("/invoices", post(invoices::create).get(invoices::list))
        .route("/invoices/:id", get(invoices::get))
        .route("/invoices/:id/pay", post(payments::pay_invoice))
        .route(
            "/webhook-endpoints",
            post(webhooks::create_endpoint).get(webhooks::list_endpoints),
        )
        .route("/webhook-events", get(webhooks::list_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn build_state() -> anyhow::Result<AppState> {
    let database_url = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;

    sqlx::migrate!("./migrations").run(&db).await?;

    let dev_api_key =
        env::var("DEV_API_KEY").unwrap_or_else(|_| "dodo_test_dev_secret_change_me".to_string());
    ensure_seed_business(&db, &dev_api_key).await?;

    let mut webhook_key = [0u8; 32];
    let key_source =
        env::var("WEBHOOK_MASTER_KEY").unwrap_or_else(|_| "local-development-key".to_string());
    webhook_key.copy_from_slice(&Sha256::digest(key_source.as_bytes()));

    Ok(AppState {
        db,
        http: Client::builder().build()?,
        psp_url: env::var("PSP_URL").unwrap_or_else(|_| "http://localhost:8081".to_string()),
        webhook_key,
    })
}

async fn ensure_seed_business(db: &PgPool, api_key: &str) -> anyhow::Result<()> {
    let (prefix, secret) = api_key
        .strip_prefix("dodo_")
        .and_then(|value| value.split_once('_'))
        .context("DEV_API_KEY must look like dodo_<prefix>_<secret>")?;

    let business_id: Uuid =
        match sqlx::query_scalar("SELECT id FROM businesses WHERE name = 'Demo Business' LIMIT 1")
            .fetch_optional(db)
            .await?
        {
            Some(id) => id,
            None => {
                sqlx::query_scalar(
                    "INSERT INTO businesses (name) VALUES ('Demo Business') RETURNING id",
                )
                .fetch_one(db)
                .await?
            }
        };

    let salt = SaltString::generate(&mut OsRng);
    let secret_hash = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?
        .to_string();

    sqlx::query(
        "INSERT INTO api_keys (business_id, prefix, secret_hash) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (prefix) DO UPDATE \
         SET business_id = EXCLUDED.business_id, \
             secret_hash = EXCLUDED.secret_hash, \
             revoked_at = NULL",
    )
    .bind(business_id)
    .bind(prefix)
    .bind(secret_hash)
    .execute(db)
    .await?;

    info!(business_id = %business_id, "seed API key ready");
    Ok(())
}

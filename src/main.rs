mod app;
mod auth;
mod controllers;
mod error;
mod mock_psp;
mod models;
mod services;
mod state;

use std::env;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    match env::args().nth(1).as_deref() {
        Some("psp") => mock_psp::run().await,
        _ => app::run().await,
    }
}

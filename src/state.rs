use reqwest::Client;
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub http: Client,
    pub psp_url: String,
    pub webhook_key: [u8; 32],
}

use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use sqlx::Row;
use url::Url;

use crate::{
    auth::BusinessAuth,
    error::ApiError,
    models::{EndpointInput, EndpointResponse, WebhookEventSummary},
    services::webhooks::encrypt_secret,
    state::AppState,
};

pub async fn create_endpoint(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<EndpointInput>,
) -> Result<(StatusCode, Json<EndpointResponse>), ApiError> {
    validate_webhook_url(&input.url)?;

    let mut secret_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = format!("whsec_{}", URL_SAFE_NO_PAD.encode(secret_bytes));
    let encrypted_secret = encrypt_secret(&secret, &state.webhook_key)?;

    let row = sqlx::query(
        "INSERT INTO webhook_endpoints (business_id, url, secret_ciphertext) \
         VALUES ($1, $2, $3) RETURNING id, created_at",
    )
    .bind(auth.id)
    .bind(&input.url)
    .bind(encrypted_secret)
    .fetch_one(&state.db)
    .await
    .map_err(ApiError::db)?;

    let endpoint = EndpointResponse {
        id: row.try_get("id").map_err(ApiError::db)?,
        url: input.url,
        secret: Some(secret),
        active: true,
        created_at: row.try_get("created_at").map_err(ApiError::db)?,
    };

    Ok((StatusCode::CREATED, Json(endpoint)))
}

pub async fn list_endpoints(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<EndpointResponse>>, ApiError> {
    let rows = sqlx::query(
        "SELECT id, url, active, created_at FROM webhook_endpoints \
         WHERE business_id = $1 ORDER BY created_at DESC",
    )
    .bind(auth.id)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::db)?;

    let endpoints = rows
        .into_iter()
        .map(|row| EndpointResponse {
            id: row.try_get("id").unwrap_or_default(),
            url: row.try_get("url").unwrap_or_default(),
            secret: None,
            active: row.try_get("active").unwrap_or(false),
            created_at: row.try_get("created_at").unwrap_or_else(|_| Utc::now()),
        })
        .collect();

    Ok(Json(endpoints))
}

pub async fn list_events(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<WebhookEventSummary>>, ApiError> {
    let events = sqlx::query_as::<_, WebhookEventSummary>(
        "SELECT e.id, e.event_type, e.created_at, \
                COUNT(d.id)::BIGINT AS delivery_count, \
                COUNT(d.id) FILTER (WHERE d.status = 'delivered')::BIGINT AS delivered_count \
         FROM webhook_events e \
         LEFT JOIN webhook_deliveries d ON d.event_id = e.id \
         WHERE e.business_id = $1 \
         GROUP BY e.id ORDER BY e.created_at DESC",
    )
    .bind(auth.id)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::db)?;

    Ok(Json(events))
}

fn validate_webhook_url(value: &str) -> Result<(), ApiError> {
    let url = Url::parse(value).map_err(|_| invalid_webhook_url())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid_webhook_url());
    }
    Ok(())
}

fn invalid_webhook_url() -> ApiError {
    ApiError::client(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_webhook_url",
        "url must be valid http or https",
    )
}

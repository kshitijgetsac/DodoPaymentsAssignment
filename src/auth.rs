use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
use async_trait::async_trait;
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use sqlx::Row;
use uuid::Uuid;

use crate::{error::ApiError, state::AppState};

#[derive(Clone)]
pub struct BusinessAuth {
    pub id: Uuid,
}

#[async_trait]
impl FromRequestParts<AppState> for BusinessAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let authorization = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                ApiError::client(
                    StatusCode::UNAUTHORIZED,
                    "missing_api_key",
                    "Authorization: Bearer <key> is required",
                )
            })?;

        let raw_key = authorization.strip_prefix("Bearer ").ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "use a Bearer API key",
            )
        })?;

        let key_body = raw_key.strip_prefix("dodo_").ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid key format",
            )
        })?;
        let (prefix, secret) = key_body.split_once('_').ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid key format",
            )
        })?;

        let key = sqlx::query(
            "SELECT business_id, secret_hash FROM api_keys \
             WHERE prefix = $1 AND revoked_at IS NULL",
        )
        .bind(prefix)
        .fetch_optional(&state.db)
        .await
        .map_err(ApiError::db)?
        .ok_or_else(invalid_api_key)?;

        let business_id: Uuid = key.try_get("business_id").map_err(ApiError::db)?;
        let secret_hash: String = key.try_get("secret_hash").map_err(ApiError::db)?;
        let parsed_hash = PasswordHash::new(&secret_hash).map_err(|_| invalid_api_key())?;

        Argon2::default()
            .verify_password(secret.as_bytes(), &parsed_hash)
            .map_err(|_| invalid_api_key())?;

        Ok(Self { id: business_id })
    }
}

fn invalid_api_key() -> ApiError {
    ApiError::client(
        StatusCode::UNAUTHORIZED,
        "invalid_api_key",
        "invalid or revoked API key",
    )
}

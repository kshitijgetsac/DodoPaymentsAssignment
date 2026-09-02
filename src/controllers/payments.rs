use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    Json,
};
use uuid::Uuid;

use crate::{
    auth::BusinessAuth, error::ApiError, models::PayInput, services::payments, state::AppState,
};

pub async fn pay_invoice(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(input): Json<PayInput>,
) -> Result<Response, ApiError> {
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim();

    if idempotency_key.is_empty() || idempotency_key.len() > 200 {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_idempotency_key",
            "Idempotency-Key is required",
        ));
    }

    if input.card_token.trim().is_empty() {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_card_token",
            "card_token is required",
        ));
    }

    payments::pay_invoice(&state, auth.id, invoice_id, idempotency_key, input).await
}

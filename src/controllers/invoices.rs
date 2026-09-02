use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::{
    auth::BusinessAuth,
    error::ApiError,
    models::{Invoice, InvoiceDetail, InvoiceInput, InvoiceListQuery, LineItem, PaymentAttempt},
    services::invoices,
    state::AppState,
};

pub async fn create(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<InvoiceInput>,
) -> Result<(StatusCode, Json<InvoiceDetail>), ApiError> {
    let invoice = invoices::create(&state, auth.id, input).await?;
    Ok((StatusCode::CREATED, Json(invoice)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceDetail>, ApiError> {
    let invoice = sqlx::query_as::<_, Invoice>(
        "SELECT id, customer_id, status, currency, total_cents, due_date, created_at \
         FROM invoices WHERE id = $1 AND business_id = $2",
    )
    .bind(id)
    .bind(auth.id)
    .fetch_optional(&state.db)
    .await
    .map_err(ApiError::db)?
    .ok_or_else(invoice_not_found)?;

    let line_items = sqlx::query_as::<_, LineItem>(
        "SELECT id, description, quantity, unit_amount_cents, line_total_cents \
         FROM invoice_line_items WHERE invoice_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::db)?;

    let payment_attempts = sqlx::query_as::<_, PaymentAttempt>(
        "SELECT id, status, psp_ref, failure_code, created_at, updated_at \
         FROM payment_attempts WHERE invoice_id = $1 ORDER BY created_at DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::db)?;

    Ok(Json(InvoiceDetail {
        invoice,
        line_items,
        payment_attempts,
    }))
}

pub async fn list(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Query(query): Query<InvoiceListQuery>,
) -> Result<Json<Vec<Invoice>>, ApiError> {
    let invoices = match query.status.as_deref() {
        None => {
            sqlx::query_as::<_, Invoice>(
                "SELECT id, customer_id, status, currency, total_cents, due_date, created_at \
                 FROM invoices WHERE business_id = $1 ORDER BY created_at DESC",
            )
            .bind(auth.id)
            .fetch_all(&state.db)
            .await
        }
        Some(status @ ("open" | "paid")) => {
            sqlx::query_as::<_, Invoice>(
                "SELECT id, customer_id, status, currency, total_cents, due_date, created_at \
                 FROM invoices WHERE business_id = $1 AND status = $2 ORDER BY created_at DESC",
            )
            .bind(auth.id)
            .bind(status)
            .fetch_all(&state.db)
            .await
        }
        Some(_) => {
            return Err(ApiError::client(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_status",
                "status must be open or paid",
            ));
        }
    }
    .map_err(ApiError::db)?;

    Ok(Json(invoices))
}

fn invoice_not_found() -> ApiError {
    ApiError::client(
        StatusCode::NOT_FOUND,
        "invoice_not_found",
        "invoice not found",
    )
}

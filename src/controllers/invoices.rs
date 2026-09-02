use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde_json::json;
use uuid::Uuid;

use crate::{
    auth::BusinessAuth,
    error::ApiError,
    models::{Invoice, InvoiceDetail, InvoiceInput, InvoiceListQuery, LineItem, PaymentAttempt},
    services::webhooks::insert_event,
    state::AppState,
};

pub async fn create(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<InvoiceInput>,
) -> Result<(StatusCode, Json<InvoiceDetail>), ApiError> {
    let line_totals = validate_line_items(&input)?;
    let total_cents = line_totals.iter().try_fold(0i64, |total, value| {
        total.checked_add(*value).ok_or_else(amount_overflow)
    })?;

    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let customer_exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(input.customer_id)
    .bind(auth.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?
    .is_some();

    if !customer_exists {
        return Err(ApiError::client(
            StatusCode::NOT_FOUND,
            "customer_not_found",
            "customer not found",
        ));
    }

    let invoice = sqlx::query_as::<_, Invoice>(
        "INSERT INTO invoices \
            (business_id, customer_id, status, currency, total_cents, due_date) \
         VALUES ($1, $2, 'open', 'USD', $3, $4) \
         RETURNING id, customer_id, status, currency, total_cents, due_date, created_at",
    )
    .bind(auth.id)
    .bind(input.customer_id)
    .bind(total_cents)
    .bind(input.due_date)
    .fetch_one(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    let mut line_items = Vec::with_capacity(input.line_items.len());
    for (item, line_total) in input.line_items.iter().zip(line_totals) {
        let saved_item = sqlx::query_as::<_, LineItem>(
            "INSERT INTO invoice_line_items \
                (invoice_id, description, quantity, unit_amount_cents, line_total_cents) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING id, description, quantity, unit_amount_cents, line_total_cents",
        )
        .bind(invoice.id)
        .bind(item.description.trim())
        .bind(item.quantity)
        .bind(item.unit_amount_cents)
        .bind(line_total)
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::db)?;

        line_items.push(saved_item);
    }

    insert_event(
        &mut tx,
        auth.id,
        "invoice.created",
        json!({
            "invoice_id": invoice.id,
            "status": "open",
            "total_cents": total_cents,
            "currency": "USD"
        }),
    )
    .await?;

    tx.commit().await.map_err(ApiError::db)?;

    Ok((
        StatusCode::CREATED,
        Json(InvoiceDetail {
            invoice,
            line_items,
            payment_attempts: Vec::new(),
        }),
    ))
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

fn validate_line_items(input: &InvoiceInput) -> Result<Vec<i64>, ApiError> {
    if input.line_items.is_empty() || input.line_items.len() > 100 {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_line_items",
            "at least one line item is required",
        ));
    }

    input
        .line_items
        .iter()
        .map(|item| {
            if item.description.trim().is_empty()
                || item.description.len() > 500
                || item.quantity <= 0
                || item.unit_amount_cents < 0
            {
                return Err(ApiError::client(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_line_item",
                    "description, positive quantity, and non-negative unit amount are required",
                ));
            }

            item.quantity
                .checked_mul(item.unit_amount_cents)
                .ok_or_else(amount_overflow)
        })
        .collect()
}

fn amount_overflow() -> ApiError {
    ApiError::client(
        StatusCode::UNPROCESSABLE_ENTITY,
        "amount_overflow",
        "invoice amount is too large",
    )
}

fn invoice_not_found() -> ApiError {
    ApiError::client(
        StatusCode::NOT_FOUND,
        "invoice_not_found",
        "invoice not found",
    )
}

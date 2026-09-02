use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::{
    error::ApiError,
    models::{Invoice, InvoiceDetail, InvoiceInput, LineItem},
    services::webhooks::insert_event,
    state::AppState,
};

pub async fn create(
    state: &AppState,
    business_id: Uuid,
    input: InvoiceInput,
) -> Result<InvoiceDetail, ApiError> {
    let line_totals = validate_line_items(&input)?;
    let total_cents = line_totals.iter().try_fold(0i64, |total, value| {
        total.checked_add(*value).ok_or_else(amount_overflow)
    })?;

    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let customer_exists = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(input.customer_id)
    .bind(business_id)
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
    .bind(business_id)
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
        business_id,
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

    Ok(InvoiceDetail {
        invoice,
        line_items,
        payment_attempts: Vec::new(),
    })
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

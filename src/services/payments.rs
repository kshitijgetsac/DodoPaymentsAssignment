use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, Row};
use tokio::time::{sleep, timeout, Duration};
use uuid::Uuid;

use crate::{
    error::ApiError,
    models::{PayInput, PaymentResponse, PspReply},
    services::webhooks::insert_event,
    state::AppState,
};

#[derive(FromRow)]
struct IdempotencyRecord {
    request_hash: String,
    state: String,
    payment_attempt_id: Option<Uuid>,
    response_status: Option<i32>,
    response_body: Option<Value>,
}

#[derive(FromRow)]
struct PendingAttempt {
    id: Uuid,
    recovery_attempts: i32,
}

pub async fn pay_invoice(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    idempotency_key: &str,
    input: PayInput,
) -> Result<Response, ApiError> {
    let request_hash = hash_payment_request(invoice_id, &input)?;
    let mut tx = state.db.begin().await.map_err(ApiError::db)?;

    // Locking the invoice makes two payment requests for the same invoice wait
    // for one another instead of both creating a charge.
    let invoice_status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM invoices \
         WHERE id = $1 AND business_id = $2 FOR UPDATE",
    )
    .bind(invoice_id)
    .bind(business_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    let invoice_status = invoice_status.ok_or_else(|| {
        ApiError::client(
            StatusCode::NOT_FOUND,
            "invoice_not_found",
            "invoice not found",
        )
    })?;

    let existing = sqlx::query_as::<_, IdempotencyRecord>(
        "SELECT request_hash, state, payment_attempt_id, response_status, response_body \
         FROM idempotency_keys WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(idempotency_key)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    if let Some(record) = existing {
        if record.request_hash != request_hash {
            return Err(ApiError::client(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "key was used with a different request",
            ));
        }

        tx.rollback().await.map_err(ApiError::db)?;
        return Ok(response_from_idempotency_record(record));
    }

    if invoice_status == "paid" {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "invoice_not_payable",
            "invoice is already paid",
        ));
    }

    let pending_attempt: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM payment_attempts \
         WHERE invoice_id = $1 AND status = 'pending'",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    if pending_attempt.is_some() {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "payment_in_progress",
            "another payment is already processing",
        ));
    }

    let attempt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, status, card_token) \
         VALUES ($1, $2, 'pending', $3)",
    )
    .bind(attempt_id)
    .bind(invoice_id)
    .bind(&input.card_token)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    let insert_result = sqlx::query(
        "INSERT INTO idempotency_keys \
            (business_id, key, request_hash, state, payment_attempt_id) \
         VALUES ($1, $2, $3, 'processing', $4)",
    )
    .bind(business_id)
    .bind(idempotency_key)
    .bind(&request_hash)
    .bind(attempt_id)
    .execute(&mut *tx)
    .await;

    if let Err(error) = insert_result {
        if !is_unique_violation(&error) {
            return Err(ApiError::db(error));
        }

        // A business-wide key can race across two different invoice rows.
        // Roll back our attempt, then return the winner's result or a clear
        // conflict if the same key represented a different request.
        tx.rollback().await.map_err(ApiError::db)?;
        let record = find_idempotency_record(&state.db, business_id, idempotency_key)
            .await?
            .ok_or_else(|| {
                ApiError::Internal(anyhow::anyhow!(
                    "idempotency record missing after unique-key conflict"
                ))
            })?;

        if record.request_hash != request_hash {
            return Err(ApiError::client(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "key was used with a different request",
            ));
        }
        return Ok(response_from_idempotency_record(record));
    }

    tx.commit().await.map_err(ApiError::db)?;

    call_psp(state, attempt_id, &input).await
}

async fn call_psp(
    state: &AppState,
    attempt_id: Uuid,
    input: &PayInput,
) -> Result<Response, ApiError> {
    let request = state
        .http
        .post(format!("{}/charges", state.psp_url))
        .header("Idempotency-Key", attempt_id.to_string())
        .json(input)
        .send();

    let response = match timeout(Duration::from_secs(2), request).await {
        Ok(Ok(response)) if response.status().is_success() => response,
        _ => return Ok(processing_response(Some(attempt_id))),
    };

    let reply: PspReply = response
        .json()
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;

    match reply.status.as_str() {
        "succeeded" => finalize_payment(state, attempt_id, true, reply.psp_ref, None).await,
        "failed" => finalize_payment(state, attempt_id, false, reply.psp_ref, reply.code).await,
        _ => Ok(processing_response(Some(attempt_id))),
    }
}

async fn finalize_payment(
    state: &AppState,
    attempt_id: Uuid,
    succeeded: bool,
    psp_ref: Option<String>,
    failure_code: Option<String>,
) -> Result<Response, ApiError> {
    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let row = sqlx::query(
        "SELECT pa.status, pa.invoice_id, i.business_id, i.status AS invoice_status \
         FROM payment_attempts pa \
         JOIN invoices i ON i.id = pa.invoice_id \
         WHERE pa.id = $1 FOR UPDATE",
    )
    .bind(attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?
    .ok_or_else(|| {
        ApiError::client(
            StatusCode::NOT_FOUND,
            "payment_attempt_not_found",
            "payment attempt not found",
        )
    })?;

    let current_status: String = row.try_get("status").map_err(ApiError::db)?;
    let invoice_id: Uuid = row.try_get("invoice_id").map_err(ApiError::db)?;
    let business_id: Uuid = row.try_get("business_id").map_err(ApiError::db)?;
    let invoice_status: String = row.try_get("invoice_status").map_err(ApiError::db)?;

    if current_status != "pending" {
        let existing = sqlx::query_as::<_, IdempotencyRecord>(
            "SELECT request_hash, state, payment_attempt_id, response_status, response_body \
             FROM idempotency_keys WHERE payment_attempt_id = $1",
        )
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(ApiError::db)?;

        tx.rollback().await.map_err(ApiError::db)?;
        return Ok(existing
            .map(response_from_idempotency_record)
            .unwrap_or_else(|| processing_response(Some(attempt_id))));
    }

    if succeeded && invoice_status != "open" {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "invoice_not_payable",
            "invoice is no longer open",
        ));
    }

    let (payment_status, new_invoice_status, http_status) = if succeeded {
        ("succeeded", "paid", StatusCode::OK)
    } else {
        ("failed", "open", StatusCode::PAYMENT_REQUIRED)
    };

    sqlx::query(
        "UPDATE payment_attempts \
         SET status = $2, psp_ref = $3, failure_code = $4, updated_at = now() \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt_id)
    .bind(payment_status)
    .bind(&psp_ref)
    .bind(&failure_code)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    if succeeded {
        sqlx::query(
            "UPDATE invoices SET status = 'paid', updated_at = now() \
             WHERE id = $1 AND status = 'open'",
        )
        .bind(invoice_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::db)?;
    }

    let response_body = json!(PaymentResponse {
        invoice_id,
        payment_attempt_id: attempt_id,
        payment_status: payment_status.to_string(),
        invoice_status: new_invoice_status.to_string(),
        psp_ref: psp_ref.clone(),
        failure_code: failure_code.clone(),
    });

    sqlx::query(
        "UPDATE idempotency_keys \
         SET state = 'completed', response_status = $2, response_body = $3, updated_at = now() \
         WHERE payment_attempt_id = $1",
    )
    .bind(attempt_id)
    .bind(i32::from(http_status.as_u16()))
    .bind(&response_body)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::db)?;

    let event_type = if succeeded {
        "invoice.paid"
    } else {
        "invoice.payment_failed"
    };
    insert_event(&mut tx, business_id, event_type, response_body.clone()).await?;

    tx.commit().await.map_err(ApiError::db)?;
    Ok((http_status, Json(response_body)).into_response())
}

pub async fn recovery_worker(state: AppState) {
    loop {
        let attempts = claim_pending_attempts(&state).await;

        match attempts {
            Ok(attempts) => {
                for attempt in attempts {
                    recover_attempt(&state, attempt).await;
                }
            }
            Err(error) => tracing::warn!(error = ?error, "payment recovery query failed"),
        }

        sleep(Duration::from_secs(2)).await;
    }
}

async fn claim_pending_attempts(state: &AppState) -> Result<Vec<PendingAttempt>, sqlx::Error> {
    // Moving next_retry_at forward acts as a short lease. SKIP LOCKED keeps
    // multiple API replicas from claiming the same rows at the same time.
    sqlx::query_as::<_, PendingAttempt>(
        "WITH due AS ( \
             SELECT id FROM payment_attempts \
             WHERE status = 'pending' AND next_retry_at <= now() \
             ORDER BY next_retry_at \
             LIMIT 20 FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE payment_attempts pa \
         SET next_retry_at = now() + interval '60 seconds' \
         FROM due WHERE pa.id = due.id \
         RETURNING pa.id, pa.recovery_attempts",
    )
    .fetch_all(&state.db)
    .await
}

async fn recover_attempt(state: &AppState, attempt: PendingAttempt) {
    let request = state
        .http
        .get(format!("{}/charges/{}", state.psp_url, attempt.id))
        .send();

    match timeout(Duration::from_secs(2), request).await {
        Ok(Ok(response)) if response.status().is_success() => {
            match response.json::<PspReply>().await {
                Ok(reply) if reply.status == "succeeded" => {
                    let _ = finalize_payment(state, attempt.id, true, reply.psp_ref, None).await;
                }
                Ok(reply) if reply.status == "failed" => {
                    let _ =
                        finalize_payment(state, attempt.id, false, reply.psp_ref, reply.code).await;
                }
                _ => schedule_recovery(state, attempt, false).await,
            }
        }
        Ok(Ok(response)) if response.status() == StatusCode::NOT_FOUND => {
            schedule_recovery(state, attempt, true).await;
        }
        _ => schedule_recovery(state, attempt, true).await,
    }
}

async fn schedule_recovery(state: &AppState, attempt: PendingAttempt, count_failure: bool) {
    if !count_failure {
        let _ = sqlx::query(
            "UPDATE payment_attempts \
             SET next_retry_at = now() + (5 * interval '1 second'), updated_at = now() \
             WHERE id = $1 AND status = 'pending'",
        )
        .bind(attempt.id)
        .execute(&state.db)
        .await;
        return;
    }

    let next_attempt = attempt.recovery_attempts + 1;
    if next_attempt >= 4 {
        let _ = finalize_payment(
            state,
            attempt.id,
            false,
            None,
            Some("psp_unavailable".to_string()),
        )
        .await;
        return;
    }

    let delay_seconds = match next_attempt {
        1 => 5,
        2 => 20,
        _ => 60,
    };
    let _ = sqlx::query(
        "UPDATE payment_attempts \
         SET recovery_attempts = $2, \
             next_retry_at = now() + ($3 * interval '1 second'), \
             updated_at = now() \
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt.id)
    .bind(next_attempt)
    .bind(delay_seconds)
    .execute(&state.db)
    .await;
}

fn hash_payment_request(invoice_id: Uuid, input: &PayInput) -> Result<String, ApiError> {
    let mut hasher = Sha256::new();
    hasher.update(format!("POST:/invoices/{invoice_id}/pay:"));
    hasher.update(serde_json::to_vec(input).map_err(|error| ApiError::Internal(error.into()))?);
    Ok(hex::encode(hasher.finalize()))
}

async fn find_idempotency_record(
    db: &sqlx::PgPool,
    business_id: Uuid,
    key: &str,
) -> Result<Option<IdempotencyRecord>, ApiError> {
    sqlx::query_as::<_, IdempotencyRecord>(
        "SELECT request_hash, state, payment_attempt_id, response_status, response_body \
         FROM idempotency_keys WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key)
    .fetch_optional(db)
    .await
    .map_err(ApiError::db)
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("23505")
    )
}

fn response_from_idempotency_record(record: IdempotencyRecord) -> Response {
    if record.state != "completed" {
        return processing_response(record.payment_attempt_id);
    }

    let status = record.response_status.unwrap_or(500);
    let body = record
        .response_body
        .unwrap_or_else(|| json!({"error": {"code": "missing_response"}}));
    stored_response(status, body)
}

fn stored_response(status: i32, body: Value) -> Response {
    let status = StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(body)).into_response()
}

fn processing_response(attempt_id: Option<Uuid>) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "processing",
            "payment_attempt_id": attempt_id
        })),
    )
        .into_response()
}

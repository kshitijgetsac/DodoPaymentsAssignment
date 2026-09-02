use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::Context;
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use async_trait::async_trait;
use axum::{
    extract::{FromRequestParts, Path, Query, State},
    http::{request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{NaiveDate, Utc};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    FromRow, Postgres, Row, Transaction,
};
use std::{collections::HashMap, env, net::SocketAddr, sync::Arc};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{sleep, timeout, Duration as TokioDuration},
};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: PgPool,
    http: Client,
    psp_url: String,
    webhook_key: [u8; 32],
}

#[derive(Debug, Error)]
enum ApiError {
    #[error("{code}: {message}")]
    Client {
        status: StatusCode,
        code: &'static str,
        message: String,
    },
    #[error("database error")]
    Database(#[source] sqlx::Error),
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl ApiError {
    fn client(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self::Client {
            status,
            code,
            message: message.into(),
        }
    }
    fn db(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            Self::Client {
                status,
                code,
                message,
            } => (*status, *code, message.clone()),
            Self::Database(error) => {
                error!(error = ?error, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "database failure".to_string(),
                )
            }
            Self::Internal(error) => {
                error!(error = ?error, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "internal failure".to_string(),
                )
            }
        };
        (
            status,
            Json(json!({
                "error": {"code": code, "message": message, "request_id": Uuid::new_v4()}
            })),
        )
            .into_response()
    }
}

#[derive(Clone)]
struct BusinessAuth {
    id: Uuid,
}

#[async_trait]
impl FromRequestParts<AppState> for BusinessAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let value = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                ApiError::client(
                    StatusCode::UNAUTHORIZED,
                    "missing_api_key",
                    "Authorization: Bearer <key> is required",
                )
            })?;
        let raw = value.strip_prefix("Bearer ").ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "use a Bearer API key",
            )
        })?;
        let rest = raw.strip_prefix("dodo_").ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid key format",
            )
        })?;
        let (prefix, secret) = rest.split_once('_').ok_or_else(|| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid key format",
            )
        })?;
        let row = sqlx::query("SELECT business_id, secret_hash FROM api_keys WHERE prefix = $1 AND revoked_at IS NULL")
            .bind(prefix).fetch_optional(&state.db).await.map_err(ApiError::db)?
            .ok_or_else(|| ApiError::client(StatusCode::UNAUTHORIZED, "invalid_api_key", "invalid or revoked API key"))?;
        let business_id: Uuid = row.try_get("business_id").map_err(ApiError::db)?;
        let hash: String = row.try_get("secret_hash").map_err(ApiError::db)?;
        let parsed = PasswordHash::new(&hash).map_err(|_| {
            ApiError::client(
                StatusCode::UNAUTHORIZED,
                "invalid_api_key",
                "invalid API key",
            )
        })?;
        Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .map_err(|_| {
                ApiError::client(
                    StatusCode::UNAUTHORIZED,
                    "invalid_api_key",
                    "invalid or revoked API key",
                )
            })?;
        Ok(Self { id: business_id })
    }
}

#[derive(Serialize, FromRow)]
struct Customer {
    id: Uuid,
    name: String,
    email: String,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Deserialize)]
struct CustomerInput {
    name: String,
    email: String,
}

async fn create_customer(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<CustomerInput>,
) -> Result<(StatusCode, Json<Customer>), ApiError> {
    if input.name.trim().is_empty() || input.name.len() > 200 || !input.email.contains('@') {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_customer",
            "name and a valid email are required",
        ));
    }
    let customer = sqlx::query_as::<_, Customer>("INSERT INTO customers (business_id, name, email) VALUES ($1,$2,$3) RETURNING id,name,email,created_at")
        .bind(auth.id).bind(input.name.trim()).bind(input.email.trim()).fetch_one(&state.db).await.map_err(ApiError::db)?;
    Ok((StatusCode::CREATED, Json(customer)))
}

async fn get_customer(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<Customer>, ApiError> {
    let customer = sqlx::query_as::<_, Customer>(
        "SELECT id,name,email,created_at FROM customers WHERE id=$1 AND business_id=$2",
    )
    .bind(id)
    .bind(auth.id)
    .fetch_optional(&state.db)
    .await
    .map_err(ApiError::db)?
    .ok_or_else(|| {
        ApiError::client(
            StatusCode::NOT_FOUND,
            "customer_not_found",
            "customer not found",
        )
    })?;
    Ok(Json(customer))
}

async fn list_customers(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<Customer>>, ApiError> {
    let customers = sqlx::query_as::<_, Customer>("SELECT id,name,email,created_at FROM customers WHERE business_id=$1 ORDER BY created_at DESC")
        .bind(auth.id).fetch_all(&state.db).await.map_err(ApiError::db)?;
    Ok(Json(customers))
}

#[derive(Deserialize)]
struct LineItemInput {
    description: String,
    quantity: i64,
    unit_amount_cents: i64,
}

#[derive(Deserialize)]
struct InvoiceInput {
    customer_id: Uuid,
    due_date: NaiveDate,
    line_items: Vec<LineItemInput>,
}

#[derive(Serialize, FromRow, Clone)]
struct Invoice {
    id: Uuid,
    customer_id: Uuid,
    status: String,
    currency: String,
    total_cents: i64,
    due_date: NaiveDate,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Serialize, FromRow, Clone)]
struct LineItem {
    id: Uuid,
    description: String,
    quantity: i64,
    unit_amount_cents: i64,
    line_total_cents: i64,
}

#[derive(Serialize, FromRow, Clone)]
struct PaymentAttempt {
    id: Uuid,
    status: String,
    psp_ref: Option<String>,
    failure_code: Option<String>,
    created_at: chrono::DateTime<Utc>,
    updated_at: chrono::DateTime<Utc>,
}

#[derive(Serialize)]
struct InvoiceDetail {
    #[serde(flatten)]
    invoice: Invoice,
    line_items: Vec<LineItem>,
    payment_attempts: Vec<PaymentAttempt>,
}

async fn create_invoice(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<InvoiceInput>,
) -> Result<(StatusCode, Json<InvoiceDetail>), ApiError> {
    if input.line_items.is_empty() || input.line_items.len() > 100 {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_line_items",
            "at least one line item is required",
        ));
    }
    let mut total = 0i64;
    let mut line_totals = Vec::with_capacity(input.line_items.len());
    for item in &input.line_items {
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
        let line_total = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .ok_or_else(|| {
                ApiError::client(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "amount_overflow",
                    "line item amount is too large",
                )
            })?;
        total = total.checked_add(line_total).ok_or_else(|| {
            ApiError::client(
                StatusCode::UNPROCESSABLE_ENTITY,
                "amount_overflow",
                "invoice amount is too large",
            )
        })?;
        line_totals.push(line_total);
    }
    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let customer_exists =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM customers WHERE id=$1 AND business_id=$2")
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
    let invoice = sqlx::query_as::<_, Invoice>("INSERT INTO invoices (business_id,customer_id,status,currency,total_cents,due_date) VALUES ($1,$2,'open','USD',$3,$4) RETURNING id,customer_id,status,currency,total_cents,due_date,created_at")
        .bind(auth.id).bind(input.customer_id).bind(total).bind(input.due_date).fetch_one(&mut *tx).await.map_err(ApiError::db)?;
    let mut items = Vec::with_capacity(input.line_items.len());
    for (item, line_total) in input.line_items.iter().zip(line_totals) {
        let row = sqlx::query_as::<_, LineItem>("INSERT INTO invoice_line_items (invoice_id,description,quantity,unit_amount_cents,line_total_cents) VALUES ($1,$2,$3,$4,$5) RETURNING id,description,quantity,unit_amount_cents,line_total_cents")
            .bind(invoice.id).bind(item.description.trim()).bind(item.quantity).bind(item.unit_amount_cents).bind(line_total).fetch_one(&mut *tx).await.map_err(ApiError::db)?;
        items.push(row);
    }
    insert_event(&mut tx, auth.id, "invoice.created", json!({"invoice_id": invoice.id, "status": "open", "total_cents": total, "currency": "USD"})).await?;
    tx.commit().await.map_err(ApiError::db)?;
    Ok((
        StatusCode::CREATED,
        Json(InvoiceDetail {
            invoice,
            line_items: items,
            payment_attempts: vec![],
        }),
    ))
}

#[derive(Deserialize)]
struct InvoiceListQuery {
    status: Option<String>,
}

async fn get_invoice(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<InvoiceDetail>, ApiError> {
    let invoice = sqlx::query_as::<_, Invoice>("SELECT id,customer_id,status,currency,total_cents,due_date,created_at FROM invoices WHERE id=$1 AND business_id=$2")
        .bind(id).bind(auth.id).fetch_optional(&state.db).await.map_err(ApiError::db)?.ok_or_else(|| ApiError::client(StatusCode::NOT_FOUND, "invoice_not_found", "invoice not found"))?;
    let line_items = sqlx::query_as::<_, LineItem>("SELECT id,description,quantity,unit_amount_cents,line_total_cents FROM invoice_line_items WHERE invoice_id=$1 ORDER BY id").bind(id).fetch_all(&state.db).await.map_err(ApiError::db)?;
    let payment_attempts = sqlx::query_as::<_, PaymentAttempt>("SELECT id,status,psp_ref,failure_code,created_at,updated_at FROM payment_attempts WHERE invoice_id=$1 ORDER BY created_at DESC").bind(id).fetch_all(&state.db).await.map_err(ApiError::db)?;
    Ok(Json(InvoiceDetail {
        invoice,
        line_items,
        payment_attempts,
    }))
}

async fn list_invoices(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Query(query): Query<InvoiceListQuery>,
) -> Result<Json<Vec<Invoice>>, ApiError> {
    let invoices = match query.status.as_deref() {
        None => sqlx::query_as::<_, Invoice>("SELECT id,customer_id,status,currency,total_cents,due_date,created_at FROM invoices WHERE business_id=$1 ORDER BY created_at DESC").bind(auth.id).fetch_all(&state.db).await,
        Some(status @ ("open" | "paid")) => sqlx::query_as::<_, Invoice>("SELECT id,customer_id,status,currency,total_cents,due_date,created_at FROM invoices WHERE business_id=$1 AND status=$2 ORDER BY created_at DESC").bind(auth.id).bind(status).fetch_all(&state.db).await,
        Some(_) => return Err(ApiError::client(StatusCode::UNPROCESSABLE_ENTITY, "invalid_status", "status must be open or paid")),
    }.map_err(ApiError::db)?;
    Ok(Json(invoices))
}

#[derive(Deserialize, Serialize, Clone)]
struct PayInput {
    card_token: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct PaymentResponse {
    invoice_id: Uuid,
    payment_attempt_id: Uuid,
    payment_status: String,
    invoice_status: String,
    psp_ref: Option<String>,
    failure_code: Option<String>,
}

#[derive(FromRow)]
struct IdempotencyRow {
    request_hash: String,
    state: String,
    payment_attempt_id: Option<Uuid>,
    response_status: Option<i32>,
    response_body: Option<Value>,
}

fn stored_response(status: i32, body: Value) -> Response {
    let status = StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(body)).into_response()
}

async fn pay_invoice(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(invoice_id): Path<Uuid>,
    headers: HeaderMap,
    Json(input): Json<PayInput>,
) -> Result<Response, ApiError> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if key.is_empty() || key.len() > 200 {
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
    let mut hasher = Sha256::new();
    hasher.update(format!("POST:/invoices/{invoice_id}/pay:"));
    hasher.update(serde_json::to_vec(&input).map_err(|e| ApiError::Internal(e.into()))?);
    let request_hash = hex::encode(hasher.finalize());
    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let invoice_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM invoices WHERE id=$1 AND business_id=$2 FOR UPDATE")
            .bind(invoice_id)
            .bind(auth.id)
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
    if let Some(existing) = sqlx::query_as::<_, IdempotencyRow>("SELECT request_hash,state,payment_attempt_id,response_status,response_body FROM idempotency_keys WHERE business_id=$1 AND key=$2")
        .bind(auth.id).bind(&key).fetch_optional(&mut *tx).await.map_err(ApiError::db)? {
        if existing.request_hash != request_hash { return Err(ApiError::client(StatusCode::CONFLICT, "idempotency_key_reused", "key was used with a different request")); }
        tx.rollback().await.map_err(ApiError::db)?;
        if existing.state == "completed" { return Ok(stored_response(existing.response_status.unwrap_or(500), existing.response_body.unwrap_or_else(|| json!({"error":{"code":"missing_response"}})))); }
        return Ok((StatusCode::ACCEPTED, Json(json!({"status":"processing", "payment_attempt_id": existing.payment_attempt_id}))).into_response());
    }
    if invoice_status == "paid" {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "invoice_not_payable",
            "invoice is already paid",
        ));
    }
    let pending: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM payment_attempts WHERE invoice_id=$1 AND status='pending'",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::db)?;
    if pending.is_some() {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "payment_in_progress",
            "another payment is already processing",
        ));
    }
    let attempt_id = Uuid::new_v4();
    sqlx::query("INSERT INTO idempotency_keys (business_id,key,request_hash,state,payment_attempt_id) VALUES ($1,$2,$3,'processing',$4)").bind(auth.id).bind(&key).bind(&request_hash).bind(attempt_id).execute(&mut *tx).await.map_err(ApiError::db)?;
    sqlx::query("INSERT INTO payment_attempts (id,invoice_id,status,card_token) VALUES ($1,$2,'pending',$3)").bind(attempt_id).bind(invoice_id).bind(&input.card_token).execute(&mut *tx).await.map_err(ApiError::db)?;
    tx.commit().await.map_err(ApiError::db)?;

    let psp_result = timeout(
        TokioDuration::from_secs(2),
        state
            .http
            .post(format!("{}/charges", state.psp_url))
            .header("Idempotency-Key", attempt_id.to_string())
            .json(&input)
            .send(),
    )
    .await;
    match psp_result {
        Ok(Ok(response)) if response.status().is_success() => {
            let result: PspReply = response
                .json()
                .await
                .map_err(|e| ApiError::Internal(e.into()))?;
            match result.status.as_str() {
                "succeeded" => {
                    finalize_payment(&state, attempt_id, true, result.psp_ref, None).await
                }
                "failed" => {
                    finalize_payment(&state, attempt_id, false, result.psp_ref, result.code).await
                }
                _ => Ok((
                    StatusCode::ACCEPTED,
                    Json(json!({"status":"processing", "payment_attempt_id": attempt_id})),
                )
                    .into_response()),
            }
        }
        _ => Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status":"processing", "payment_attempt_id": attempt_id})),
        )
            .into_response()),
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct PspReply {
    status: String,
    psp_ref: Option<String>,
    code: Option<String>,
}

async fn finalize_payment(
    state: &AppState,
    attempt_id: Uuid,
    succeeded: bool,
    psp_ref: Option<String>,
    failure_code: Option<String>,
) -> Result<Response, ApiError> {
    let mut tx = state.db.begin().await.map_err(ApiError::db)?;
    let row = sqlx::query("SELECT pa.status,pa.invoice_id,i.business_id,i.status AS invoice_status FROM payment_attempts pa JOIN invoices i ON i.id=pa.invoice_id WHERE pa.id=$1 FOR UPDATE")
        .bind(attempt_id).fetch_optional(&mut *tx).await.map_err(ApiError::db)?.ok_or_else(|| ApiError::client(StatusCode::NOT_FOUND, "payment_attempt_not_found", "payment attempt not found"))?;
    let current: String = row.try_get("status").map_err(ApiError::db)?;
    let invoice_id: Uuid = row.try_get("invoice_id").map_err(ApiError::db)?;
    let business_id: Uuid = row.try_get("business_id").map_err(ApiError::db)?;
    let invoice_current: String = row.try_get("invoice_status").map_err(ApiError::db)?;
    if current != "pending" {
        let existing = sqlx::query_as::<_, IdempotencyRow>("SELECT request_hash,state,payment_attempt_id,response_status,response_body FROM idempotency_keys WHERE payment_attempt_id=$1").bind(attempt_id).fetch_optional(&mut *tx).await.map_err(ApiError::db)?;
        tx.rollback().await.map_err(ApiError::db)?;
        if let Some(existing) = existing.filter(|x| x.state == "completed") {
            return Ok(stored_response(
                existing.response_status.unwrap_or(500),
                existing.response_body.unwrap_or_else(|| json!({})),
            ));
        }
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({"status":"processing", "payment_attempt_id": attempt_id})),
        )
            .into_response());
    }
    if succeeded && invoice_current != "open" {
        return Err(ApiError::client(
            StatusCode::CONFLICT,
            "invoice_not_payable",
            "invoice is no longer open",
        ));
    }
    let (payment_status, invoice_status, http_status) = if succeeded {
        ("succeeded", "paid", StatusCode::OK)
    } else {
        ("failed", "open", StatusCode::PAYMENT_REQUIRED)
    };
    sqlx::query("UPDATE payment_attempts SET status=$2,psp_ref=$3,failure_code=$4,updated_at=now() WHERE id=$1 AND status='pending'").bind(attempt_id).bind(payment_status).bind(&psp_ref).bind(&failure_code).execute(&mut *tx).await.map_err(ApiError::db)?;
    if succeeded {
        sqlx::query(
            "UPDATE invoices SET status='paid',updated_at=now() WHERE id=$1 AND status='open'",
        )
        .bind(invoice_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::db)?;
    }
    let response = json!(PaymentResponse {
        invoice_id,
        payment_attempt_id: attempt_id,
        payment_status: payment_status.to_string(),
        invoice_status: invoice_status.to_string(),
        psp_ref: psp_ref.clone(),
        failure_code: failure_code.clone()
    });
    sqlx::query("UPDATE idempotency_keys SET state='completed',response_status=$2,response_body=$3,updated_at=now() WHERE payment_attempt_id=$1").bind(attempt_id).bind(http_status.as_u16() as i32).bind(&response).execute(&mut *tx).await.map_err(ApiError::db)?;
    insert_event(
        &mut tx,
        business_id,
        if succeeded {
            "invoice.paid"
        } else {
            "invoice.payment_failed"
        },
        response.clone(),
    )
    .await?;
    tx.commit().await.map_err(ApiError::db)?;
    Ok((http_status, Json(response)).into_response())
}

async fn insert_event(
    tx: &mut Transaction<'_, Postgres>,
    business_id: Uuid,
    event_type: &str,
    payload: Value,
) -> Result<(), ApiError> {
    let event_id: Uuid = sqlx::query_scalar("INSERT INTO webhook_events (business_id,event_type,payload) VALUES ($1,$2,$3) RETURNING id").bind(business_id).bind(event_type).bind(payload).fetch_one(&mut **tx).await.map_err(ApiError::db)?;
    sqlx::query("INSERT INTO webhook_deliveries (event_id,endpoint_id) SELECT $1,id FROM webhook_endpoints WHERE business_id=$2 AND active=true ON CONFLICT DO NOTHING").bind(event_id).bind(business_id).execute(&mut **tx).await.map_err(ApiError::db)?;
    Ok(())
}

#[derive(Deserialize)]
struct EndpointInput {
    url: String,
}

#[derive(Serialize)]
struct EndpointResponse {
    id: Uuid,
    url: String,
    secret: Option<String>,
    active: bool,
    created_at: chrono::DateTime<Utc>,
}

async fn create_endpoint(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<EndpointInput>,
) -> Result<(StatusCode, Json<EndpointResponse>), ApiError> {
    let url = Url::parse(&input.url).map_err(|_| {
        ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_webhook_url",
            "url must be valid http or https",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_webhook_url",
            "url must be valid http or https",
        ));
    }
    let mut secret_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut secret_bytes);
    let secret = format!("whsec_{}", URL_SAFE_NO_PAD.encode(secret_bytes));
    let encrypted = encrypt_secret(&secret, &state.webhook_key)?;
    let endpoint_url = input.url.clone();
    let row = sqlx::query("INSERT INTO webhook_endpoints (business_id,url,secret_ciphertext) VALUES ($1,$2,$3) RETURNING id,created_at")
        .bind(auth.id).bind(&endpoint_url).bind(encrypted).fetch_one(&state.db).await.map_err(ApiError::db)?;
    Ok((
        StatusCode::CREATED,
        Json(EndpointResponse {
            id: row.try_get("id").map_err(ApiError::db)?,
            url: endpoint_url,
            secret: Some(secret),
            active: true,
            created_at: row.try_get("created_at").map_err(ApiError::db)?,
        }),
    ))
}

async fn list_endpoints(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<EndpointResponse>>, ApiError> {
    let rows = sqlx::query("SELECT id,url,active,created_at FROM webhook_endpoints WHERE business_id=$1 ORDER BY created_at DESC").bind(auth.id).fetch_all(&state.db).await.map_err(ApiError::db)?;
    Ok(Json(
        rows.into_iter()
            .map(|row| EndpointResponse {
                id: row.try_get("id").unwrap_or_default(),
                url: row.try_get("url").unwrap_or_default(),
                secret: None,
                active: row.try_get("active").unwrap_or(false),
                created_at: row.try_get("created_at").unwrap_or_else(|_| Utc::now()),
            })
            .collect(),
    ))
}

#[derive(Serialize, FromRow)]
struct WebhookEventSummary {
    id: Uuid,
    event_type: String,
    created_at: chrono::DateTime<Utc>,
    delivery_count: i64,
    delivered_count: i64,
}

async fn list_events(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<WebhookEventSummary>>, ApiError> {
    let events = sqlx::query_as::<_, WebhookEventSummary>("SELECT e.id,e.event_type,e.created_at,COUNT(d.id)::BIGINT AS delivery_count,COUNT(d.id) FILTER (WHERE d.status='delivered')::BIGINT AS delivered_count FROM webhook_events e LEFT JOIN webhook_deliveries d ON d.event_id=e.id WHERE e.business_id=$1 GROUP BY e.id ORDER BY e.created_at DESC")
        .bind(auth.id).fetch_all(&state.db).await.map_err(ApiError::db)?;
    Ok(Json(events))
}

fn encrypt_secret(secret: &str, key: &[u8; 32]) -> Result<String, ApiError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), secret.as_bytes())
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    Ok(format!(
        "{}{}",
        hex::encode(nonce_bytes),
        hex::encode(ciphertext)
    ))
}

fn decrypt_secret(value: &str, key: &[u8; 32]) -> Result<String, ApiError> {
    let bytes = hex::decode(value).map_err(|e| ApiError::Internal(e.into()))?;
    if bytes.len() < 12 {
        return Err(ApiError::client(
            StatusCode::INTERNAL_SERVER_ERROR,
            "secret_error",
            "invalid encrypted secret",
        ));
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .map_err(|e| ApiError::Internal(anyhow::anyhow!(e.to_string())))?;
    String::from_utf8(plain).map_err(|e| ApiError::Internal(e.into()))
}

#[derive(FromRow)]
struct PendingAttempt {
    id: Uuid,
    recovery_attempts: i32,
}

async fn payment_recovery_worker(state: AppState) {
    loop {
        if let Ok(rows) = sqlx::query_as::<_, PendingAttempt>("SELECT id,recovery_attempts FROM payment_attempts WHERE status='pending' AND next_retry_at <= now() ORDER BY next_retry_at LIMIT 20").fetch_all(&state.db).await {
            for attempt in rows { recover_one(&state, attempt).await; }
        }
        sleep(TokioDuration::from_secs(2)).await;
    }
}

async fn recover_one(state: &AppState, attempt: PendingAttempt) {
    let response = timeout(
        TokioDuration::from_secs(2),
        state
            .http
            .get(format!("{}/charges/{}", state.psp_url, attempt.id))
            .send(),
    )
    .await;
    match response {
        Ok(Ok(resp)) if resp.status().is_success() => match resp.json::<PspReply>().await {
            Ok(reply) if reply.status == "succeeded" => {
                let _ = finalize_payment(state, attempt.id, true, reply.psp_ref, None).await;
            }
            Ok(reply) if reply.status == "failed" => {
                let _ = finalize_payment(state, attempt.id, false, reply.psp_ref, reply.code).await;
            }
            _ => schedule_recovery(state, attempt, false).await,
        },
        Ok(Ok(resp)) if resp.status() == StatusCode::NOT_FOUND => {
            schedule_recovery(state, attempt, true).await
        }
        _ => schedule_recovery(state, attempt, true).await,
    }
}

async fn schedule_recovery(state: &AppState, attempt: PendingAttempt, count_failure: bool) {
    let next = attempt.recovery_attempts + i32::from(count_failure);
    if next >= 4 {
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
    let delay = match next {
        1 => 5,
        2 => 20,
        _ => 60,
    };
    let _ = sqlx::query("UPDATE payment_attempts SET recovery_attempts=$2,next_retry_at=now()+($3 * interval '1 second'),updated_at=now() WHERE id=$1 AND status='pending'")
        .bind(attempt.id).bind(next).bind(delay).execute(&state.db).await;
}

#[derive(FromRow)]
struct DeliveryJob {
    id: Uuid,
    event_id: Uuid,
    event_type: String,
    payload: Value,
    url: String,
    secret_ciphertext: String,
    attempts: i32,
}

async fn webhook_worker(state: AppState) {
    loop {
        match claim_delivery(&state.db).await {
            Ok(Some(job)) => deliver_one(&state, job).await,
            Ok(None) => sleep(TokioDuration::from_millis(500)).await,
            Err(error) => {
                warn!(error = ?error, "webhook worker query failed");
                sleep(TokioDuration::from_secs(2)).await;
            }
        }
    }
}

async fn claim_delivery(db: &PgPool) -> Result<Option<DeliveryJob>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let row = sqlx::query_as::<_, DeliveryJob>("SELECT d.id,d.event_id,e.event_type,e.payload,ep.url,ep.secret_ciphertext,d.attempts FROM webhook_deliveries d JOIN webhook_events e ON e.id=d.event_id JOIN webhook_endpoints ep ON ep.id=d.endpoint_id WHERE d.status='pending' AND d.next_attempt_at <= now() AND (d.locked_at IS NULL OR d.locked_at < now()-interval '60 seconds') ORDER BY d.next_attempt_at LIMIT 1 FOR UPDATE OF d SKIP LOCKED")
        .fetch_optional(&mut *tx).await?;
    if let Some(ref job) = row {
        sqlx::query("UPDATE webhook_deliveries SET locked_at=now() WHERE id=$1")
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(row)
}

async fn deliver_one(state: &AppState, job: DeliveryJob) {
    let secret = match decrypt_secret(&job.secret_ciphertext, &state.webhook_key) {
        Ok(value) => value,
        Err(error) => {
            mark_delivery_failed(&state.db, job.id, job.attempts, format!("secret: {error}")).await;
            return;
        }
    };
    let body = match serde_json::to_vec(
        &json!({"id":job.event_id,"type":job.event_type,"data":job.payload}),
    ) {
        Ok(body) => body,
        Err(error) => {
            mark_delivery_failed(&state.db, job.id, job.attempts, error.to_string()).await;
            return;
        }
    };
    let timestamp = Utc::now().timestamp().to_string();
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(
        format!(
            "{}.{}.{}",
            timestamp,
            job.event_id,
            String::from_utf8_lossy(&body)
        )
        .as_bytes(),
    );
    let signature = format!("v1={}", hex::encode(mac.finalize().into_bytes()));
    let result = timeout(
        TokioDuration::from_secs(5),
        state
            .http
            .post(&job.url)
            .header("content-type", "application/json")
            .header("X-Dodo-Event-Id", job.event_id.to_string())
            .header("X-Dodo-Timestamp", &timestamp)
            .header("X-Dodo-Signature", signature)
            .body(body)
            .send(),
    )
    .await;
    match result {
        Ok(Ok(response)) if response.status().is_success() => {
            let _ = sqlx::query("UPDATE webhook_deliveries SET status='delivered',attempts=attempts+1,delivered_at=now(),locked_at=NULL WHERE id=$1").bind(job.id).execute(&state.db).await;
        }
        Ok(Ok(response)) => {
            mark_delivery_failed(
                &state.db,
                job.id,
                job.attempts,
                format!("receiver returned {}", response.status()),
            )
            .await
        }
        Ok(Err(error)) => {
            mark_delivery_failed(&state.db, job.id, job.attempts, error.to_string()).await
        }
        Err(_) => {
            mark_delivery_failed(
                &state.db,
                job.id,
                job.attempts,
                "delivery timeout".to_string(),
            )
            .await
        }
    }
}

async fn mark_delivery_failed(db: &PgPool, id: Uuid, attempts: i32, error: String) {
    let next_attempt = attempts + 1;
    let delays = [1i64, 5, 30, 120, 600, 1800];
    let exhausted = next_attempt >= delays.len() as i32;
    let delay = delays[(next_attempt.min(delays.len() as i32) - 1) as usize];
    let _ = if exhausted {
        sqlx::query("UPDATE webhook_deliveries SET status='exhausted',attempts=$2,last_error=$3,locked_at=NULL WHERE id=$1").bind(id).bind(next_attempt).bind(error).execute(db).await
    } else {
        sqlx::query("UPDATE webhook_deliveries SET status='pending',attempts=$2,last_error=$3,next_attempt_at=now()+make_interval(secs => $4),locked_at=NULL WHERE id=$1").bind(id).bind(next_attempt).bind(error).bind(delay).execute(db).await
    };
}

fn routes(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/customers", post(create_customer).get(list_customers))
        .route("/customers/:id", get(get_customer))
        .route("/invoices", post(create_invoice).get(list_invoices))
        .route("/invoices/:id", get(get_invoice))
        .route("/invoices/:id/pay", post(pay_invoice))
        .route(
            "/webhook-endpoints",
            post(create_endpoint).get(list_endpoints),
        )
        .route("/webhook-events", get(list_events))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn api_main() -> anyhow::Result<()> {
    let database_url = env::var("DATABASE_URL").context("DATABASE_URL is required")?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    let dev_key =
        env::var("DEV_API_KEY").unwrap_or_else(|_| "dodo_test_dev_secret_change_me".to_string());
    ensure_seed_business(&db, &dev_key).await?;
    let mut key = [0u8; 32];
    let digest = Sha256::digest(
        env::var("WEBHOOK_MASTER_KEY")
            .unwrap_or_else(|_| "local-development-key".into())
            .as_bytes(),
    );
    key.copy_from_slice(&digest);
    let state = AppState {
        db: db.clone(),
        http: Client::builder().build()?,
        psp_url: env::var("PSP_URL").unwrap_or_else(|_| "http://localhost:8081".into()),
        webhook_key: key,
    };
    tokio::spawn(payment_recovery_worker(state.clone()));
    tokio::spawn(webhook_worker(state.clone()));
    let addr: SocketAddr = "0.0.0.0:8080".parse()?;
    info!(%addr, "invoice API listening");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, routes(state)).await?;
    Ok(())
}

async fn ensure_seed_business(db: &PgPool, key: &str) -> anyhow::Result<()> {
    let (prefix, secret) = key
        .strip_prefix("dodo_")
        .and_then(|v| v.split_once('_'))
        .context("DEV_API_KEY must look like dodo_<prefix>_<secret>")?;
    let business_id: Uuid =
        match sqlx::query_scalar("SELECT id FROM businesses WHERE name='Demo Business' LIMIT 1")
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
    let hash = Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .to_string();
    sqlx::query("INSERT INTO api_keys (business_id,prefix,secret_hash) VALUES ($1,$2,$3) ON CONFLICT (prefix) DO UPDATE SET business_id=EXCLUDED.business_id,secret_hash=EXCLUDED.secret_hash,revoked_at=NULL")
        .bind(business_id).bind(prefix).bind(hash).execute(db).await?;
    info!(business_id = %business_id, "seed API key ready");
    Ok(())
}

#[derive(Clone)]
struct PspState {
    charges: Arc<Mutex<HashMap<String, PspReply>>>,
}

async fn psp_charge(
    State(state): State<PspState>,
    headers: HeaderMap,
    Json(input): Json<PayInput>,
) -> Result<Json<PspReply>, Response> {
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Idempotency-Key required"})),
        )
            .into_response());
    }
    if input.card_token == "tok_network_error" {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"network_error"})),
        )
            .into_response());
    }
    if let Some(existing) = state.charges.lock().await.get(&key).cloned() {
        return Ok(Json(existing));
    }
    let (delay, final_reply) = match input.card_token.as_str() {
        "tok_success" => (
            100,
            PspReply {
                status: "succeeded".into(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            },
        ),
        "tok_insufficient_funds" => (
            100,
            PspReply {
                status: "failed".into(),
                psp_ref: None,
                code: Some("insufficient_funds".into()),
            },
        ),
        "tok_card_declined" => (
            100,
            PspReply {
                status: "failed".into(),
                psp_ref: None,
                code: Some("card_declined".into()),
            },
        ),
        "tok_timeout" => (
            30_000,
            PspReply {
                status: "succeeded".into(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            },
        ),
        _ => (
            100,
            PspReply {
                status: "failed".into(),
                psp_ref: None,
                code: Some("unknown_token".into()),
            },
        ),
    };
    state.charges.lock().await.insert(
        key.clone(),
        PspReply {
            status: "processing".into(),
            psp_ref: final_reply.psp_ref.clone(),
            code: final_reply.code.clone(),
        },
    );
    let charges = state.charges.clone();
    let completion_key = key.clone();
    tokio::spawn(async move {
        sleep(TokioDuration::from_millis(delay)).await;
        charges.lock().await.insert(completion_key, final_reply);
    });
    sleep(TokioDuration::from_millis(delay)).await;
    let reply = state
        .charges
        .lock()
        .await
        .get(&key)
        .cloned()
        .unwrap_or(PspReply {
            status: "processing".into(),
            psp_ref: None,
            code: None,
        });
    Ok(Json(reply))
}

async fn psp_status(
    State(state): State<PspState>,
    Path(key): Path<String>,
) -> Result<Json<PspReply>, Response> {
    state
        .charges
        .lock()
        .await
        .get(&key)
        .cloned()
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error":"charge not found"})),
            )
                .into_response()
        })
}

async fn psp_main() -> anyhow::Result<()> {
    let state = PspState {
        charges: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/charges", post(psp_charge))
        .route("/charges/:key", get(psp_status))
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
    info!("mock PSP listening on 8081");
    axum::serve(listener, app).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    match env::args().nth(1).as_deref() {
        Some("psp") => psp_main().await,
        _ => api_main().await,
    }
}

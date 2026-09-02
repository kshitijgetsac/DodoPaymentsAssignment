use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use sha2::Sha256;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use tokio::time::{sleep, timeout, Duration};
use tracing::warn;
use uuid::Uuid;

use crate::{error::ApiError, state::AppState};

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

pub async fn insert_event(
    tx: &mut Transaction<'_, Postgres>,
    business_id: Uuid,
    event_type: &str,
    payload: Value,
) -> Result<(), ApiError> {
    let event_id: Uuid = sqlx::query_scalar(
        "INSERT INTO webhook_events (business_id, event_type, payload) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(business_id)
    .bind(event_type)
    .bind(payload)
    .fetch_one(&mut **tx)
    .await
    .map_err(ApiError::db)?;

    sqlx::query(
        "INSERT INTO webhook_deliveries (event_id, endpoint_id) \
         SELECT $1, id FROM webhook_endpoints \
         WHERE business_id = $2 AND active = true \
         ON CONFLICT DO NOTHING",
    )
    .bind(event_id)
    .bind(business_id)
    .execute(&mut **tx)
    .await
    .map_err(ApiError::db)?;

    Ok(())
}

pub fn encrypt_secret(secret: &str, key: &[u8; 32]) -> Result<String, ApiError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|error| ApiError::Internal(anyhow::anyhow!(error.to_string())))?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), secret.as_bytes())
        .map_err(|error| ApiError::Internal(anyhow::anyhow!(error.to_string())))?;

    Ok(format!("{}{}", hex::encode(nonce), hex::encode(ciphertext)))
}

fn decrypt_secret(value: &str, key: &[u8; 32]) -> Result<String, ApiError> {
    let bytes = hex::decode(value).map_err(|error| ApiError::Internal(error.into()))?;
    if bytes.len() < 12 {
        return Err(ApiError::client(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "secret_error",
            "invalid encrypted secret",
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|error| ApiError::Internal(anyhow::anyhow!(error.to_string())))?;
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .map_err(|error| ApiError::Internal(anyhow::anyhow!(error.to_string())))?;

    String::from_utf8(plaintext).map_err(|error| ApiError::Internal(error.into()))
}

pub async fn delivery_worker(state: AppState) {
    loop {
        match claim_delivery(&state.db).await {
            Ok(Some(job)) => deliver(&state, job).await,
            Ok(None) => sleep(Duration::from_millis(500)).await,
            Err(error) => {
                warn!(error = ?error, "webhook worker query failed");
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn claim_delivery(db: &PgPool) -> Result<Option<DeliveryJob>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let job = sqlx::query_as::<_, DeliveryJob>(
        "SELECT d.id, d.event_id, e.event_type, e.payload, \
                ep.url, ep.secret_ciphertext, d.attempts \
         FROM webhook_deliveries d \
         JOIN webhook_events e ON e.id = d.event_id \
         JOIN webhook_endpoints ep ON ep.id = d.endpoint_id \
         WHERE d.status = 'pending' \
           AND d.next_attempt_at <= now() \
           AND (d.locked_at IS NULL OR d.locked_at < now() - interval '60 seconds') \
         ORDER BY d.next_attempt_at \
         LIMIT 1 FOR UPDATE OF d SKIP LOCKED",
    )
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(job) = &job {
        sqlx::query("UPDATE webhook_deliveries SET locked_at = now() WHERE id = $1")
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(job)
}

async fn deliver(state: &AppState, job: DeliveryJob) {
    let secret = match decrypt_secret(&job.secret_ciphertext, &state.webhook_key) {
        Ok(secret) => secret,
        Err(error) => {
            record_failure(&state.db, &job, format!("secret: {error}")).await;
            return;
        }
    };

    let body = match serde_json::to_vec(&json!({
        "id": job.event_id,
        "type": job.event_type,
        "data": job.payload
    })) {
        Ok(body) => body,
        Err(error) => {
            record_failure(&state.db, &job, error.to_string()).await;
            return;
        }
    };

    let timestamp = Utc::now().timestamp().to_string();
    let signature = sign_payload(&secret, &timestamp, job.event_id, &body);
    let request = state
        .http
        .post(&job.url)
        .header("content-type", "application/json")
        .header("X-Dodo-Event-Id", job.event_id.to_string())
        .header("X-Dodo-Timestamp", &timestamp)
        .header("X-Dodo-Signature", signature)
        .body(body)
        .send();

    match timeout(Duration::from_secs(5), request).await {
        Ok(Ok(response)) if response.status().is_success() => {
            let _ = sqlx::query(
                "UPDATE webhook_deliveries \
                 SET status = 'delivered', attempts = attempts + 1, \
                     delivered_at = now(), locked_at = NULL \
                 WHERE id = $1",
            )
            .bind(job.id)
            .execute(&state.db)
            .await;
        }
        Ok(Ok(response)) => {
            record_failure(
                &state.db,
                &job,
                format!("receiver returned {}", response.status()),
            )
            .await;
        }
        Ok(Err(error)) => record_failure(&state.db, &job, error.to_string()).await,
        Err(_) => record_failure(&state.db, &job, "delivery timeout".to_string()).await,
    }
}

fn sign_payload(secret: &str, timestamp: &str, event_id: Uuid, body: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
    mac.update(
        format!(
            "{}.{}.{}",
            timestamp,
            event_id,
            String::from_utf8_lossy(body)
        )
        .as_bytes(),
    );
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

async fn record_failure(db: &PgPool, job: &DeliveryJob, error: String) {
    let next_attempt = job.attempts + 1;
    let retry_delays = [1i64, 5, 30, 120, 600, 1800];
    let exhausted = next_attempt >= retry_delays.len() as i32;
    let delay_index = (next_attempt.min(retry_delays.len() as i32) - 1) as usize;
    let delay_seconds = retry_delays[delay_index];

    if exhausted {
        let _ = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status = 'exhausted', attempts = $2, last_error = $3, locked_at = NULL \
             WHERE id = $1",
        )
        .bind(job.id)
        .bind(next_attempt)
        .bind(error)
        .execute(db)
        .await;
    } else {
        let _ = sqlx::query(
            "UPDATE webhook_deliveries \
             SET status = 'pending', attempts = $2, last_error = $3, \
                 next_attempt_at = now() + make_interval(secs => $4), locked_at = NULL \
             WHERE id = $1",
        )
        .bind(job.id)
        .bind(next_attempt)
        .bind(error)
        .bind(delay_seconds)
        .execute(db)
        .await;
    }
}

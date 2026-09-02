use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use tokio::{sync::Mutex, time::sleep};
use tower_http::trace::TraceLayer;
use tracing::info;
use uuid::Uuid;

use crate::models::{PayInput, PspReply};

#[derive(Clone)]
struct PspState {
    charges: Arc<Mutex<HashMap<String, PspReply>>>,
    create_calls: Arc<Mutex<HashMap<String, u64>>>,
}

pub async fn run() -> anyhow::Result<()> {
    let state = PspState {
        charges: Arc::new(Mutex::new(HashMap::new())),
        create_calls: Arc::new(Mutex::new(HashMap::new())),
    };
    let app = Router::new()
        .route("/charges", post(create_charge))
        .route("/charges/:key", get(get_charge))
        .route("/_test/charges/:key/call-count", get(get_call_count))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8081").await?;
    info!("mock PSP listening on 8081");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn create_charge(
    State(state): State<PspState>,
    headers: HeaderMap,
    Json(input): Json<PayInput>,
) -> Result<Json<PspReply>, Response> {
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();

    if idempotency_key.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Idempotency-Key required"})),
        )
            .into_response());
    }

    let mut create_calls = state.create_calls.lock().await;
    *create_calls.entry(idempotency_key.clone()).or_default() += 1;
    drop(create_calls);

    if input.card_token == "tok_network_error" {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "network_error"})),
        )
            .into_response());
    }

    if let Some(existing) = state.charges.lock().await.get(&idempotency_key).cloned() {
        return Ok(Json(existing));
    }

    let (delay, final_reply) = reply_for_token(&input.card_token);
    state.charges.lock().await.insert(
        idempotency_key.clone(),
        PspReply {
            status: "processing".to_string(),
            psp_ref: final_reply.psp_ref.clone(),
            code: final_reply.code.clone(),
        },
    );

    let charges = state.charges.clone();
    let completion_key = idempotency_key.clone();
    let completed_reply = final_reply.clone();
    tokio::spawn(async move {
        sleep(delay).await;
        charges.lock().await.insert(completion_key, completed_reply);
    });

    // This sleep intentionally makes tok_timeout exceed the API's timeout. The
    // background task above still records the eventual result for reconciliation.
    sleep(delay).await;
    let reply = state
        .charges
        .lock()
        .await
        .get(&idempotency_key)
        .cloned()
        .unwrap_or_else(processing_reply);

    Ok(Json(reply))
}

async fn get_call_count(
    State(state): State<PspState>,
    Path(key): Path<String>,
) -> Json<serde_json::Value> {
    let calls = state
        .create_calls
        .lock()
        .await
        .get(&key)
        .copied()
        .unwrap_or_default();
    Json(json!({"calls": calls}))
}

async fn get_charge(
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
                Json(json!({"error": "charge not found"})),
            )
                .into_response()
        })
}

fn reply_for_token(card_token: &str) -> (std::time::Duration, PspReply) {
    let quick = std::time::Duration::from_millis(100);
    match card_token {
        "tok_success" => (
            quick,
            PspReply {
                status: "succeeded".to_string(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            },
        ),
        "tok_insufficient_funds" => (
            quick,
            PspReply {
                status: "failed".to_string(),
                psp_ref: None,
                code: Some("insufficient_funds".to_string()),
            },
        ),
        "tok_card_declined" => (
            quick,
            PspReply {
                status: "failed".to_string(),
                psp_ref: None,
                code: Some("card_declined".to_string()),
            },
        ),
        "tok_timeout" => (
            std::time::Duration::from_secs(30),
            PspReply {
                status: "succeeded".to_string(),
                psp_ref: Some(Uuid::new_v4().to_string()),
                code: None,
            },
        ),
        _ => (
            quick,
            PspReply {
                status: "failed".to_string(),
                psp_ref: None,
                code: Some("unknown_token".to_string()),
            },
        ),
    }
}

fn processing_reply() -> PspReply {
    PspReply {
        status: "processing".to_string(),
        psp_ref: None,
        code: None,
    }
}

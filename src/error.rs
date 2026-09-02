use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;
use tracing::error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ApiError {
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
    pub fn client(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self::Client {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn db(error: sqlx::Error) -> Self {
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
                "error": {
                    "code": code,
                    "message": message,
                    "request_id": Uuid::new_v4()
                }
            })),
        )
            .into_response()
    }
}

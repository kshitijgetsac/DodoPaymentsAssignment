use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;

use crate::{
    auth::BusinessAuth,
    error::ApiError,
    models::{Customer, CustomerInput},
    state::AppState,
};

pub async fn create(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Json(input): Json<CustomerInput>,
) -> Result<(StatusCode, Json<Customer>), ApiError> {
    let name = input.name.trim();
    let email = input.email.trim();

    if name.is_empty() || name.len() > 200 || !email.contains('@') {
        return Err(ApiError::client(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_customer",
            "name and a valid email are required",
        ));
    }

    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (business_id, name, email) \
         VALUES ($1, $2, $3) \
         RETURNING id, name, email, created_at",
    )
    .bind(auth.id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await
    .map_err(ApiError::db)?;

    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get(
    State(state): State<AppState>,
    auth: BusinessAuth,
    Path(id): Path<Uuid>,
) -> Result<Json<Customer>, ApiError> {
    let customer = sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at \
         FROM customers WHERE id = $1 AND business_id = $2",
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

pub async fn list(
    State(state): State<AppState>,
    auth: BusinessAuth,
) -> Result<Json<Vec<Customer>>, ApiError> {
    let customers = sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at \
         FROM customers WHERE business_id = $1 ORDER BY created_at DESC",
    )
    .bind(auth.id)
    .fetch_all(&state.db)
    .await
    .map_err(ApiError::db)?;

    Ok(Json(customers))
}

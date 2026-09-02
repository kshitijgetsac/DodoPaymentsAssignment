use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Serialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CustomerInput {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceInput {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemInput>,
}

#[derive(Debug, Serialize, FromRow, Clone)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    pub status: String,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow, Clone)]
pub struct LineItem {
    pub id: Uuid,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub line_total_cents: i64,
}

#[derive(Debug, Serialize, FromRow, Clone)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub status: String,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct InvoiceDetail {
    #[serde(flatten)]
    pub invoice: Invoice,
    pub line_items: Vec<LineItem>,
    pub payment_attempts: Vec<PaymentAttempt>,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceListQuery {
    pub status: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PayInput {
    pub card_token: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentResponse {
    pub invoice_id: Uuid,
    pub payment_attempt_id: Uuid,
    pub payment_status: String,
    pub invoice_status: String,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PspReply {
    pub status: String,
    pub psp_ref: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EndpointInput {
    pub url: String,
}

#[derive(Debug, Serialize)]
pub struct EndpointResponse {
    pub id: Uuid,
    pub url: String,
    pub secret: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
pub struct WebhookEventSummary {
    pub id: Uuid,
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub delivery_count: i64,
    pub delivered_count: i64,
}

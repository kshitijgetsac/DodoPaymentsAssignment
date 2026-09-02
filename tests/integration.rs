use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::env;
use tokio::{
    task::JoinSet,
    time::{sleep, Duration},
};
use uuid::Uuid;

fn config() -> (Client, String, String) {
    (
        Client::new(),
        env::var("TEST_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
        env::var("DODO_API_KEY").unwrap_or_else(|_| "dodo_test_dev_secret_change_me".into()),
    )
}

async fn fixture(client: &Client, base: &str, key: &str) -> (Uuid, Uuid) {
    let customer: Value = client.post(format!("{base}/customers")).bearer_auth(key).json(&json!({"name":"Integration Customer","email":format!("{}@example.com", Uuid::new_v4())})).send().await.unwrap().json().await.unwrap();
    let customer_id = Uuid::parse_str(customer["id"].as_str().unwrap()).unwrap();
    let invoice: Value = client.post(format!("{base}/invoices")).bearer_auth(key).json(&json!({"customer_id":customer_id,"due_date":"2030-01-31","line_items":[{"description":"Test","quantity":1,"unit_amount_cents":100}]})).send().await.unwrap().json().await.unwrap();
    (
        customer_id,
        Uuid::parse_str(invoice["id"].as_str().unwrap()).unwrap(),
    )
}

#[tokio::test]
#[ignore = "requires docker compose up"]
async fn concurrent_payment_has_one_attempt() {
    let (client, base, key) = config();
    let (_, invoice_id) = fixture(&client, &base, &key).await;
    let mut set = JoinSet::new();
    for _ in 0..8 {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        set.spawn(async move {
            client
                .post(format!("{base}/invoices/{invoice_id}/pay"))
                .bearer_auth(key)
                .header("Idempotency-Key", Uuid::new_v4().to_string())
                .json(&json!({"card_token":"tok_success"}))
                .send()
                .await
                .unwrap()
                .status()
        });
    }
    let mut successes = 0;
    while let Some(result) = set.join_next().await {
        if result.unwrap() == StatusCode::OK {
            successes += 1;
        }
    }
    assert!(successes <= 1);
    sleep(Duration::from_millis(500)).await;
    let invoice: Value = client
        .get(format!("{base}/invoices/{invoice_id}"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(invoice["status"], "paid");
    assert_eq!(invoice["payment_attempts"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires docker compose up"]
async fn idempotent_retry_replays_without_second_attempt() {
    let (client, base, key) = config();
    let (_, invoice_id) = fixture(&client, &base, &key).await;
    let idem = format!("idem-{}", Uuid::new_v4());
    let first = client
        .post(format!("{base}/invoices/{invoice_id}/pay"))
        .bearer_auth(&key)
        .header("Idempotency-Key", &idem)
        .json(&json!({"card_token":"tok_success"}))
        .send()
        .await
        .unwrap();
    let first_status = first.status();
    let first_body = first.text().await.unwrap();
    let second = client
        .post(format!("{base}/invoices/{invoice_id}/pay"))
        .bearer_auth(&key)
        .header("Idempotency-Key", &idem)
        .json(&json!({"card_token":"tok_success"}))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), first_status);
    assert_eq!(second.text().await.unwrap(), first_body);
}

#[tokio::test]
#[ignore = "requires docker compose up; takes about 30 seconds"]
async fn timeout_is_accepted_then_reconciled() {
    let (client, base, key) = config();
    let (_, invoice_id) = fixture(&client, &base, &key).await;
    let response = client
        .post(format!("{base}/invoices/{invoice_id}/pay"))
        .bearer_auth(&key)
        .header("Idempotency-Key", Uuid::new_v4().to_string())
        .json(&json!({"card_token":"tok_timeout"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    for _ in 0..25 {
        sleep(Duration::from_secs(2)).await;
        let invoice: Value = client
            .get(format!("{base}/invoices/{invoice_id}"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if invoice["status"] == "paid" {
            return;
        }
    }
    panic!("timeout payment was not reconciled");
}

# Invoice & Payment Service

A small Rust/Axum billing backend for the Dodo Payments backend assignment. It
uses PostgreSQL, a durable webhook outbox, and a separate mock PSP process.

## Run it

Requirements: Docker Desktop.

```bash
docker compose up --build
```

The API listens on `http://localhost:8080`. The mock PSP listens on port 8081.
The Compose seed business uses this development key:

```text
dodo_test_dev_secret_change_me
```

Use it as `Authorization: Bearer <key>`. All money values are integer cents.

## Curl walkthrough

Create a customer:

```bash
API_KEY=dodo_test_dev_secret_change_me
CUSTOMER_ID=$(curl -sS -X POST http://localhost:8080/customers \
  -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  -d '{"name":"Priya Shah","email":"priya@example.com"}' | jq -r .id)
```

Create an invoice. The server calculates the total (`102000` cents):

```bash
INVOICE_ID=$(curl -sS -X POST http://localhost:8080/invoices \
  -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  -d "{\"customer_id\":\"$CUSTOMER_ID\",\"due_date\":\"2030-01-31\",\"line_items\":[{\"description\":\"Website work\",\"quantity\":2,\"unit_amount_cents\":50000},{\"description\":\"Hosting\",\"quantity\":1,\"unit_amount_cents\":2000}]}" | jq -r .id)
```

Successful payment (`tok_success`):

```bash
curl -i -X POST "http://localhost:8080/invoices/$INVOICE_ID/pay" \
  -H "Authorization: Bearer $API_KEY" -H 'Idempotency-Key: demo-success-1' \
  -H 'content-type: application/json' -d '{"card_token":"tok_success"}'
```

Declined payment (`tok_card_declined`) leaves the invoice open and returns
HTTP 402:

```bash
curl -i -X POST "http://localhost:8080/invoices/$INVOICE_ID/pay" \
  -H "Authorization: Bearer $API_KEY" -H 'Idempotency-Key: demo-decline-1' \
  -H 'content-type: application/json' -d '{"card_token":"tok_card_declined"}'
```

For a timeout demo, use `tok_timeout`. The API returns `202` quickly, and the
background worker reconciles the PSP result after about 30 seconds. Inspect
the eventual state with `GET /invoices/{id}`.

## Tests

The focused integration tests run against a live Compose stack and are marked
ignored for normal offline `cargo test` runs:

```bash
TEST_BASE_URL=http://localhost:8080 \
DODO_API_KEY=dodo_test_dev_secret_change_me \
cargo test --test integration -- --ignored --nocapture
```

They cover concurrent payment attempts, idempotent replay without another PSP
call, and PSP timeout reconciliation.

## Documentation

- [DESIGN.md](DESIGN.md) explains the data model, state machine, idempotency,
  failure modes, and webhook guarantees.
- [OPENAPI.yaml](OPENAPI.yaml) documents the HTTP API and error shape.
- [AI_USAGE.md](AI_USAGE.md) discloses how AI was used and what I decided.

## Demo Video

Add the shareable Loom/QuickTime/Drive link here before submitting. The video
should cover architecture, a live Compose demo, the state machine, and one
failure mode as requested in the assignment.

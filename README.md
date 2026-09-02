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

## Code structure

The application follows a small REST-friendly MVC layout:

```text
src/
├── main.rs          process entry point
├── app.rs           configuration, startup, and Axum routes
├── auth.rs          API-key authentication
├── models.rs        request, response, and database data shapes
├── controllers/     HTTP input validation and response handling
├── services/        payment, recovery, and webhook business logic
├── state.rs         shared database and HTTP clients
├── error.rs         consistent API errors
└── mock_psp.rs      separate mock payment provider
```

In a JSON API there are no HTML templates, so the serialized response models
are the view layer. Controllers stay thin, while services contain workflows
that span database transactions or external calls.

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

To see webhook delivery during the demo, create a temporary receiver URL at
webhook.site, set it below, and register it. The signing secret is shown only
in this response:

```bash
WEBHOOK_URL=https://webhook.site/replace-with-your-id
curl -sS -X POST http://localhost:8080/webhook-endpoints \
  -H "Authorization: Bearer $API_KEY" -H 'content-type: application/json' \
  -d "{\"url\":\"$WEBHOOK_URL\"}" | jq
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

After creating or paying an invoice, inspect delivery status with:

```bash
curl -sS http://localhost:8080/webhook-events \
  -H "Authorization: Bearer $API_KEY" | jq
```

## Tests

The focused integration tests run against a live Compose stack and are marked
ignored for normal offline `cargo test` runs:

```bash
TEST_BASE_URL=http://localhost:8080 \
DODO_API_KEY=dodo_test_dev_secret_change_me \
cargo test --test integration -- --ignored --nocapture
```

They cover concurrent payment attempts, exact idempotent replay without another
PSP call, cross-invoice key races, and PSP timeout reconciliation.

## Documentation

- [DESIGN.md](DESIGN.md) explains the data model, state machine, idempotency,
  failure modes, and webhook guarantees.
- [OPENAPI.yaml](OPENAPI.yaml) documents the HTTP API and error shape.
- [AI_USAGE.md](AI_USAGE.md) discloses how AI was used and what I decided.

## Demo Video

Add the shareable Loom/QuickTime/Drive link here before submitting. The video
should cover architecture, a live Compose demo, the state machine, and one
failure mode as requested in the assignment.

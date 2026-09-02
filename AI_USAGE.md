# AI Usage

I used ChatGPT/Codex during implementation for:

- Translating the assignment into a small Rust/Axum/PostgreSQL architecture.
- Drafting handler and SQL boilerplate, then compiling and correcting it locally.
- Refactoring the original single-file prototype into a small
  controllers/services/models layout while keeping the behavior unchanged.
- Reviewing idempotency, row-locking, PSP timeout handling, HMAC signing, and
  Docker Compose setup.
- Drafting the initial documentation and focused integration-test structure.

Three decisions I made independently:

1. I chose only `open -> paid` for the invoice state machine. The assignment
   mentions additional typical states, but no required endpoint needs drafts,
   voids, or collections. Keeping them out makes invalid transitions explicit
   and keeps the implementation within the time budget.
2. I used a partial unique PostgreSQL index for one pending payment attempt per
   invoice, instead of holding a database lock while making the PSP HTTP call.
   That keeps slow-provider behavior from tying up database connections.
3. I made the mock PSP idempotent by stable payment-attempt ID and added a
   status lookup. This is the smallest way to demonstrate the crash-after-PSP-
   success case without claiming a real PSP call can be rolled back.

Things AI got wrong and I corrected during verification: an early idempotency
design hashed only the request body, so the same key and body could be reused
against a different invoice. I changed the fingerprint to include the HTTP
method, invoice path, and canonical body. A live smoke test also caught that I
was inserting the idempotency row before its referenced payment-attempt row;
I reversed those inserts inside the same transaction to satisfy the foreign
key. Finally, timeout reconciliation was initially polling a still-processing
PSP charge too slowly, so processing checks now run every five seconds. A later
review found that the same business-wide idempotency key could race across two
different invoices and that recovery rows were not leased across replicas; I
added a conflict path and `SKIP LOCKED` recovery leases, then tested the race.

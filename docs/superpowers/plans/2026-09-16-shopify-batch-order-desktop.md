# Shopify Batch Order Desktop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build OrderPilot, a local macOS and Windows desktop app that stores searchable Shopify store profiles and safely creates 1–100 independent orders from one reusable order template.

**Architecture:** A React/TypeScript UI calls a narrow Tauri command surface. Rust owns validation, SQLite persistence, Shopify GraphQL requests, sequential batch execution, recovery, and CSV export; UI components never receive stored access tokens. Batch items use a persisted `sourceIdentifier` and explicit `uncertain` state to prevent blind duplicate retries.

**Tech Stack:** Tauri 2, React, TypeScript, Vite, Rust, Tokio, SQLx/SQLite, Reqwest, Serde, Vitest, React Testing Library, Cargo tests.

**Spec:** `docs/superpowers/specs/2026-09-16-shopify-batch-order-desktop-design.md`

## Global Constraints

- Target macOS and Windows; no cloud service or multi-user authentication.
- Pin every Shopify request to Admin GraphQL API `2026-07`.
- Required scopes are `read_products`, `read_customers`, `read_orders`, and `write_orders`.
- Store access tokens in local SQLite as explicitly approved; never return them from list/get commands or print them in logs.
- Batch size is an integer from 1 through 100 and defaults to 1.
- `PENDING` is the default financial status; `PAID` requires an explicit confirmation flag.
- Create batch items sequentially and persist each result before starting the next item.
- Never automatically retry an `uncertain` item after reconciliation fails.
- Use test-first red-green-refactor loops for domain logic, persistence, Shopify integration, and UI behavior.

---

## File Structure

```text
package.json                         # frontend scripts and dependencies
vite.config.ts                      # Vite dev/build configuration
src/
  main.tsx                          # React entrypoint
  App.tsx                           # route/work-surface composition
  styles.css                        # OrderPilot design tokens and responsive layout
  lib/tauri.ts                      # typed invoke/event wrapper
  types.ts                          # UI-facing DTOs
  stores/StoreSidebar.tsx           # searchable store switcher
  stores/StoreDialog.tsx            # add/edit/test/delete store flow
  orders/OrderComposer.tsx          # product/customer/status/batch form
  orders/ConfirmationDialog.tsx     # final summary and PAID warning
  batches/BatchProgress.tsx         # live counters and item results
  batches/BatchHistory.tsx          # resume/reconcile/retry/export entrypoint
  test/setup.ts                     # DOM test setup and Tauri mocks
src-tauri/
  Cargo.toml                        # Rust dependencies
  tauri.conf.json                   # desktop/window/build settings
  capabilities/default.json         # minimum Tauri permissions
  migrations/0001_init.sql          # stores, batch_jobs, batch_items schema
  src/main.rs                       # Tauri bootstrap
  src/lib.rs                        # state construction and command registration
  src/domain.rs                     # domain types and validation
  src/error.rs                      # stable app error contract and redaction
  src/repository.rs                 # SQLx repositories and migrations
  src/shopify/mod.rs                # ShopifyGateway trait and HTTP client
  src/shopify/graphql.rs            # versioned GraphQL documents and response DTOs
  src/batch.rs                      # sequential executor and recovery policy
  src/commands.rs                   # narrow Tauri command DTO boundary
  tests/repository_test.rs          # SQLite integration tests
  tests/shopify_test.rs             # mock-server GraphQL tests
  tests/batch_test.rs               # executor/recovery integration tests
```

---

### Task 1: Bootstrap the desktop application and domain contract

**Files:**
- Create: `package.json`, `vite.config.ts`, `tsconfig.json`, `index.html`
- Create: `src/main.tsx`, `src/App.tsx`, `src/styles.css`, `src/types.ts`, `src/test/setup.ts`
- Create: `src-tauri/Cargo.toml`, `src-tauri/build.rs`, `src-tauri/tauri.conf.json`, `src-tauri/capabilities/default.json`
- Create: `src-tauri/src/main.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/domain.rs`, `src-tauri/src/error.rs`

**Interfaces:**
- Produces: `normalize_shop_domain(&str) -> Result<String, AppError>`
- Produces: `BatchSize::new(u16) -> Result<BatchSize, AppError>`
- Produces: `FinancialStatus::{Pending, Paid}` and `OrderTemplate`
- Produces: serialized `AppErrorDto { code: String, message: String, retryable: bool }`

- [ ] **Step 1: Create the Tauri/Vite manifests and install locked dependencies**

Use React 19-compatible packages and Tauri 2. Add frontend test tooling and the Rust crates required by later tasks:

```bash
npm install react react-dom @tauri-apps/api @tauri-apps/plugin-dialog
npm install -D typescript vite @vitejs/plugin-react vitest jsdom @testing-library/react @testing-library/user-event @testing-library/jest-dom @types/react @types/react-dom @tauri-apps/cli
cd src-tauri && cargo add tauri serde serde_json thiserror uuid chrono secrecy async-trait tokio reqwest sqlx csv
cd src-tauri && cargo add --dev wiremock tempfile
```

Configure `npm run dev`, `npm run build`, `npm run test`, and `npm run tauri`; set the Tauri product name to `OrderPilot`, identifier to `com.orderpilot.desktop`, and the initial window to 1280×800 with a 960×680 minimum.

- [ ] **Step 2: Write failing domain validation tests**

Add to `src-tauri/src/domain.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_shop_subdomain() {
        assert_eq!(normalize_shop_domain("northwind-us").unwrap(), "northwind-us.myshopify.com");
    }

    #[test]
    fn rejects_non_shopify_domain() {
        assert_eq!(normalize_shop_domain("example.com").unwrap_err().code(), "INVALID_SHOP_DOMAIN");
    }

    #[test]
    fn batch_size_accepts_only_one_through_one_hundred() {
        assert!(BatchSize::new(1).is_ok());
        assert!(BatchSize::new(100).is_ok());
        assert!(BatchSize::new(0).is_err());
        assert!(BatchSize::new(101).is_err());
    }
}
```

- [ ] **Step 3: Run the tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::tests -- --nocapture`

Expected: compilation fails because `normalize_shop_domain`, `BatchSize`, and `AppError` do not exist.

- [ ] **Step 4: Implement the minimal domain types and redacted error DTO**

Implement lowercase domain normalization, strict Shopify subdomain validation, batch size validation, `OrderTemplate`, customer modes, and financial status. Ensure `AppError` owns safe user messages and never formats a token:

```rust
pub fn normalize_shop_domain(raw: &str) -> Result<String, AppError> {
    let value = raw.trim().to_ascii_lowercase();
    let host = value.strip_suffix(".myshopify.com").unwrap_or(&value);
    let valid = !host.is_empty()
        && host.len() <= 63
        && host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !host.starts_with('-')
        && !host.ends_with('-');
    valid.then(|| format!("{host}.myshopify.com"))
        .ok_or_else(|| AppError::validation("INVALID_SHOP_DOMAIN", "请输入有效的 Shopify 店铺域名"))
}
```

- [ ] **Step 5: Run foundation checks and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::tests && npm run test -- --run && npm run build`

Expected: all domain tests pass and the placeholder React/Tauri build succeeds.

```bash
git add package.json package-lock.json vite.config.ts tsconfig.json index.html src src-tauri
git commit -m "feat: bootstrap OrderPilot domain and desktop shell"
```

---

### Task 2: Add SQLite schema and store-profile persistence

**Files:**
- Create: `src-tauri/migrations/0001_init.sql`
- Create: `src-tauri/src/repository.rs`
- Create: `src-tauri/tests/repository_test.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**
- Produces: `Repository::connect(path) -> Result<Repository, AppError>`
- Produces: `create_store`, `search_stores`, `update_store`, `delete_store`, `store_credentials`
- Produces: public `StoreSummary` without `access_token`

- [ ] **Step 1: Write failing store repository integration tests**

```rust
#[tokio::test]
async fn searches_store_without_exposing_token() {
    let repo = test_repository().await;
    let id = repo.create_store(NewStore {
        display_name: "美国主店".into(),
        shop_domain: "northwind-us".into(),
        access_token: SecretString::from("shpat_secret"),
    }).await.unwrap();

    let stores = repo.search_stores("northwind").await.unwrap();
    assert_eq!(stores[0].id, id);
    assert_eq!(stores[0].shop_domain, "northwind-us.myshopify.com");
    assert!(!serde_json::to_string(&stores[0]).unwrap().contains("shpat_secret"));
}

#[tokio::test]
async fn deleting_store_keeps_batch_history() {
    let repo = seeded_repository_with_batch().await;
    repo.delete_store(STORE_ID).await.unwrap();
    assert_eq!(repo.list_batches().await.unwrap().len(), 1);
}
```

- [ ] **Step 2: Run repository tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test repository_test -- --nocapture`

Expected: compilation fails because migrations and `Repository` are missing.

- [ ] **Step 3: Create the migration with explicit constraints**

Define `stores`, `batch_jobs`, and `batch_items`. Use `ON DELETE SET NULL` for historical batch `store_id`, `UNIQUE(shop_domain)`, `UNIQUE(source_identifier)`, status `CHECK` constraints, and indexes for store search, batch history, and item status.

```sql
CREATE TABLE stores (
  id TEXT PRIMARY KEY,
  display_name TEXT NOT NULL,
  shop_domain TEXT NOT NULL UNIQUE,
  access_token TEXT NOT NULL,
  last_connection_status TEXT,
  last_connection_message TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
```

- [ ] **Step 4: Implement repository methods with transactional writes**

Use `SqlitePool`, run `sqlx::migrate!()` on connection, bind every SQL value, escape `LIKE` wildcards in search input, and return `StoreSummary` for UI calls. Keep token access in an internal `StoreCredentials` method used only by services.

- [ ] **Step 5: Run repository tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test repository_test`

Expected: CRUD, search, uniqueness, migration, and history-retention tests pass.

```bash
git add src-tauri/migrations src-tauri/src/repository.rs src-tauri/src/lib.rs src-tauri/tests/repository_test.rs
git commit -m "feat: persist searchable Shopify store profiles"
```

---

### Task 3: Build the Shopify GraphQL transport and connection test

**Files:**
- Create: `src-tauri/src/shopify/mod.rs`
- Create: `src-tauri/src/shopify/graphql.rs`
- Create: `src-tauri/tests/shopify_test.rs`
- Modify: `src-tauri/src/lib.rs`, `src-tauri/src/error.rs`

**Interfaces:**
- Produces: async `ShopifyGateway` trait
- Produces: `ShopifyHttpClient::new(base_url_override)` for production and mock tests
- Produces: `test_connection(&StoreCredentials) -> ConnectionReport`
- Consumes: internal `StoreCredentials`; never serializes it

- [ ] **Step 1: Write failing mock-server tests for headers, API pinning, scopes, and redaction**

```rust
#[tokio::test]
async fn test_connection_uses_pinned_graphql_endpoint_and_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/admin/api/2026-07/graphql.json"))
        .and(header("X-Shopify-Access-Token", "shpat_secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(shop_access_scopes_response()))
        .mount(&server).await;

    let report = client_for(&server).test_connection(&credentials()).await.unwrap();
    assert!(report.required_scopes_present);
}

#[tokio::test]
async fn authentication_error_never_contains_token() {
    let error = failing_client("shpat_secret").test_connection(&credentials()).await.unwrap_err();
    assert!(!format!("{error:?}").contains("shpat_secret"));
}
```

- [ ] **Step 2: Run Shopify tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test shopify_test test_connection -- --nocapture`

Expected: compilation fails because the gateway and GraphQL transport are absent.

- [ ] **Step 3: Implement the gateway and a single GraphQL request pipeline**

The request pipeline must set `Content-Type`, `X-Shopify-Access-Token`, a 30-second timeout, parse HTTP failures separately from GraphQL top-level errors and mutation `userErrors`, and map 401/403 to non-retryable credential errors. Use Shopify throttle metadata and `Retry-After` when available; retry only 429 and 5xx responses up to three total attempts.

```rust
#[async_trait]
pub trait ShopifyGateway: Send + Sync {
    async fn test_connection(&self, store: &StoreCredentials) -> Result<ConnectionReport, AppError>;
    async fn search_variants(&self, store: &StoreCredentials, query: &str) -> Result<Vec<ProductVariant>, AppError>;
    async fn search_customers(&self, store: &StoreCredentials, query: &str) -> Result<Vec<CustomerSummary>, AppError>;
    async fn create_order(&self, store: &StoreCredentials, input: CreateOrderInput) -> Result<CreatedOrder, AppError>;
    async fn find_order_by_source(&self, store: &StoreCredentials, source: &str) -> Result<Option<CreatedOrder>, AppError>;
}
```

- [ ] **Step 4: Verify transport behavior and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test shopify_test`

Expected: tests cover success, 401, 403 with missing scopes, 429, 500, timeout, malformed JSON, GraphQL errors, and token redaction.

```bash
git add src-tauri/src/shopify src-tauri/src/error.rs src-tauri/src/lib.rs src-tauri/tests/shopify_test.rs
git commit -m "feat: add resilient Shopify GraphQL gateway"
```

---

### Task 4: Implement product-variant and customer search

**Files:**
- Modify: `src-tauri/src/shopify/graphql.rs`, `src-tauri/src/shopify/mod.rs`
- Modify: `src-tauri/tests/shopify_test.rs`

**Interfaces:**
- Produces: `ProductVariant { id, product_title, variant_title, sku, price, currency_code, inventory_quantity }`
- Produces: `CustomerSummary { id, display_name, email, phone, default_address }`
- Consumes: normalized non-empty search text with a maximum length of 120 characters

- [ ] **Step 1: Add failing search tests**

```rust
#[tokio::test]
async fn maps_products_to_selectable_variants() {
    let gateway = gateway_with_fixture("product-search.json").await;
    let rows = gateway.search_variants(&credentials(), "hoodie").await.unwrap();
    assert_eq!(rows[0].sku.as_deref(), Some("HD-NV-M"));
    assert_eq!(rows[0].variant_title, "Navy / M");
}

#[tokio::test]
async fn reports_protected_customer_data_separately() {
    let gateway = gateway_with_fixture("customer-protected-data-error.json").await;
    let error = gateway.search_customers(&credentials(), "alice").await.unwrap_err();
    assert_eq!(error.code(), "CUSTOMER_DATA_RESTRICTED");
}
```

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test shopify_test search -- --nocapture`

Expected: tests fail because search GraphQL documents and response mapping are not implemented.

- [ ] **Step 3: Implement versioned search documents and safe query construction**

Use GraphQL variables, never string interpolation. Query at most 20 products/customers per request. Search product title and variant SKU, return concrete variant IDs, and preserve nullable SKU/address/phone fields. Reject blank and overlong query strings before network access.

- [ ] **Step 4: Run tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test shopify_test search`

Expected: title search, SKU search, multiple variants, empty results, invalid input, and protected-customer-data tests pass.

```bash
git add src-tauri/src/shopify src-tauri/tests/shopify_test.rs
git commit -m "feat: search Shopify variants and customers"
```

---

### Task 5: Persist batch jobs and map order creation inputs

**Files:**
- Modify: `src-tauri/src/domain.rs`, `src-tauri/src/repository.rs`
- Modify: `src-tauri/src/shopify/graphql.rs`, `src-tauri/src/shopify/mod.rs`
- Modify: `src-tauri/tests/repository_test.rs`, `src-tauri/tests/shopify_test.rs`

**Interfaces:**
- Produces: `create_batch(store_id, OrderTemplate, BatchSize) -> BatchJob`
- Produces: `next_queued_item`, `mark_creating`, `mark_succeeded`, `mark_failed`, `mark_uncertain`
- Produces: GraphQL `CreateOrderInput` with `variantId`, quantity, optional customer/address, financial status, and source identifier

- [ ] **Step 1: Write failing atomic batch creation and order-input tests**

```rust
#[tokio::test]
async fn creates_job_and_all_items_atomically() {
    let repo = test_repository().await;
    let job = repo.create_batch(STORE_ID, template(), BatchSize::new(3).unwrap()).await.unwrap();
    let items = repo.list_batch_items(job.id).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[2].source_identifier, format!("orderpilot/{}/3", job.id));
}

#[test]
fn paid_order_requires_explicit_confirmation() {
    let result = CreateOrderInput::from_template(template_paid(), "source-1", false);
    assert_eq!(result.unwrap_err().code(), "PAID_CONFIRMATION_REQUIRED");
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml create_batch paid_order -- --nocapture`

Expected: tests fail because batch repository operations and order mapping do not exist.

- [ ] **Step 3: Implement transactional batch creation and legal state transitions**

Insert the job plus all items in one SQL transaction. Freeze `OrderTemplate` as JSON. Enforce state transitions in Rust (`queued→creating`, `creating→succeeded|failed|uncertain`, `queued→stopped`) and update timestamps in the same write as result fields.

- [ ] **Step 4: Implement `orderCreate` mapping and response parsing**

Send a selected variant ID and per-order quantity. Include customer fields only for the selected customer mode, set `financialStatus`, set `sourceIdentifier`, and attach a non-sensitive custom attribute `OrderPilot-Batch` with the batch UUID. Return both Shopify GID and human-readable order name.

- [ ] **Step 5: Run tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml`

Expected: repository atomicity, unique source identifiers, PAID confirmation, optional customer shapes, GraphQL `userErrors`, and response mapping all pass.

```bash
git add src-tauri/src/domain.rs src-tauri/src/repository.rs src-tauri/src/shopify src-tauri/tests
git commit -m "feat: persist batches and create Shopify orders"
```

---

### Task 6: Implement the sequential executor, stop, resume, and reconciliation

**Files:**
- Create: `src-tauri/src/batch.rs`
- Create: `src-tauri/tests/batch_test.rs`
- Modify: `src-tauri/src/repository.rs`, `src-tauri/src/lib.rs`

**Interfaces:**
- Produces: `BatchService::run(batch_id, paid_confirmed, progress_sink)`
- Produces: `request_stop`, `resume`, `retry_failed`, `reconcile_uncertain`, `force_retry_uncertain`
- Consumes: `Arc<dyn ShopifyGateway>`, `Repository`, `ProgressSink`

- [ ] **Step 1: Write failing executor tests with a fake gateway**

```rust
#[tokio::test]
async fn persists_each_success_before_starting_next_item() {
    let fixture = Fixture::batch(3).with_gateway_results(vec![ok("#1"), ok("#2"), ok("#3")]);
    fixture.service.run(fixture.batch_id, true, fixture.progress()).await.unwrap();
    assert_eq!(fixture.repo.item_statuses(fixture.batch_id).await, vec!["succeeded"; 3]);
    assert_eq!(fixture.gateway.max_concurrency(), 1);
}

#[tokio::test]
async fn uncertain_item_is_reconciled_but_never_blindly_retried() {
    let fixture = Fixture::batch(1).with_timeout_then_reconciliation(None);
    fixture.service.run(fixture.batch_id, true, fixture.progress()).await.unwrap();
    assert_eq!(fixture.repo.item_status(1).await, "uncertain");
    assert_eq!(fixture.gateway.create_calls(), 1);
}
```

- [ ] **Step 2: Run executor tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test batch_test -- --nocapture`

Expected: compilation fails because `BatchService` and progress events do not exist.

- [ ] **Step 3: Implement the sequential execution loop**

Load the frozen template, resolve credentials internally, mark one item `creating`, call Shopify, persist the terminal result, emit a sanitized progress event, then continue. Credential/permission/database errors pause the job; item business errors mark only that item failed.

- [ ] **Step 4: Implement lifecycle controls and reconciliation**

Stop converts only queued items to stopped. Resume converts stopped items back to queued only after user action. Retry creates a new attempt only for `failed`; `uncertain` requires three reconciliation queries over 30 seconds and remains uncertain if absent. `force_retry_uncertain` requires `risk_confirmed: true` and records the user-forced attempt before submitting.

- [ ] **Step 5: Run executor tests and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test batch_test`

Expected: sequentiality, partial failure, stop, resume, credential pause, timeout-found, timeout-not-found, failed retry, and forced retry tests pass.

```bash
git add src-tauri/src/batch.rs src-tauri/src/repository.rs src-tauri/src/lib.rs src-tauri/tests/batch_test.rs
git commit -m "feat: execute and recover Shopify order batches"
```

---

### Task 7: Expose a narrow typed Tauri command surface

**Files:**
- Create: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/lib.rs`, `src-tauri/src/main.rs`
- Create: `src/lib/tauri.ts`
- Modify: `src/types.ts`

**Interfaces:**
- Produces commands: `search_stores`, `save_store`, `test_store_connection`, `delete_store`
- Produces commands: `search_variants`, `search_customers`, `create_batch`, `start_batch`
- Produces commands: `get_batch`, `list_batches`, `stop_batch`, `resume_batch`, `retry_failed`, `reconcile_uncertain`, `force_retry_uncertain`, `export_batch_csv`
- Produces event: `batch-progress` carrying sanitized `BatchProgressDto`

- [ ] **Step 1: Write failing serialization and command-boundary tests**

```rust
#[test]
fn store_summary_json_has_no_access_token_field() {
    let json = serde_json::to_value(StoreSummaryDto::from(summary())).unwrap();
    assert!(json.get("accessToken").is_none());
}

#[test]
fn app_error_dto_is_stable() {
    let dto = AppErrorDto::from(AppError::validation("INVALID_BATCH_SIZE", "批量数量必须为 1 到 100"));
    assert_eq!(dto.code, "INVALID_BATCH_SIZE");
    assert!(!dto.retryable);
}
```

- [ ] **Step 2: Run command tests and verify RED**

Run: `cargo test --manifest-path src-tauri/Cargo.toml commands::tests -- --nocapture`

Expected: tests fail because command DTOs and registrations are missing.

- [ ] **Step 3: Implement commands, state, event emission, and typed frontend wrappers**

Use one `AppState` containing `Repository`, `Arc<dyn ShopifyGateway>`, `BatchService`, and a task registry keyed by batch UUID. Commands return DTOs only. `src/lib/tauri.ts` is the only frontend module allowed to call `invoke` or `listen`.

```ts
export const api = {
  searchStores: (query: string) => invoke<StoreSummary[]>('search_stores', { query }),
  createBatch: (input: CreateBatchRequest) => invoke<BatchJob>('create_batch', { input }),
  startBatch: (batchId: string, paidConfirmed: boolean) =>
    invoke<void>('start_batch', { batchId, paidConfirmed }),
};
```

- [ ] **Step 4: Run Rust and TypeScript checks and commit**

Run: `cargo test --manifest-path src-tauri/Cargo.toml && npm run build`

Expected: command tests and TypeScript compilation pass; no DTO contains an access token.

```bash
git add src-tauri/src src/lib/tauri.ts src/types.ts
git commit -m "feat: expose typed Tauri application commands"
```

---

### Task 8: Build the store sidebar and store-management dialog

**Files:**
- Create: `src/stores/StoreSidebar.tsx`, `src/stores/StoreDialog.tsx`
- Create: `src/stores/StoreSidebar.test.tsx`, `src/stores/StoreDialog.test.tsx`
- Modify: `src/App.tsx`, `src/styles.css`, `src/test/setup.ts`

**Interfaces:**
- Consumes: store commands from `src/lib/tauri.ts`
- Produces: selected `storeId` for the order composer

- [ ] **Step 1: Write failing UI tests for search, token masking, test connection, and deletion**

```tsx
it('keeps the token masked and saves a normalized store', async () => {
  render(<StoreDialog open mode="create" onClose={vi.fn()} onSaved={vi.fn()} />);
  await user.type(screen.getByLabelText('店铺显示名称'), '美国主店');
  await user.type(screen.getByLabelText('Shopify 店铺域名'), 'northwind-us');
  await user.type(screen.getByLabelText('Admin API Access Token'), 'shpat_secret');
  expect(screen.getByLabelText('Admin API Access Token')).toHaveAttribute('type', 'password');
  await user.click(screen.getByRole('button', { name: '测试连接' }));
  expect(await screen.findByText('连接正常')).toBeVisible();
});
```

- [ ] **Step 2: Run UI tests and verify RED**

Run: `npm run test -- --run src/stores`

Expected: tests fail because store components are missing.

- [ ] **Step 3: Implement the approved focused store UI**

Build the dark searchable sidebar and accessible dialog from the accepted prototype. Use semantic labels, keyboard focus management, 16px body text, clear empty/loading/error states, a show/hide token control, permission-specific connection results, and a destructive confirmation before delete. Do not show a dashboard.

- [ ] **Step 4: Run UI tests and commit**

Run: `npm run test -- --run src/stores && npm run build`

Expected: search, selection, create/edit, masking, connection test, permission error, delete confirmation, and keyboard tests pass.

```bash
git add src/stores src/App.tsx src/styles.css src/test/setup.ts
git commit -m "feat: add searchable Shopify store management UI"
```

---

### Task 9: Build the order composer and confirmation flow

**Files:**
- Create: `src/orders/OrderComposer.tsx`, `src/orders/ConfirmationDialog.tsx`
- Create: `src/orders/OrderComposer.test.tsx`, `src/orders/ConfirmationDialog.test.tsx`
- Modify: `src/App.tsx`, `src/styles.css`

**Interfaces:**
- Consumes: selected store ID, variant/customer search, `create_batch`, `start_batch`
- Produces: validated `CreateBatchRequest`

- [ ] **Step 1: Write failing tests for the complete order form**

```tsx
it('defaults to one pending order and separates per-order quantity from batch count', async () => {
  render(<OrderComposer storeId="store-1" />);
  expect(screen.getByLabelText('付款状态')).toHaveValue('PENDING');
  expect(screen.getByLabelText('批量创建数量')).toHaveValue(1);
  await user.clear(screen.getByLabelText('批量创建数量'));
  await user.type(screen.getByLabelText('批量创建数量'), '10');
  expect(screen.getByText('预计创建 10 张独立订单')).toBeVisible();
});

it('requires a second confirmation for paid orders', async () => {
  renderReadyComposer({ financialStatus: 'PAID' });
  await user.click(screen.getByRole('button', { name: /检查并创建/ }));
  expect(screen.getByText(/只适用于已经通过其他渠道完成收款/)).toBeVisible();
});
```

- [ ] **Step 2: Run order UI tests and verify RED**

Run: `npm run test -- --run src/orders`

Expected: tests fail because composer and confirmation components are missing.

- [ ] **Step 3: Implement variant selection, customer modes, validation, and summary**

Debounce searches by 250 ms and cancel stale results. Require a concrete variant, positive per-order quantity, and integer batch size 1–100. Support existing customer, manual customer, and no customer. Show exact store, variant, per-order quantity, customer, status, per-order amount, and batch count before submission.

- [ ] **Step 4: Implement PAID confirmation and batch start**

For `PENDING`, confirmation creates and starts the batch. For `PAID`, require a dedicated checkbox or second confirm button and pass `paidConfirmed: true`. Disable duplicate submissions while `create_batch` or `start_batch` is pending, then navigate to the returned batch ID.

- [ ] **Step 5: Run UI tests and commit**

Run: `npm run test -- --run src/orders && npm run build`

Expected: all customer modes, product selection, validation boundaries, summaries, PAID confirmation, error states, and double-submit prevention pass.

```bash
git add src/orders src/App.tsx src/styles.css
git commit -m "feat: compose and confirm Shopify order batches"
```

---

### Task 10: Build batch progress, history, recovery, and CSV export

**Files:**
- Create: `src/batches/BatchProgress.tsx`, `src/batches/BatchHistory.tsx`
- Create: `src/batches/BatchProgress.test.tsx`, `src/batches/BatchHistory.test.tsx`
- Modify: `src/App.tsx`, `src/styles.css`, `src/lib/tauri.ts`
- Modify: `src-tauri/src/commands.rs`, `src-tauri/src/repository.rs`

**Interfaces:**
- Consumes: `batch-progress` events and batch lifecycle commands
- Produces: downloadable CSV columns `sequence,status,shopify_order_name,shopify_order_id,source_identifier,error_code,error_message`

- [ ] **Step 1: Write failing progress and recovery UI tests**

```tsx
it('updates counters from progress events and never offers ordinary retry for uncertain items', async () => {
  render(<BatchProgress batchId="batch-1" />);
  emitProgress({ succeeded: 6, failed: 1, uncertain: 1, queued: 2 });
  expect(await screen.findByText('6')).toBeVisible();
  expect(screen.getByRole('button', { name: '仅重试失败项' })).toBeEnabled();
  expect(screen.getByRole('button', { name: '重新核对待确认项' })).toBeEnabled();
  expect(screen.queryByRole('button', { name: '重试待确认项' })).not.toBeInTheDocument();
});
```

- [ ] **Step 2: Run batch UI tests and verify RED**

Run: `npm run test -- --run src/batches`

Expected: tests fail because batch views and event subscriptions are missing.

- [ ] **Step 3: Implement live progress, history, and lifecycle actions**

Render progress bar, four counters, current item, and a scrollable accessible result table. Subscribe on mount and always unlisten on unmount. Provide stop, explicit resume, failed-only retry, uncertain reconciliation, and a separate force-retry dialog that states duplicate-order risk.

- [ ] **Step 4: Implement deterministic CSV export**

Sort rows by sequence, emit UTF-8 with BOM for Excel compatibility, quote fields with the `csv` crate, and use Tauri's save dialog. Never include store tokens or raw request headers.

- [ ] **Step 5: Run batch UI/backend tests and commit**

Run: `npm run test -- --run src/batches && cargo test --manifest-path src-tauri/Cargo.toml && npm run build`

Expected: event cleanup, counters, table states, stop/resume/retry/reconcile confirmations, CSV escaping, and token exclusion tests pass.

```bash
git add src/batches src/App.tsx src/styles.css src/lib/tauri.ts src-tauri/src/commands.rs src-tauri/src/repository.rs
git commit -m "feat: show and recover batch order results"
```

---

### Task 11: Verify the complete desktop application and package both platforms

**Files:**
- Create: `docs/testing/shopify-development-store.md`
- Create: `.github/workflows/release.yml`
- Modify: `README.md`, `src-tauri/tauri.conf.json`, `package.json`

**Interfaces:**
- Consumes: all earlier application interfaces
- Produces: local macOS and Windows installers and a documented development-store acceptance procedure

- [ ] **Step 1: Add a release workflow and acceptance checklist**

Configure a matrix for `macos-latest` and `windows-latest` that runs frontend tests, Rust tests, frontend build, and `npm run tauri build`. Do not publish installers automatically; upload them as workflow artifacts.

Document exact development-store setup: create/install an app with the four scopes, copy an offline Admin API token, add a product with a known variant/SKU, create a customer, and use Shopify test mode rather than a live payment provider.

- [ ] **Step 2: Run every automated gate locally**

Run:

```bash
npm run test -- --run
npm run build
cargo fmt --manifest-path src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path src-tauri/Cargo.toml
npm run tauri build
```

Expected: all tests, formatting, linting, frontend build, and the current-platform desktop installer succeed.

- [ ] **Step 3: Perform the development-store acceptance run**

Create a store profile, test scopes, find the known SKU, select an existing customer, keep `PENDING`, set per-order quantity to 1, set batch size to 10, confirm, and verify 10 explicit terminal results. Repeat with one mocked timeout and confirm reconciliation finds the source identifier without creating a duplicate.

- [ ] **Step 4: Inspect secrets and final diff**

Run:

```bash
rg -n "shpat_|X-Shopify-Access-Token" --glob '!docs/**' --glob '!**/target/**' --glob '!node_modules/**'
git status --short
git diff --check
```

Expected: only header-name constants and sanitized test fixtures match; no real token, generated database, installer, or build directory is tracked; `git diff --check` is clean.

- [ ] **Step 5: Commit release readiness**

```bash
git add README.md docs/testing .github/workflows/release.yml src-tauri/tauri.conf.json package.json package-lock.json
git commit -m "chore: verify and package OrderPilot desktop app"
```

---

## Plan Self-Review Result

- Every requirement in the approved spec maps to at least one task.
- Domain, persistence, Shopify transport, batch recovery, command boundary, UI, export, and packaging have explicit tests.
- Tokens remain internal to Rust services and are excluded from DTOs, events, CSV, and logs.
- `failed` and `uncertain` are distinct throughout; only failed items have ordinary retry.
- Shopify API version, batch limits, default payment status, PAID confirmation, and platform targets match the spec.

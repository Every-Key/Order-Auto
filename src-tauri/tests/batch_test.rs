use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use orderpilot_lib::{
    batch::{BatchProgress, BatchService, ForcedClaimGate, ProgressSink, ReconciliationClock},
    domain::{
        BatchItem, BatchItemStatus as Status, BatchSize, CustomerMode, FinancialStatus,
        OrderTemplate,
    },
    error::AppError,
    repository::{ForcedRetryAttempt, NewStore, Repository, StoreCredentials},
    shopify::{
        ConnectionReport, CreateOrderInput, CreatedOrder, CustomerSummary, ProductVariant,
        ShopifyGateway,
    },
};
use secrecy::SecretString;
use tempfile::TempDir;
use tokio::sync::Notify;

#[derive(Default)]
struct CreateGate {
    entered: Notify,
    release: Notify,
}

#[derive(Default)]
struct ClaimGate {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl ForcedClaimGate for ClaimGate {
    async fn before_claim(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<BatchProgress>>);
impl ProgressSink for Events {
    fn emit(&self, event: BatchProgress) {
        self.0.lock().unwrap().push(event);
    }
}

struct FakeGateway {
    repo: Repository,
    batch_id: String,
    results: Mutex<VecDeque<Result<CreatedOrder, AppError>>>,
    seen: Mutex<Vec<Vec<BatchItem>>>,
    inputs: Mutex<Vec<serde_json::Value>>,
    calls: AtomicUsize,
    in_flight: AtomicUsize,
    max_concurrency: AtomicUsize,
    found: Mutex<VecDeque<Result<Option<CreatedOrder>, AppError>>>,
    lookups: Mutex<Vec<(String, Duration)>>,
    clock: Arc<TestClock>,
    gate: Mutex<Option<Arc<CreateGate>>>,
    lookup_gate: Mutex<Option<Arc<CreateGate>>>,
    audits_seen: Mutex<Vec<Vec<ForcedRetryAttempt>>>,
}

#[derive(Default)]
struct TestClock(AtomicU64);
#[async_trait]
impl ReconciliationClock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
    async fn wait(&self, duration: Duration) {
        self.0
            .fetch_add(duration.as_millis() as u64, Ordering::SeqCst);
    }
}

#[async_trait]
impl ShopifyGateway for FakeGateway {
    async fn find_order_by_source(
        &self,
        _: &StoreCredentials,
        source: &str,
    ) -> Result<Option<CreatedOrder>, AppError> {
        let items = self.repo.list_batch_items(&self.batch_id).await.unwrap();
        assert_eq!(
            items
                .iter()
                .find(|item| item.source_identifier == source)
                .unwrap()
                .status,
            Status::Uncertain
        );
        self.lookups
            .lock()
            .unwrap()
            .push((source.into(), self.clock.now()));
        let gate = self.lookup_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.found.lock().unwrap().pop_front().unwrap_or(Ok(None))
    }
    async fn create_order(
        &self,
        _: &StoreCredentials,
        input: CreateOrderInput,
    ) -> Result<CreatedOrder, AppError> {
        let concurrency = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrency
            .fetch_max(concurrency, Ordering::SeqCst);
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot = self.repo.list_batch_items(&self.batch_id).await.unwrap();
        self.seen.lock().unwrap().push(snapshot);
        let audits = self
            .repo
            .list_forced_retry_attempts(&self.batch_id)
            .await
            .unwrap();
        self.audits_seen.lock().unwrap().push(audits);
        self.inputs
            .lock()
            .unwrap()
            .push(serde_json::to_value(input).unwrap());
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        tokio::task::yield_now().await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(order(call)))
    }
    async fn test_connection(&self, _: &StoreCredentials) -> Result<ConnectionReport, AppError> {
        unreachable!()
    }
    async fn search_variants(
        &self,
        _: &StoreCredentials,
        _: &str,
    ) -> Result<Vec<ProductVariant>, AppError> {
        unreachable!()
    }
    async fn search_customers(
        &self,
        _: &StoreCredentials,
        _: &str,
    ) -> Result<Vec<CustomerSummary>, AppError> {
        unreachable!()
    }
}

fn order(number: usize) -> CreatedOrder {
    CreatedOrder {
        id: format!("gid://shopify/Order/{number}"),
        name: format!("#{number}"),
    }
}

fn template() -> OrderTemplate {
    OrderTemplate {
        variant_id: "gid://shopify/ProductVariant/123".into(),
        quantity: 2,
        customer: CustomerMode::None,
        financial_status: FinancialStatus::Pending,
    }
}

struct Fixture {
    _directory: TempDir,
    repo: Repository,
    batch_id: String,
    store_id: String,
    gateway: Arc<FakeGateway>,
    service: BatchService,
    events: Events,
}

impl Fixture {
    async fn batch(size: u16) -> Self {
        Self::with_template(size, template()).await
    }
    async fn with_template(size: u16, template: OrderTemplate) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::connect(directory.path().join("test.sqlite"))
            .await
            .unwrap();
        let store_id = repo
            .create_store(NewStore {
                display_name: "Test".into(),
                shop_domain: "test-shop".into(),
                access_token: SecretString::from("shpat_secret"),
            })
            .await
            .unwrap();
        let batch_id = repo
            .create_batch(&store_id, template, BatchSize::new(size).unwrap())
            .await
            .unwrap()
            .id;
        let clock = Arc::new(TestClock::default());
        let gateway = Arc::new(FakeGateway {
            repo: repo.clone(),
            batch_id: batch_id.clone(),
            results: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_concurrency: AtomicUsize::new(0),
            found: Mutex::new(VecDeque::new()),
            lookups: Mutex::new(Vec::new()),
            clock: clock.clone(),
            gate: Mutex::new(None),
            lookup_gate: Mutex::new(None),
            audits_seen: Mutex::new(Vec::new()),
        });
        let service = BatchService::new(repo.clone(), gateway.clone())
            .await
            .unwrap()
            .with_reconciliation_clock(clock);
        Self {
            _directory: directory,
            repo,
            batch_id,
            store_id,
            gateway,
            service,
            events: Events::default(),
        }
    }
    async fn items(&self) -> Vec<BatchItem> {
        self.repo.list_batch_items(&self.batch_id).await.unwrap()
    }
}

#[tokio::test]
async fn persists_each_success_before_starting_next_item() {
    let f = Fixture::batch(3).await;
    f.service.run(&f.batch_id, false, &f.events).await.unwrap();
    let items = f.items().await;
    assert!(items
        .iter()
        .all(|item| item.status == Status::Succeeded && item.attempt_count == 1));
    assert_eq!(f.gateway.max_concurrency.load(Ordering::SeqCst), 1);
    for (index, snapshot) in f.gateway.seen.lock().unwrap().iter().enumerate() {
        assert!(snapshot[..index]
            .iter()
            .all(|item| item.status == Status::Succeeded && item.shopify_order_id.is_some()));
        assert_eq!(snapshot[index].status, Status::Creating);
    }
    let job = f.repo.get_batch(&f.batch_id).await.unwrap();
    assert_eq!(job.status, "completed");
    assert!(job.started_at.is_some() && job.finished_at.is_some());
    let events = f.events.0.lock().unwrap();
    assert_eq!(events.last().unwrap().succeeded, 3);
    assert_eq!(events.last().unwrap().total, 3);
    assert_eq!(events.last().unwrap().status, "completed");
}

#[tokio::test]
async fn item_business_failure_is_redacted_persisted_and_does_not_stop_later_items() {
    let f = Fixture::batch(3).await;
    f.gateway.results.lock().unwrap().extend([
        Ok(order(1)),
        Err(AppError::validation(
            "SHOPIFY_USER_ERROR",
            "[INVALID] Address invalid shpat_secret",
        )),
        Ok(order(3)),
    ]);
    f.service.run(&f.batch_id, false, &f.events).await.unwrap();
    let items = f.items().await;
    assert_eq!(
        items.iter().map(|item| item.status).collect::<Vec<_>>(),
        vec![Status::Succeeded, Status::Failed, Status::Succeeded]
    );
    assert_eq!(items[1].error_code.as_deref(), Some("SHOPIFY_USER_ERROR"));
    assert_eq!(
        items[1].error_message.as_deref(),
        Some("[INVALID] Address invalid [REDACTED]")
    );
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "completed_with_errors"
    );
    let json = serde_json::to_string(&*f.events.0.lock().unwrap()).unwrap();
    assert!(!json.contains("shpat_secret"));
    assert!(!json.contains("accessToken"));
    assert!(!json.contains("orderTemplate"));
}

#[tokio::test]
async fn credential_and_permission_failures_pause_the_batch_without_failing_business_items() {
    for code in [
        "SHOPIFY_UNAUTHORIZED",
        "SHOPIFY_FORBIDDEN",
        "SHOPIFY_CREDENTIALS",
        "CUSTOMER_DATA_RESTRICTED",
        "INVALID_SHOP_DOMAIN",
    ] {
        let f = Fixture::batch(3).await;
        f.gateway
            .results
            .lock()
            .unwrap()
            .push_back(Err(AppError::validation(
                code,
                "Access denied shpat_secret",
            )));
        let error = f
            .service
            .run(&f.batch_id, false, &f.events)
            .await
            .unwrap_err();
        assert_eq!(error.code(), code);
        assert!(!error.message().contains("shpat_secret"));
        assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
        let items = f.items().await;
        assert!(items.iter().all(|item| item.status == Status::Queued));
        assert_eq!(items[0].error_code.as_deref(), Some(code));
        assert_eq!(items[0].attempt_count, 1);
        assert_eq!(items[1].attempt_count, 0);
        assert_eq!(
            f.repo.get_batch(&f.batch_id).await.unwrap().status,
            "paused"
        );
        assert_eq!(f.events.0.lock().unwrap().last().unwrap().status, "paused");
    }
}

#[tokio::test]
async fn ambiguous_outcomes_are_reconciled_three_times_within_thirty_seconds_never_recreated() {
    for code in [
        "SHOPIFY_TIMEOUT",
        "SHOPIFY_NETWORK",
        "SHOPIFY_SERVER_ERROR",
        "SHOPIFY_INVALID_RESPONSE",
        "SHOPIFY_GRAPHQL",
        "SHOPIFY_HTTP",
    ] {
        let f = Fixture::batch(1).await;
        f.gateway
            .results
            .lock()
            .unwrap()
            .push_back(Err(AppError::validation(code, "Unknown result")));
        f.service.run(&f.batch_id, false, &f.events).await.unwrap();
        let items = f.items().await;
        assert_eq!(items[0].status, Status::Uncertain);
        assert_eq!(items[0].attempt_count, 1);
        assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *f.gateway.lookups.lock().unwrap(),
            vec![
                (items[0].source_identifier.clone(), Duration::ZERO),
                (items[0].source_identifier.clone(), Duration::from_secs(10)),
                (items[0].source_identifier.clone(), Duration::from_secs(20))
            ]
        );
        assert_eq!(
            f.repo.get_batch(&f.batch_id).await.unwrap().status,
            "completed_with_errors"
        );
    }
}

#[tokio::test]
async fn startup_recovers_creating_as_uncertain_without_submitting_any_orders() {
    let f = Fixture::batch(2).await;
    let first = f.items().await.remove(0);
    f.repo.mark_creating(&first.id).await.unwrap();
    let reopened = Repository::connect(f._directory.path().join("test.sqlite"))
        .await
        .unwrap();
    let _service = BatchService::new(reopened.clone(), f.gateway.clone())
        .await
        .unwrap();
    let items = reopened.list_batch_items(&f.batch_id).await.unwrap();
    assert_eq!(items[0].status, Status::Uncertain);
    assert_eq!(items[0].attempt_count, 1);
    assert_eq!(items[0].error_code.as_deref(), Some("PROCESS_INTERRUPTED"));
    assert_eq!(items[1].status, Status::Queued);
    assert_eq!(
        reopened.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert!(f.gateway.lookups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn explicit_resume_reconciles_crashed_creating_before_queued_work_and_never_resubmits_it() {
    let f = Fixture::batch(2).await;
    let first = f.items().await.remove(0);
    f.repo.mark_creating(&first.id).await.unwrap();
    f.gateway
        .found
        .lock()
        .unwrap()
        .extend([Ok(None), Ok(Some(order(42)))]);
    f.service
        .resume(&f.batch_id, false, &f.events)
        .await
        .unwrap();
    let items = f.items().await;
    assert!(items.iter().all(|item| item.status == Status::Succeeded));
    assert_eq!(items[0].shopify_order_name.as_deref(), Some("#42"));
    assert_eq!(items[0].attempt_count, 1);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.gateway.seen.lock().unwrap()[0][0].status,
        Status::Succeeded
    );
    assert_eq!(f.gateway.lookups.lock().unwrap().len(), 2);
    assert_eq!(
        f.gateway.inputs.lock().unwrap()[0]["sourceIdentifier"],
        items[1].source_identifier
    );
}

#[tokio::test]
async fn stop_finishes_in_flight_order_and_only_explicit_resume_runs_stopped_items() {
    let f = Fixture::batch(3).await;
    let gate = Arc::new(CreateGate::default());
    *f.gateway.gate.lock().unwrap() = Some(gate.clone());
    let service = f.service.clone();
    let batch_id = f.batch_id.clone();
    let running =
        tokio::spawn(async move { service.run(&batch_id, false, &Events::default()).await });
    gate.entered.notified().await;
    f.service
        .request_stop(&f.batch_id, &f.events)
        .await
        .unwrap();
    assert_eq!(
        f.items()
            .await
            .iter()
            .map(|item| item.status)
            .collect::<Vec<_>>(),
        vec![Status::Creating, Status::Stopped, Status::Stopped]
    );
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "stopping"
    );
    gate.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
    assert_eq!(
        f.service
            .run(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "BATCH_RESUME_REQUIRED"
    );
    f.service
        .resume(&f.batch_id, false, &f.events)
        .await
        .unwrap();
    assert!(f
        .items()
        .await
        .iter()
        .all(|item| item.status == Status::Succeeded && item.attempt_count == 1));
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn duplicate_lifecycle_actions_cannot_recover_or_submit_a_live_item() {
    let f = Fixture::batch(2).await;
    let gate = Arc::new(CreateGate::default());
    *f.gateway.gate.lock().unwrap() = Some(gate.clone());
    let service = f.service.clone();
    let batch_id = f.batch_id.clone();
    let running =
        tokio::spawn(async move { service.run(&batch_id, false, &Events::default()).await });
    gate.entered.notified().await;
    let error = f
        .service
        .resume(&f.batch_id, false, &f.events)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "BATCH_BUSY");
    assert_eq!(f.items().await[0].status, Status::Creating);
    assert!(f.gateway.lookups.lock().unwrap().is_empty());
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    gate.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(f.gateway.max_concurrency.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retry_failed_creates_new_attempts_only_for_failed_items() {
    let f = Fixture::batch(4).await;
    let items = f.items().await;
    f.repo.mark_creating(&items[0].id).await.unwrap();
    f.repo
        .mark_failed(
            &items[0].id,
            &AppError::validation("SHOPIFY_USER_ERROR", "Bad address"),
        )
        .await
        .unwrap();
    f.repo.mark_creating(&items[1].id).await.unwrap();
    f.repo
        .mark_uncertain(
            &items[1].id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    f.repo.mark_stopped(&items[2].id).await.unwrap();
    f.service
        .retry_failed(&f.batch_id, false, &f.events)
        .await
        .unwrap();
    let saved = f.items().await;
    assert_eq!(
        saved.iter().map(|item| item.status).collect::<Vec<_>>(),
        vec![
            Status::Succeeded,
            Status::Uncertain,
            Status::Stopped,
            Status::Queued
        ]
    );
    assert_eq!(saved[0].attempt_count, 2);
    assert_eq!(saved[0].source_identifier, items[0].source_identifier);
    assert_eq!(saved[1].attempt_count, 1);
    assert_eq!(saved[2].attempt_count, 0);
    assert_eq!(saved[3].attempt_count, 0);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert!(f.gateway.lookups.lock().unwrap().is_empty());
}

#[tokio::test]
async fn manual_reconciliation_only_queries_uncertain_items_and_leaves_other_work_untouched() {
    let f = Fixture::batch(3).await;
    let items = f.items().await;
    for item in &items[..2] {
        f.repo.mark_creating(&item.id).await.unwrap();
        f.repo
            .mark_uncertain(
                &item.id,
                &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
            )
            .await
            .unwrap();
    }
    f.gateway.found.lock().unwrap().extend([
        Ok(None),
        Ok(Some(order(42))),
        Err(AppError::validation("SHOPIFY_NETWORK", "Unavailable")),
    ]);
    f.service
        .reconcile_uncertain(&f.batch_id, &f.events)
        .await
        .unwrap();
    let saved = f.items().await;
    assert_eq!(
        saved.iter().map(|item| item.status).collect::<Vec<_>>(),
        vec![Status::Succeeded, Status::Uncertain, Status::Queued]
    );
    assert_eq!(saved[0].attempt_count, 1);
    assert_eq!(saved[0].shopify_order_name.as_deref(), Some("#42"));
    assert_eq!(saved[1].attempt_count, 1);
    assert_eq!(saved[2].attempt_count, 0);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.gateway.lookups.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn force_retry_requires_confirmation_and_durably_records_it_before_submitting_only_that_item()
{
    let f = Fixture::batch(2).await;
    let first = f.items().await.remove(0);
    f.repo.mark_creating(&first.id).await.unwrap();
    f.repo
        .mark_uncertain(
            &first.id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    let error = f
        .service
        .force_retry_uncertain(&f.batch_id, &first.id, false, false, &f.events)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "DUPLICATE_RISK_CONFIRMATION_REQUIRED");
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert!(f
        .repo
        .list_forced_retry_attempts(&f.batch_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(f.items().await[0].status, Status::Uncertain);
    f.service
        .force_retry_uncertain(&f.batch_id, &first.id, false, true, &f.events)
        .await
        .unwrap();
    let saved = f.items().await;
    assert_eq!(saved[0].status, Status::Succeeded);
    assert_eq!(saved[0].attempt_count, 2);
    assert_eq!(saved[0].source_identifier, first.source_identifier);
    assert_eq!(saved[1].status, Status::Queued);
    assert_eq!(saved[1].attempt_count, 0);
    let before_submit = f.gateway.audits_seen.lock().unwrap()[0].clone();
    assert_eq!(before_submit.len(), 1);
    assert_eq!(before_submit[0].item_id, first.id);
    assert_eq!(before_submit[0].attempt_number, 2);
    assert!(before_submit[0].risk_confirmed);
    assert!(!before_submit[0].confirmed_at.is_empty());
    let reopened = Repository::connect(f._directory.path().join("test.sqlite"))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .list_forced_retry_attempts(&f.batch_id)
            .await
            .unwrap(),
        before_submit
    );
}

#[tokio::test]
async fn stop_before_forced_claim_prevents_submission_audit_and_status_override() {
    let f = Fixture::batch(1).await;
    let item = f.items().await.remove(0);
    f.repo.mark_creating(&item.id).await.unwrap();
    f.repo
        .mark_uncertain(
            &item.id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();

    let gate = Arc::new(ClaimGate::default());
    let service = f.service.clone().with_forced_claim_gate(gate.clone());
    let batch_id = f.batch_id.clone();
    let item_id = item.id.clone();
    let events = Arc::new(Events::default());
    let force = tokio::spawn({
        let events = events.clone();
        async move {
            service
                .force_retry_uncertain(&batch_id, &item_id, false, true, events.as_ref())
                .await
        }
    });

    gate.entered.notified().await;
    f.service
        .request_stop(&f.batch_id, &f.events)
        .await
        .unwrap();
    gate.release.notify_one();

    assert_eq!(force.await.unwrap().unwrap_err().code(), "BATCH_STOPPED");
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert!(f
        .repo
        .list_forced_retry_attempts(&f.batch_id)
        .await
        .unwrap()
        .is_empty());
    let saved = f.items().await;
    assert_eq!(saved[0].status, Status::Uncertain);
    assert_eq!(saved[0].attempt_count, 1);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn rejected_forced_attempt_does_not_erase_the_original_uncertainty() {
    for code in ["SHOPIFY_USER_ERROR", "SHOPIFY_FORBIDDEN"] {
        let f = Fixture::batch(1).await;
        let item = f.items().await.remove(0);
        f.repo.mark_creating(&item.id).await.unwrap();
        f.repo
            .mark_uncertain(
                &item.id,
                &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
            )
            .await
            .unwrap();
        f.gateway
            .results
            .lock()
            .unwrap()
            .push_back(Err(AppError::validation(code, "Rejected")));
        let result = f
            .service
            .force_retry_uncertain(&f.batch_id, &item.id, false, true, &f.events)
            .await;
        if code == "SHOPIFY_FORBIDDEN" {
            assert_eq!(result.unwrap_err().code(), code);
        } else {
            result.unwrap();
        }
        assert_eq!(f.items().await[0].status, Status::Uncertain);
        let before_retry_status = f.repo.get_batch(&f.batch_id).await.unwrap().status;
        f.service
            .retry_failed(&f.batch_id, false, &f.events)
            .await
            .unwrap();
        assert_eq!(
            f.repo.get_batch(&f.batch_id).await.unwrap().status,
            before_retry_status
        );
        assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.items().await[0].attempt_count, 2);
        assert_eq!(
            f.repo
                .list_forced_retry_attempts(&f.batch_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn paid_confirmation_is_checked_before_any_resume_or_retry_changes() {
    let mut paid = template();
    paid.financial_status = FinancialStatus::Paid;
    let f = Fixture::with_template(3, paid).await;
    let items = f.items().await;
    f.repo.mark_stopped(&items[0].id).await.unwrap();
    f.repo.mark_creating(&items[1].id).await.unwrap();
    f.repo
        .mark_failed(
            &items[1].id,
            &AppError::validation("SHOPIFY_USER_ERROR", "Rejected"),
        )
        .await
        .unwrap();
    f.repo.mark_creating(&items[2].id).await.unwrap();
    f.repo
        .mark_uncertain(
            &items[2].id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    let before = f.items().await;
    assert_eq!(
        f.service
            .resume(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "PAID_CONFIRMATION_REQUIRED"
    );
    assert_eq!(f.items().await, before);
    assert_eq!(
        f.service
            .retry_failed(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "PAID_CONFIRMATION_REQUIRED"
    );
    assert_eq!(f.items().await, before);
    assert_eq!(
        f.service
            .force_retry_uncertain(&f.batch_id, &items[2].id, false, true, &f.events)
            .await
            .unwrap_err()
            .code(),
        "PAID_CONFIRMATION_REQUIRED"
    );
    assert_eq!(f.items().await, before);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert!(f.gateway.lookups.lock().unwrap().is_empty());
}

async fn fault_connection(f: &Fixture) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(f._directory.path().join("test.sqlite")),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn database_result_failure_pauses_before_next_order_and_resume_reconciles_the_committed_remote_order(
) {
    let f = Fixture::batch(2).await;
    let connection = fault_connection(&f).await;
    sqlx::query("CREATE TRIGGER fail_result BEFORE UPDATE OF status ON batch_items WHEN NEW.status = 'succeeded' BEGIN SELECT RAISE(FAIL, 'disk unavailable'); END").execute(&connection).await.unwrap();
    let error = f
        .service
        .run(&f.batch_id, false, &f.events)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "DATABASE_ERROR");
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.items()
            .await
            .iter()
            .map(|item| item.status)
            .collect::<Vec<_>>(),
        vec![Status::Creating, Status::Queued]
    );
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
    sqlx::query("DROP TRIGGER fail_result")
        .execute(&connection)
        .await
        .unwrap();
    f.gateway
        .found
        .lock()
        .unwrap()
        .push_back(Ok(Some(order(1))));
    f.service
        .resume(&f.batch_id, false, &f.events)
        .await
        .unwrap();
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 2);
    assert!(f
        .items()
        .await
        .iter()
        .all(|item| item.status == Status::Succeeded && item.attempt_count == 1));
}

#[tokio::test]
async fn forced_audit_write_failure_rolls_back_claim_and_prevents_submission() {
    let f = Fixture::batch(1).await;
    let item = f.items().await.remove(0);
    f.repo.mark_creating(&item.id).await.unwrap();
    f.repo
        .mark_uncertain(
            &item.id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    let before = f.items().await;
    let connection = fault_connection(&f).await;
    sqlx::query("CREATE TRIGGER fail_audit BEFORE INSERT ON forced_retry_attempts BEGIN SELECT RAISE(FAIL, 'disk unavailable'); END").execute(&connection).await.unwrap();
    let error = f
        .service
        .force_retry_uncertain(&f.batch_id, &item.id, false, true, &f.events)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "DATABASE_ERROR");
    assert_eq!(f.items().await, before);
    assert!(f
        .repo
        .list_forced_retry_attempts(&f.batch_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn deleted_credentials_pause_without_claiming_or_submitting() {
    let f = Fixture::batch(1).await;
    f.repo.delete_store(&f.store_id).await.unwrap();
    assert_eq!(
        f.service
            .run(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "STORE_NOT_FOUND"
    );
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.items().await[0].attempt_count, 0);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn absent_crashed_order_stays_uncertain_while_resume_continues_only_queued_work() {
    let f = Fixture::batch(2).await;
    let first = f.items().await.remove(0);
    f.repo.mark_creating(&first.id).await.unwrap();
    f.service
        .resume(&f.batch_id, false, &f.events)
        .await
        .unwrap();
    let items = f.items().await;
    assert_eq!(items[0].status, Status::Uncertain);
    assert_eq!(items[0].attempt_count, 1);
    assert_eq!(items[1].status, Status::Succeeded);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.gateway.lookups.lock().unwrap().len(), 3);
    assert_eq!(
        f.gateway.inputs.lock().unwrap()[0]["sourceIdentifier"],
        items[1].source_identifier
    );
}

#[tokio::test]
async fn permission_failure_during_reconciliation_pauses_without_new_submissions() {
    let f = Fixture::batch(2).await;
    let first = f.items().await.remove(0);
    f.repo.mark_creating(&first.id).await.unwrap();
    f.gateway
        .found
        .lock()
        .unwrap()
        .push_back(Err(AppError::validation(
            "SHOPIFY_FORBIDDEN",
            "Missing read_orders",
        )));
    assert_eq!(
        f.service
            .resume(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "SHOPIFY_FORBIDDEN"
    );
    assert_eq!(f.items().await[0].status, Status::Uncertain);
    assert_eq!(f.items().await[1].status, Status::Queued);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn successful_gateway_results_are_redacted_before_persistence_and_progress() {
    let f = Fixture::batch(1).await;
    f.gateway
        .results
        .lock()
        .unwrap()
        .push_back(Ok(CreatedOrder {
            id: "gid://shopify/Order/1".into(),
            name: "#1 shpat_secret".into(),
        }));
    f.service.run(&f.batch_id, false, &f.events).await.unwrap();
    assert_eq!(
        f.items().await[0].shopify_order_name.as_deref(),
        Some("#1 [REDACTED]")
    );
    assert!(!serde_json::to_string(&*f.events.0.lock().unwrap())
        .unwrap()
        .contains("shpat_secret"));
}

#[tokio::test]
async fn failed_stop_database_write_still_prevents_later_submissions() {
    let f = Fixture::batch(2).await;
    let connection = fault_connection(&f).await;
    sqlx::query("CREATE TRIGGER fail_stop BEFORE UPDATE OF status ON batch_items WHEN NEW.status = 'stopped' BEGIN SELECT RAISE(FAIL, 'disk unavailable'); END").execute(&connection).await.unwrap();
    let gate = Arc::new(CreateGate::default());
    *f.gateway.gate.lock().unwrap() = Some(gate.clone());
    let service = f.service.clone();
    let batch_id = f.batch_id.clone();
    let running =
        tokio::spawn(async move { service.run(&batch_id, false, &Events::default()).await });
    gate.entered.notified().await;
    assert_eq!(
        f.service
            .request_stop(&f.batch_id, &f.events)
            .await
            .unwrap_err()
            .code(),
        "DATABASE_ERROR"
    );
    gate.release.notify_one();
    running.await.unwrap().unwrap();
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.repo.get_batch(&f.batch_id).await.unwrap().status,
        "paused"
    );
}

#[tokio::test]
async fn a_hung_reconciliation_query_is_bounded_and_remains_uncertain() {
    let f = Fixture::batch(1).await;
    let item = f.items().await.remove(0);
    f.repo.mark_creating(&item.id).await.unwrap();
    f.repo
        .mark_uncertain(
            &item.id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    let service = BatchService::new(f.repo.clone(), f.gateway.clone())
        .await
        .unwrap();
    let gate = Arc::new(CreateGate::default());
    *f.gateway.lookup_gate.lock().unwrap() = Some(gate.clone());
    let batch_id = f.batch_id.clone();
    let task = tokio::spawn(async move {
        service
            .reconcile_uncertain(&batch_id, &Events::default())
            .await
    });
    gate.entered.notified().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(!task.is_finished());
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    task.await.unwrap().unwrap();
    assert_eq!(f.items().await[0].status, Status::Uncertain);
    assert_eq!(f.gateway.lookups.lock().unwrap().len(), 1);
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn interrupted_execution_requires_resume_before_failed_or_forced_retry() {
    let f = Fixture::batch(3).await;
    let items = f.items().await;
    f.repo.mark_creating(&items[0].id).await.unwrap();
    f.repo
        .mark_failed(
            &items[0].id,
            &AppError::validation("SHOPIFY_USER_ERROR", "Rejected"),
        )
        .await
        .unwrap();
    f.repo.mark_creating(&items[1].id).await.unwrap();
    f.repo
        .mark_uncertain(
            &items[1].id,
            &AppError::validation("SHOPIFY_TIMEOUT", "Unknown"),
        )
        .await
        .unwrap();
    let gate = Arc::new(CreateGate::default());
    *f.gateway.gate.lock().unwrap() = Some(gate.clone());
    let service = f.service.clone();
    let batch_id = f.batch_id.clone();
    let running =
        tokio::spawn(async move { service.run(&batch_id, false, &Events::default()).await });
    gate.entered.notified().await;
    running.abort();
    assert!(running.await.unwrap_err().is_cancelled());
    assert_eq!(f.items().await[2].status, Status::Creating);
    assert_eq!(
        f.service
            .retry_failed(&f.batch_id, false, &f.events)
            .await
            .unwrap_err()
            .code(),
        "BATCH_RESUME_REQUIRED"
    );
    assert_eq!(
        f.service
            .force_retry_uncertain(&f.batch_id, &items[1].id, false, true, &f.events)
            .await
            .unwrap_err()
            .code(),
        "BATCH_RESUME_REQUIRED"
    );
    assert_eq!(f.gateway.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.items().await[0].status, Status::Failed);
}


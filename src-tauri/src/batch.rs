use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde::Serialize;

use crate::{
    domain::{BatchItem, BatchItemStatus, OrderTemplate},
    error::AppError,
    repository::{Repository, StoreCredentials},
    shopify::{CreateOrderInput, CreatedOrder, ShopifyGateway},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchProgress {
    pub batch_id: String,
    pub status: String,
    pub total: i64,
    pub succeeded: usize,
    pub failed: usize,
    pub uncertain: usize,
    pub queued: usize,
    pub stopped: usize,
    pub current_sequence: Option<i64>,
    pub items: Vec<BatchItem>,
}

pub trait ProgressSink: Send + Sync {
    fn emit(&self, progress: BatchProgress);
}

/// Monotonic time and waits are replaceable without shortening the production policy.
#[async_trait]
pub trait ReconciliationClock: Send + Sync {
    fn now(&self) -> Duration;
    async fn wait(&self, duration: Duration);
}

/// Testable boundary immediately before a forced retry may claim an uncertain item.
#[async_trait]
pub trait ForcedClaimGate: Send + Sync {
    async fn before_claim(&self);
}

struct ImmediateForcedClaimGate;

#[async_trait]
impl ForcedClaimGate for ImmediateForcedClaimGate {
    async fn before_claim(&self) {}
}

struct TokioClock(tokio::time::Instant);
#[async_trait]
impl ReconciliationClock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[derive(Clone)]
pub struct BatchService {
    repo: Repository,
    gateway: Arc<dyn ShopifyGateway>,
    clock: Arc<dyn ReconciliationClock>,
    active: Arc<Mutex<HashMap<String, Arc<BatchActivity>>>>,
    forced_claim_gate: Arc<dyn ForcedClaimGate>,
}

#[derive(Default)]
struct BatchActivity {
    stop_requested: AtomicBool,
    forced_claim: tokio::sync::Mutex<()>,
}

struct ActiveBatch {
    batch_id: String,
    active: Arc<Mutex<HashMap<String, Arc<BatchActivity>>>>,
    activity: Arc<BatchActivity>,
}
impl Drop for ActiveBatch {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.batch_id);
    }
}

impl BatchService {
    /// Construct once at application startup; clone this service for concurrent commands.
    /// Recovery only changes local state. It never starts requests without a user action.
    pub async fn new(repo: Repository, gateway: Arc<dyn ShopifyGateway>) -> Result<Self, AppError> {
        repo.recover_interrupted(None).await?;
        Ok(Self {
            repo,
            gateway,
            clock: Arc::new(TokioClock(tokio::time::Instant::now())),
            active: Arc::new(Mutex::new(HashMap::new())),
            forced_claim_gate: Arc::new(ImmediateForcedClaimGate),
        })
    }

    pub fn with_reconciliation_clock(mut self, clock: Arc<dyn ReconciliationClock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_forced_claim_gate(mut self, gate: Arc<dyn ForcedClaimGate>) -> Self {
        self.forced_claim_gate = gate;
        self
    }

    pub async fn run(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let _active = self.acquire(batch_id)?;
        if self.repo.get_batch(batch_id).await?.status != "pending" {
            return Err(AppError::validation(
                "BATCH_RESUME_REQUIRED",
                "请明确选择继续或重试批次",
            ));
        }
        self.validate_submission(batch_id, paid_confirmed).await?;
        self.run_pausing_on_error(batch_id, paid_confirmed, progress, None)
            .await
    }

    async fn run_pausing_on_error(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
        progress: &dyn ProgressSink,
        selected: Option<&[String]>,
    ) -> Result<(), AppError> {
        let result = self
            .execute(batch_id, paid_confirmed, progress, selected)
            .await;
        self.finish_operation(batch_id, progress, result).await
    }

    async fn finish_operation(
        &self,
        batch_id: &str,
        progress: &dyn ProgressSink,
        result: Result<(), AppError>,
    ) -> Result<(), AppError> {
        if result.is_err() {
            // If storage itself failed, this may also fail. Stop immediately regardless.
            let _ = self.repo.set_batch_status(batch_id, "paused").await;
            let _ = self.emit(batch_id, progress).await;
        }
        result
    }

    pub async fn resume(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let _active = self.acquire(batch_id)?;
        self.validate_submission(batch_id, paid_confirmed).await?;
        let preparation = async {
            self.repo.recover_interrupted(Some(batch_id)).await?;
            self.repo.resume_stopped_items(batch_id).await
        }
        .await;
        self.finish_operation(batch_id, progress, preparation)
            .await?;
        self.run_pausing_on_error(batch_id, paid_confirmed, progress, None)
            .await
    }

    pub async fn retry_failed(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let _active = self.acquire(batch_id)?;
        self.require_recovered_batch(batch_id).await?;
        self.validate_submission(batch_id, paid_confirmed).await?;
        let selected = match self.repo.requeue_failed_items(batch_id).await {
            Ok(selected) => selected,
            Err(error) => return self.finish_operation(batch_id, progress, Err(error)).await,
        };
        if selected.is_empty() {
            return self.emit(batch_id, progress).await;
        }
        self.run_pausing_on_error(batch_id, paid_confirmed, progress, Some(&selected))
            .await
    }

    pub async fn request_stop(
        &self,
        batch_id: &str,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let activity = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(batch_id)
            .cloned();
        // Linearize stop against a forced claim and keep the in-memory intent even
        // when the durable write fails.
        let result = if let Some(activity) = activity {
            let _claim = activity.forced_claim.lock().await;
            activity.stop_requested.store(true, Ordering::SeqCst);
            self.repo.stop_batch(batch_id).await
        } else {
            self.repo.stop_batch(batch_id).await
        };
        self.finish_operation(batch_id, progress, result).await?;
        self.emit(batch_id, progress).await
    }

    pub async fn reconcile_uncertain(
        &self,
        batch_id: &str,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let _active = self.acquire(batch_id)?;
        let result = async {
            self.repo.recover_interrupted(Some(batch_id)).await?;
            let job = self.repo.get_batch(batch_id).await?;
            let store = self
                .repo
                .store_credentials(
                    job.store_id
                        .as_deref()
                        .ok_or_else(|| AppError::validation("STORE_NOT_FOUND", "未找到店铺"))?,
                )
                .await?;
            for item in self
                .repo
                .list_batch_items(batch_id)
                .await?
                .iter()
                .filter(|item| item.status == BatchItemStatus::Uncertain)
            {
                self.reconcile_item(item, &store).await?;
                self.emit(batch_id, progress).await?;
            }
            self.finish_batch(batch_id, progress).await
        }
        .await;
        self.finish_operation(batch_id, progress, result).await
    }

    pub async fn force_retry_uncertain(
        &self,
        batch_id: &str,
        item_id: &str,
        paid_confirmed: bool,
        risk_confirmed: bool,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let _active = self.acquire(batch_id)?;
        if !risk_confirmed {
            return Err(AppError::validation(
                "DUPLICATE_RISK_CONFIRMATION_REQUIRED",
                "强制重试可能创建重复订单，必须明确确认此风险",
            ));
        }
        self.require_recovered_batch(batch_id).await?;
        let item = self
            .repo
            .list_batch_items(batch_id)
            .await?
            .into_iter()
            .find(|item| item.id == item_id && item.status == BatchItemStatus::Uncertain)
            .ok_or_else(|| {
                AppError::validation(
                    "INVALID_ITEM_TRANSITION",
                    "仅此批次中结果待确认的项目允许强制重试",
                )
            })?;
        let job = self.repo.get_batch(batch_id).await?;
        let template: OrderTemplate = serde_json::from_str(&job.order_template_json)
            .map_err(|_| AppError::validation("INVALID_ORDER_TEMPLATE", "批次订单模板无效"))?;
        let input =
            CreateOrderInput::from_template(template, &item.source_identifier, paid_confirmed)?;
        let result = async {
            let store = self
                .repo
                .store_credentials(
                    job.store_id
                        .as_deref()
                        .ok_or_else(|| AppError::validation("STORE_NOT_FOUND", "未找到店铺"))?,
                )
                .await?;
            self.forced_claim_gate.before_claim().await;
            {
                let _claim = _active.activity.forced_claim.lock().await;
                if _active.activity.stop_requested.load(Ordering::SeqCst)
                    || matches!(
                        self.repo.get_batch(batch_id).await?.status.as_str(),
                        "stopping" | "paused"
                    )
                {
                    return Err(AppError::validation(
                        "BATCH_STOPPED",
                        "批次已停止，未执行强制重试",
                    ));
                }
                self.repo.claim_forced_attempt(batch_id, item_id).await?;
            }
            self.submit(&item, &store, input, progress).await?;
            self.finish_batch(batch_id, progress).await
        }
        .await;
        self.finish_operation(batch_id, progress, result).await
    }

    fn acquire(&self, batch_id: &str) -> Result<ActiveBatch, AppError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.contains_key(batch_id) {
            return Err(AppError::validation(
                "BATCH_BUSY",
                "此批次正在执行，请等待当前操作完成",
            ));
        }
        let activity = Arc::new(BatchActivity::default());
        active.insert(batch_id.into(), activity.clone());
        Ok(ActiveBatch {
            batch_id: batch_id.into(),
            active: self.active.clone(),
            activity,
        })
    }

    fn stop_requested(&self, batch_id: &str) -> bool {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(batch_id)
            .is_some_and(|activity| activity.stop_requested.load(Ordering::SeqCst))
    }

    async fn require_recovered_batch(&self, batch_id: &str) -> Result<(), AppError> {
        if self
            .repo
            .list_batch_items(batch_id)
            .await?
            .iter()
            .any(|item| item.status == BatchItemStatus::Creating)
        {
            return Err(AppError::validation(
                "BATCH_RESUME_REQUIRED",
                "此批次有被中断的创建请求，请先继续批次以核对结果",
            ));
        }
        Ok(())
    }

    async fn validate_submission(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
    ) -> Result<(), AppError> {
        let job = self.repo.get_batch(batch_id).await?;
        let template: OrderTemplate = serde_json::from_str(&job.order_template_json)
            .map_err(|_| AppError::validation("INVALID_ORDER_TEMPLATE", "批次订单模板无效"))?;
        CreateOrderInput::from_template(
            template,
            &format!("orderpilot/{batch_id}/1"),
            paid_confirmed,
        )?;
        Ok(())
    }

    async fn execute(
        &self,
        batch_id: &str,
        paid_confirmed: bool,
        progress: &dyn ProgressSink,
        selected: Option<&[String]>,
    ) -> Result<(), AppError> {
        let job = self.repo.get_batch(batch_id).await?;
        let template: OrderTemplate = serde_json::from_str(&job.order_template_json)
            .map_err(|_| AppError::validation("INVALID_ORDER_TEMPLATE", "批次订单模板无效"))?;
        let store = self
            .repo
            .store_credentials(
                job.store_id
                    .as_deref()
                    .ok_or_else(|| AppError::validation("STORE_NOT_FOUND", "未找到店铺"))?,
            )
            .await?;
        self.repo.set_batch_status(batch_id, "running").await?;
        for item in self
            .repo
            .list_batch_items(batch_id)
            .await?
            .iter()
            .filter(|item| selected.is_none() && item.status == BatchItemStatus::Uncertain)
        {
            self.reconcile_item(item, &store).await?;
            self.emit(batch_id, progress).await?;
        }
        for item in self
            .repo
            .list_batch_items(batch_id)
            .await?
            .into_iter()
            .filter(|item| {
                item.status == BatchItemStatus::Queued
                    && selected.is_none_or(|ids| ids.contains(&item.id))
            })
        {
            if self.stop_requested(batch_id) {
                break;
            }
            let input = CreateOrderInput::from_template(
                template.clone(),
                &item.source_identifier,
                paid_confirmed,
            )?;
            if !self.repo.claim_for_execution(&item.id).await? {
                break;
            }
            self.submit(&item, &store, input, progress).await?;
        }
        self.finish_batch(batch_id, progress).await
    }

    async fn submit(
        &self,
        item: &BatchItem,
        store: &StoreCredentials,
        input: CreateOrderInput,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        self.emit(&item.batch_id, progress).await?;
        match self.gateway.create_order(store, input).await {
            Ok(order) => {
                let order = sanitize_order(order, store);
                self.repo
                    .mark_succeeded(&item.id, &order.id, &order.name)
                    .await?;
            }
            Err(error) => {
                let error = error.redacted(store.access_token.expose_secret());
                if is_access_error(&error) {
                    if item.status == BatchItemStatus::Uncertain {
                        // Rejection of a forced attempt says nothing about the original attempt.
                        self.repo.mark_uncertain(&item.id, &error).await?;
                    } else {
                        self.repo.requeue_rejected_item(&item.id, &error).await?;
                    }
                    return Err(error);
                }
                if item.status != BatchItemStatus::Uncertain
                    && matches!(error.code(), "SHOPIFY_USER_ERROR" | "SHOPIFY_RATE_LIMITED")
                {
                    self.repo.mark_failed(&item.id, &error).await?;
                } else {
                    self.repo.mark_uncertain(&item.id, &error).await?;
                    self.emit(&item.batch_id, progress).await?;
                    self.reconcile_item(item, store).await?;
                }
            }
        }
        self.emit(&item.batch_id, progress).await
    }

    async fn finish_batch(
        &self,
        batch_id: &str,
        progress: &dyn ProgressSink,
    ) -> Result<(), AppError> {
        let items = self.repo.list_batch_items(batch_id).await?;
        let status = if items.iter().any(|item| {
            matches!(
                item.status,
                BatchItemStatus::Queued | BatchItemStatus::Stopped | BatchItemStatus::Creating
            )
        }) {
            "paused"
        } else if items
            .iter()
            .any(|item| item.status != BatchItemStatus::Succeeded)
        {
            "completed_with_errors"
        } else {
            "completed"
        };
        self.repo.set_batch_status(batch_id, status).await?;
        self.emit(batch_id, progress).await
    }

    async fn reconcile_item(
        &self,
        item: &BatchItem,
        store: &StoreCredentials,
    ) -> Result<(), AppError> {
        let start = self.clock.now();
        for attempt in 0..3 {
            let elapsed = self.clock.now().saturating_sub(start);
            let scheduled = Duration::from_secs(attempt * 10);
            if elapsed < scheduled {
                self.clock.wait(scheduled - elapsed).await;
            }
            let remaining =
                Duration::from_secs(30).saturating_sub(self.clock.now().saturating_sub(start));
            if remaining.is_zero() {
                break;
            }
            let result = tokio::time::timeout(
                remaining.min(Duration::from_secs(10)),
                self.gateway
                    .find_order_by_source(store, &item.source_identifier),
            )
            .await;
            match result {
                Ok(Ok(Some(order))) => {
                    let order = sanitize_order(order, store);
                    self.repo
                        .record_reconciled_order(&item.id, &order.id, &order.name)
                        .await?;
                    break;
                }
                Ok(Ok(None)) => {}
                Ok(Err(error)) if is_access_error(&error) => {
                    return Err(error.redacted(store.access_token.expose_secret()))
                }
                // Failed queries cannot prove absence. Leave the item uncertain.
                Ok(Err(_)) | Err(_) => break,
            }
        }
        Ok(())
    }

    async fn emit(&self, batch_id: &str, progress: &dyn ProgressSink) -> Result<(), AppError> {
        let job = self.repo.get_batch(batch_id).await?;
        let items = self.repo.list_batch_items(batch_id).await?;
        let count = |status| items.iter().filter(|item| item.status == status).count();
        progress.emit(BatchProgress {
            batch_id: batch_id.into(),
            status: job.status,
            total: job.requested_count,
            succeeded: count(BatchItemStatus::Succeeded),
            failed: count(BatchItemStatus::Failed),
            uncertain: count(BatchItemStatus::Uncertain),
            queued: count(BatchItemStatus::Queued),
            stopped: count(BatchItemStatus::Stopped),
            current_sequence: items
                .iter()
                .find(|item| item.status == BatchItemStatus::Creating)
                .map(|item| item.sequence_number),
            items,
        });
        Ok(())
    }
}

fn is_access_error(error: &AppError) -> bool {
    matches!(
        error.code(),
        "SHOPIFY_UNAUTHORIZED"
            | "SHOPIFY_FORBIDDEN"
            | "SHOPIFY_CREDENTIALS"
            | "CUSTOMER_DATA_RESTRICTED"
            | "INVALID_SHOP_DOMAIN"
    )
}

fn sanitize_order(mut order: CreatedOrder, store: &StoreCredentials) -> CreatedOrder {
    let token = store.access_token.expose_secret();
    if !token.is_empty() {
        order.id = order.id.replace(token, "[REDACTED]");
        order.name = order.name.replace(token, "[REDACTED]");
    }
    order
}


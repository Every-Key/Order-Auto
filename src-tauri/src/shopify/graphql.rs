use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use reqwest::{
    header::{HeaderValue, CONTENT_TYPE, RETRY_AFTER},
    Client, Url,
};
use secrecy::ExposeSecret;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::{CustomerMode, FinancialStatus, MailingAddress, OrderTemplate};
use crate::{domain::normalize_shop_domain, error::AppError, repository::StoreCredentials};

/// Validated mutation input. Private fields prevent bypassing paid confirmation.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrderInput {
    line_items: Vec<CreateLineItem>,
    financial_status: FinancialStatus,
    source_identifier: String,
    custom_attributes: Vec<CustomAttribute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    customer: Option<CreateCustomer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shipping_address: Option<CreateShippingAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedOrder {
    pub id: String,
    pub name: String,
}

pub(crate) const CREATE_ORDER_MUTATION: &str = "mutation CreateOrder($order: OrderCreateOrderInput!) { orderCreate(order: $order) { order { id name } userErrors { field message code } } }";

pub(crate) const FIND_ORDER_QUERY: &str = "query FindOrderBySource($query: String!) { orders(first: 2, query: $query) { nodes { id name sourceIdentifier } pageInfo { hasNextPage } } }";

#[derive(Deserialize)]
pub(crate) struct FindOrderData {
    orders: FoundOrders,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundOrders {
    nodes: Vec<FoundOrder>,
    page_info: OrderPageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderPageInfo {
    has_next_page: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundOrder {
    #[serde(flatten)]
    order: CreatedOrder,
    source_identifier: Option<String>,
}

impl FindOrderData {
    pub(crate) fn into_order(self, source: &str) -> Result<Option<CreatedOrder>, AppError> {
        if self.orders.page_info.has_next_page || self.orders.nodes.len() > 1 {
            return Err(AppError::validation(
                "SHOPIFY_SOURCE_AMBIGUOUS",
                "发现多个来源标识相同的订单，请在 Shopify 中核对",
            ));
        }
        self.orders
            .nodes
            .into_iter()
            .next()
            .map(|node| {
                if node.source_identifier.as_deref() != Some(source)
                    || !crate::domain::valid_shopify_gid(&node.order.id, "Order")
                    || node.order.name.trim().is_empty()
                {
                    Err(invalid_response())
                } else {
                    Ok(node.order)
                }
            })
            .transpose()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOrderData {
    pub order_create: CreateOrderPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateOrderPayload {
    pub order: Option<CreatedOrder>,
    user_errors: Vec<OrderUserError>,
}

#[derive(Deserialize)]
struct OrderUserError {
    field: Option<Vec<String>>,
    message: String,
    code: Option<String>,
}

impl CreateOrderData {
    pub(crate) fn into_order(self) -> Result<CreatedOrder, AppError> {
        if !self.order_create.user_errors.is_empty() {
            // A returned order may already exist remotely despite accompanying errors.
            // Only an explicit no-order response is safe for ordinary failed-item retry.
            let code = if self.order_create.order.is_some() {
                "SHOPIFY_ORDER_OUTCOME_UNCERTAIN"
            } else {
                "SHOPIFY_USER_ERROR"
            };
            let messages = self
                .order_create
                .user_errors
                .into_iter()
                .map(|error| {
                    let field = error.field.unwrap_or_default().join(".");
                    let code = error.code.unwrap_or_else(|| "UNKNOWN".into());
                    format!("[{code}] {field}: {}", error.message)
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(AppError::validation(code, messages));
        }
        self.order_create
            .order
            .filter(|order| {
                crate::domain::valid_shopify_gid(&order.id, "Order")
                    && !order.name.trim().is_empty()
            })
            .ok_or_else(invalid_response)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
enum CreateCustomer {
    ToAssociate { id: String },
    ToUpsert(CustomerDetails),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CustomerDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateShippingAddress {
    #[serde(flatten)]
    address: MailingAddress,
    #[serde(skip_serializing_if = "Option::is_none")]
    first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateLineItem {
    variant_id: String,
    quantity: u32,
}

#[derive(Debug, Clone, Serialize)]
struct CustomAttribute {
    key: String,
    value: String,
}

impl CreateOrderInput {
    pub fn from_template(
        template: OrderTemplate,
        source: &str,
        paid_confirmed: bool,
    ) -> Result<Self, AppError> {
        if template.financial_status == FinancialStatus::Paid && !paid_confirmed {
            return Err(AppError::validation(
                "PAID_CONFIRMATION_REQUIRED",
                "标记已付款前必须明确确认已通过其他渠道收款",
            ));
        }
        template.validate()?;
        let batch_id = source_batch_id(source)?;
        let mut input = Self {
            line_items: vec![CreateLineItem {
                variant_id: template.variant_id,
                quantity: template.quantity,
            }],
            financial_status: template.financial_status,
            source_identifier: source.into(),
            custom_attributes: vec![CustomAttribute {
                key: "OrderPilot-Batch".into(),
                value: batch_id.into(),
            }],
            customer: None,
            email: None,
            phone: None,
            shipping_address: None,
        };
        match template.customer {
            CustomerMode::None => {}
            CustomerMode::Existing { customer_id } => {
                input.customer = Some(CreateCustomer::ToAssociate { id: customer_id });
            }
            CustomerMode::Manual { customer } => {
                let details = CustomerDetails {
                    email: filled(customer.email),
                    first_name: filled(customer.first_name),
                    last_name: filled(customer.last_name),
                    phone: filled(customer.phone),
                };
                input.email = details.email.clone();
                input.phone = details.phone.clone();
                input.shipping_address =
                    customer
                        .shipping_address
                        .and_then(filled_address)
                        .map(|address| CreateShippingAddress {
                            address,
                            first_name: details.first_name.clone(),
                            last_name: details.last_name.clone(),
                            phone: details.phone.clone(),
                        });
                if details.email.is_some()
                    || details.first_name.is_some()
                    || details.last_name.is_some()
                    || details.phone.is_some()
                {
                    input.customer = Some(CreateCustomer::ToUpsert(details));
                }
            }
        }
        Ok(input)
    }
}

pub(crate) fn source_batch_id(source: &str) -> Result<&str, AppError> {
    source
        .strip_prefix("orderpilot/")
        .and_then(|suffix| suffix.split_once('/'))
        .filter(|(batch, sequence)| {
            uuid::Uuid::parse_str(batch).is_ok_and(|uuid| uuid.to_string() == *batch)
                && sequence.parse::<u16>().is_ok_and(|number| {
                    (1..=100).contains(&number) && number.to_string() == *sequence
                })
        })
        .map(|(batch, _)| batch)
        .ok_or_else(|| AppError::validation("INVALID_SOURCE_IDENTIFIER", "批次项来源标识无效"))
}

fn filled(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn filled_address(address: MailingAddress) -> Option<MailingAddress> {
    let address = MailingAddress {
        address1: filled(address.address1),
        address2: filled(address.address2),
        city: filled(address.city),
        province: filled(address.province),
        country: filled(address.country),
        zip: filled(address.zip),
    };
    (address != MailingAddress::default()).then_some(address)
}

pub const GRAPHQL_PATH: &str = "/admin/api/2026-07/graphql.json";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: usize = 3;

/// Wait seam makes HTTP retry timing deterministic in local integration tests.
#[async_trait]
pub trait RetryWaiter: Send + Sync {
    async fn wait(&self, delay: Duration);
}

struct TokioWaiter;

#[async_trait]
impl RetryWaiter for TokioWaiter {
    async fn wait(&self, delay: Duration) {
        tokio::time::sleep(delay).await;
    }
}

pub struct ShopifyHttpClient {
    client: Client,
    base_url_override: Option<Url>,
    retry_waiter: Arc<dyn RetryWaiter>,
}

impl ShopifyHttpClient {
    /// Pass None for Shopify production; an explicit origin supports local mock servers.
    pub fn new(base_url_override: Option<String>) -> Result<Self, AppError> {
        let base_url_override = base_url_override
            .map(|base| {
                Url::parse(&base)
                    .map_err(|_| AppError::validation("SHOPIFY_CONFIG", "Shopify 地址配置无效"))
            })
            .transpose()?;
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| AppError::validation("SHOPIFY_CONFIG", "无法初始化 Shopify 连接"))?;
        Ok(Self {
            client,
            base_url_override,
            retry_waiter: Arc::new(TokioWaiter),
        })
    }

    pub fn with_retry_waiter(mut self, waiter: Arc<dyn RetryWaiter>) -> Self {
        self.retry_waiter = waiter;
        self
    }

    /// Single request pipeline for all queries and mutations. Credentials stay in Rust.
    pub async fn execute<T: DeserializeOwned>(
        &self,
        store: &StoreCredentials,
        query: &str,
        variables: &Value,
    ) -> Result<T, AppError> {
        self.execute_attempts(store, query, variables, MAX_ATTEMPTS, false)
            .await
    }

    pub(crate) async fn execute_once<T: DeserializeOwned>(
        &self,
        store: &StoreCredentials,
        query: &str,
        variables: &Value,
    ) -> Result<T, AppError> {
        self.execute_attempts(store, query, variables, 1, false)
            .await
    }

    pub(crate) async fn execute_order_mutation<T: DeserializeOwned>(
        &self,
        store: &StoreCredentials,
        variables: &Value,
    ) -> Result<T, AppError> {
        self.execute_attempts(store, CREATE_ORDER_MUTATION, variables, MAX_ATTEMPTS, true)
            .await
    }

    async fn execute_attempts<T: DeserializeOwned>(
        &self,
        store: &StoreCredentials,
        query: &str,
        variables: &Value,
        attempts: usize,
        mutation: bool,
    ) -> Result<T, AppError> {
        let domain = normalize_shop_domain(&store.shop_domain)?;
        let base = match &self.base_url_override {
            Some(base) => base.clone(),
            None => Url::parse(&format!("https://{domain}")).map_err(|_| invalid_response())?,
        };
        let url = base.join(GRAPHQL_PATH).map_err(|_| invalid_response())?;
        let mut token = HeaderValue::from_str(store.access_token.expose_secret())
            .map_err(|_| AppError::validation("SHOPIFY_CREDENTIALS", "Shopify 访问令牌无效"))?;
        token.set_sensitive(true);
        for attempt in 0..attempts {
            let response = self
                .client
                .post(url.clone())
                .header(CONTENT_TYPE, "application/json")
                .header("X-Shopify-Access-Token", token.clone())
                .json(&json!({ "query": query, "variables": variables }))
                .send()
                .await
                .map_err(network_error)?;
            let status = response.status();
            let header_delay = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|header| header.to_str().ok())
                .and_then(retry_after_delay);
            let retryable = retryable_http(status.as_u16(), mutation);
            if retryable && attempt + 1 < attempts {
                let body: Value = response.json().await.unwrap_or(Value::Null);
                let delay = match (header_delay, throttle_delay(&body)) {
                    (Some(a), Some(b)) => a.max(b),
                    (Some(delay), None) | (None, Some(delay)) => delay,
                    (None, None) => Duration::from_secs(1 << attempt),
                };
                self.retry_waiter.wait(delay).await;
                continue;
            }
            if !status.is_success() {
                if mutation && status.is_server_error() {
                    return Err(AppError::validation(
                        "SHOPIFY_SERVER_ERROR",
                        "Shopify 返回服务错误，订单结果不确定，请核对",
                    ));
                }
                return Err(http_error(status.as_u16()));
            }
            let body = response.bytes().await.map_err(network_error)?;
            let envelope: Envelope =
                serde_json::from_slice(&body).map_err(|_| invalid_response())?;
            if let Some(errors) = envelope
                .errors
                .as_deref()
                .filter(|errors| !errors.is_empty())
            {
                if mutation && is_pre_execution_throttle(&envelope.data, errors) {
                    if attempt + 1 < attempts {
                        let body = json!({"extensions": envelope.extensions});
                        let delay = header_delay.unwrap_or_default().max(
                            throttle_delay(&body).unwrap_or(Duration::from_secs(1 << attempt)),
                        );
                        self.retry_waiter.wait(delay).await;
                        continue;
                    }
                    return Err(http_error(429));
                }
                return Err(graphql_response_error(errors, &envelope.data, mutation));
            }
            let mut data = envelope.data.ok_or_else(invalid_response)?;
            redact_token(&mut data, store.access_token.expose_secret());
            return serde_json::from_value(data).map_err(|_| invalid_response());
        }
        unreachable!("the final attempt returns its result")
    }
}

fn retryable_http(status: u16, mutation: bool) -> bool {
    status == 429 || (!mutation && (500..=599).contains(&status))
}

#[derive(Deserialize)]
struct Envelope {
    data: Option<Value>,
    errors: Option<Vec<Value>>,
    #[serde(default)]
    extensions: Value,
}

fn is_pre_execution_throttle(data: &Option<Value>, errors: &[Value]) -> bool {
    data.is_none()
        && !errors.is_empty()
        && errors.iter().all(|error| {
            error.pointer("/extensions/code").and_then(Value::as_str) == Some("THROTTLED")
        })
}

fn invalid_response() -> AppError {
    AppError::validation("SHOPIFY_INVALID_RESPONSE", "Shopify 返回了无效响应")
}

fn network_error(error: reqwest::Error) -> AppError {
    if error.is_timeout() {
        AppError::validation("SHOPIFY_TIMEOUT", "Shopify 请求超时，请核对远端结果")
    } else {
        AppError::validation("SHOPIFY_NETWORK", "Shopify 网络请求失败，请核对远端结果")
    }
}

fn http_error(status: u16) -> AppError {
    match status {
        401 => AppError::validation("SHOPIFY_UNAUTHORIZED", "Shopify 访问令牌无效或已失效"),
        403 => AppError::validation(
            "SHOPIFY_FORBIDDEN",
            "Shopify 拒绝访问，请检查访问令牌和所需权限",
        ),
        429 => AppError::transient("SHOPIFY_RATE_LIMITED", "Shopify 请求受限，请稍后重试"),
        500..=599 => {
            AppError::transient("SHOPIFY_SERVER_ERROR", "Shopify 服务暂时不可用，请稍后重试")
        }
        _ => AppError::validation("SHOPIFY_HTTP", "Shopify HTTP 请求失败"),
    }
}

fn graphql_error(errors: &[Value]) -> AppError {
    if errors.iter().any(is_protected_customer_data_denial) {
        AppError::validation(
            "CUSTOMER_DATA_RESTRICTED",
            "此应用尚未获准访问 Shopify 受保护的客户数据",
        )
    } else if errors.iter().any(|error| {
        error.pointer("/extensions/code").and_then(Value::as_str) == Some("ACCESS_DENIED")
    }) {
        AppError::validation("SHOPIFY_FORBIDDEN", "Shopify 拒绝访问，请检查所需权限")
    } else {
        AppError::validation(
            "SHOPIFY_GRAPHQL",
            "Shopify GraphQL 请求失败，请检查权限和请求参数",
        )
    }
}

fn graphql_response_error(errors: &[Value], data: &Option<Value>, mutation: bool) -> AppError {
    // A partial mutation payload can mean the order exists even if a result field is denied.
    // Mixed access/internal errors likewise cannot establish pre-execution rejection.
    if mutation
        && (data
            .as_ref()
            .and_then(|value| value.get("orderCreate"))
            .is_some_and(|value| !value.is_null())
            || errors.iter().any(|error| {
                error.pointer("/extensions/code").and_then(Value::as_str) != Some("ACCESS_DENIED")
                    && !is_protected_customer_data_denial(error)
            }))
    {
        AppError::validation("SHOPIFY_GRAPHQL", "Shopify 返回了部分创建结果，请核对订单")
    } else {
        graphql_error(errors)
    }
}

fn is_protected_customer_data_denial(error: &Value) -> bool {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let documentation = error
        .pointer("/extensions/documentation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let code = error
        .pointer("/extensions/code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mentions_protected_data = message.contains("protected customer data")
        || message.contains("protected-customer-data")
        || message.contains("customer_data")
        || documentation.contains("protected-customer-data")
        || documentation.contains("customer_data");
    let is_denial = code == "ACCESS_DENIED"
        || message.contains("access denied")
        || message.contains("not approved");
    mentions_protected_data && is_denial
}

fn retry_after_delay(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (date.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or_default(),
    )
}

fn throttle_delay(body: &Value) -> Option<Duration> {
    let cost = body.get("extensions")?.get("cost")?;
    let requested = cost.get("requestedQueryCost")?.as_f64()?;
    let throttle = cost.get("throttleStatus")?;
    let available = throttle.get("currentlyAvailable")?.as_f64()?;
    let restore = throttle.get("restoreRate")?.as_f64()?;
    if requested < 0.0 || available < 0.0 || restore <= 0.0 {
        return None;
    }
    Duration::try_from_secs_f64(((requested - available) / restore).max(0.0)).ok()
}

// Responses are untrusted: even successful data or mutation validation messages may echo
// a credential. Redact before handing typed values to callers that can format/serialize them.
fn redact_token(value: &mut Value, token: &str) {
    if token.is_empty() {
        return;
    }
    match value {
        Value::String(text) => *text = text.replace(token, "[REDACTED]"),
        Value::Array(items) => items.iter_mut().for_each(|item| redact_token(item, token)),
        Value::Object(fields) => {
            *fields = std::mem::take(fields)
                .into_iter()
                .map(|(key, mut value)| {
                    redact_token(&mut value, token);
                    (key.replace(token, "[REDACTED]"), value)
                })
                .collect();
        }
        _ => {}
    }
}

#[cfg(test)]
mod order_response_tests {
    use super::*;

    #[test]
    fn order_mutation_http_retry_policy_excludes_every_ambiguous_failure() {
        assert!(retryable_http(429, true));
        for status in [400, 401, 403, 408, 500, 502, 503, 599] {
            assert!(
                !retryable_http(status, true),
                "must not replay orderCreate after HTTP {status}"
            );
        }
        assert!(retryable_http(500, false));
    }

    #[test]
    fn mutation_throttling_is_retryable_only_when_explicitly_before_execution() {
        let throttled = vec![json!({"extensions": {"code": "THROTTLED"}})];
        assert!(is_pre_execution_throttle(&None, &throttled));
        assert!(!is_pre_execution_throttle(
            &Some(json!({"orderCreate": {"order": {"id": "42"}}})),
            &throttled
        ));
        assert!(!is_pre_execution_throttle(&None, &[]));
        assert!(!is_pre_execution_throttle(
            &None,
            &[
                throttled[0].clone(),
                json!({"extensions": {"code": "INTERNAL_SERVER_ERROR"}})
            ]
        ));
    }

    #[test]
    fn graphql_access_denial_is_distinct_from_ambiguous_execution_errors() {
        assert_eq!(
            graphql_error(&[
                json!({"message": "Access denied", "extensions": {"code": "ACCESS_DENIED"}})
            ])
            .code(),
            "SHOPIFY_FORBIDDEN"
        );
        assert_eq!(
            graphql_error(&[json!({"extensions": {"code": "INTERNAL_SERVER_ERROR"}})]).code(),
            "SHOPIFY_GRAPHQL"
        );
        let partial = Some(json!({"orderCreate": {"order": {"id": "gid://shopify/Order/42"}}}));
        assert_eq!(
            graphql_response_error(
                &[json!({"extensions": {"code": "ACCESS_DENIED"}})],
                &partial,
                true
            )
            .code(),
            "SHOPIFY_GRAPHQL"
        );
        assert_eq!(
            graphql_response_error(
                &[
                    json!({"extensions": {"code": "ACCESS_DENIED"}}),
                    json!({"extensions": {"code": "INTERNAL_SERVER_ERROR"}}),
                ],
                &None,
                true
            )
            .code(),
            "SHOPIFY_GRAPHQL"
        );
    }

    #[test]
    fn source_lookup_rejects_mismatches_invalid_results_and_multiple_matches() {
        let source = "orderpilot/00000000-0000-4000-8000-000000000001/1";
        let valid =
            json!({"id": "gid://shopify/Order/42", "name": "#42", "sourceIdentifier": source});
        let parse = |nodes: Value, more: bool| {
            serde_json::from_value::<FindOrderData>(
                json!({"orders": {"nodes": nodes, "pageInfo": {"hasNextPage": more}}}),
            )
            .unwrap()
            .into_order(source)
        };
        assert_eq!(parse(json!([]), false).unwrap(), None);
        assert_eq!(
            parse(json!([valid.clone()]), false).unwrap().unwrap().name,
            "#42"
        );
        assert_eq!(
            parse(json!([valid.clone(), valid.clone()]), false)
                .unwrap_err()
                .code(),
            "SHOPIFY_SOURCE_AMBIGUOUS"
        );
        assert_eq!(
            parse(json!([valid.clone()]), true).unwrap_err().code(),
            "SHOPIFY_SOURCE_AMBIGUOUS"
        );
        for (field, value) in [
            ("sourceIdentifier", json!("other")),
            ("sourceIdentifier", Value::Null),
            ("id", json!("")),
            ("name", json!(" ")),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert_eq!(
                parse(json!([invalid]), false).unwrap_err().code(),
                "SHOPIFY_INVALID_RESPONSE"
            );
        }
    }

    #[test]
    fn order_success_requires_both_a_gid_and_nonempty_name() {
        let valid: CreateOrderData = serde_json::from_value(json!({"orderCreate": {
            "order": {"id": "gid://shopify/Order/42", "name": "#1042"}, "userErrors": []
        }}))
        .unwrap();
        assert_eq!(
            valid.into_order().unwrap(),
            CreatedOrder {
                id: "gid://shopify/Order/42".into(),
                name: "#1042".into()
            }
        );
        for order in [
            Value::Null,
            json!({"id": "", "name": "#1042"}),
            json!({"id": "gid://shopify/Order/42", "name": " "}),
        ] {
            let data: CreateOrderData =
                serde_json::from_value(json!({"orderCreate": {"order": order, "userErrors": []}}))
                    .unwrap();
            assert_eq!(
                data.into_order().unwrap_err().code(),
                "SHOPIFY_INVALID_RESPONSE"
            );
        }
    }

    #[test]
    fn definite_order_rejection_preserves_user_error_codes_and_messages() {
        let data: CreateOrderData = serde_json::from_value(json!({"orderCreate": {
            "order": null,
            "userErrors": [
                {"field": ["order", "shippingAddress"], "message": "Invalid address", "code": "INVALID"},
                {"field": null, "message": "Variant unavailable", "code": "VARIANT_NOT_FOUND"}
            ]
        }})).unwrap();
        let error = data.into_order().unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_USER_ERROR");
        assert!(error.message().contains("INVALID"));
        assert!(error.message().contains("Invalid address"));
        assert!(error.message().contains("VARIANT_NOT_FOUND"));
        assert!(error.message().contains("Variant unavailable"));
        assert!(!error.retryable());
    }

    #[test]
    fn mixed_order_and_user_errors_are_ambiguous_not_a_definite_rejection() {
        for order in [
            json!({"id": "gid://shopify/Order/42", "name": "#1042"}),
            json!({"id": "", "name": "#1042"}),
            json!({"id": "gid://shopify/Order/42", "name": ""}),
        ] {
            let data: CreateOrderData = serde_json::from_value(json!({"orderCreate": {
                "order": order,
                "userErrors": [{"field": ["order"], "message": "Mixed outcome", "code": "INVALID"}]
            }}))
            .unwrap();
            let error = data.into_order().unwrap_err();
            assert_eq!(error.code(), "SHOPIFY_ORDER_OUTCOME_UNCERTAIN");
            assert!(error.message().contains("Mixed outcome"));
            assert!(!error.retryable());
        }
    }
}

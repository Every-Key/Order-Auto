use async_trait::async_trait;
use orderpilot_lib::domain::{CustomerMode, FinancialStatus, OrderTemplate};
use orderpilot_lib::shopify::graphql::RetryWaiter;
use orderpilot_lib::shopify::CreateOrderInput;
use orderpilot_lib::{
    repository::StoreCredentials,
    shopify::{ShopifyGateway, ShopifyHttpClient},
};
use secrecy::SecretString;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

const BATCH_ID: &str = "00000000-0000-4000-8000-000000000001";

#[tokio::test]
async fn create_order_never_replays_ambiguous_http_server_failures() {
    for status in [500, 502, 503, 599] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&server)
            .await;
        let waiter = Arc::new(RecordingWaiter::default());
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .with_retry_waiter(waiter.clone())
            .create_order(
                &credentials(),
                CreateOrderInput::from_template(
                    order_template(),
                    &format!("orderpilot/{BATCH_ID}/1"),
                    false,
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_SERVER_ERROR");
        assert!(!error.retryable(), "ambiguous mutations must be reconciled");
        assert!(waiter.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn create_order_retries_only_explicit_throttling_up_to_three_attempts() {
    for graphql in [false, true] {
        let server = MockServer::start().await;
        let body = json!({"errors": [{"extensions": {"code": "THROTTLED"}}], "extensions": {"cost": {"requestedQueryCost": 100, "throttleStatus": {"currentlyAvailable": 0, "restoreRate": 25}}}});
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(if graphql { 200 } else { 429 })
                    .insert_header("Retry-After", "2")
                    .set_body_json(body),
            )
            .expect(3)
            .mount(&server)
            .await;
        let waiter = Arc::new(RecordingWaiter::default());
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .with_retry_waiter(waiter.clone())
            .create_order(
                &credentials(),
                CreateOrderInput::from_template(
                    order_template(),
                    &format!("orderpilot/{BATCH_ID}/1"),
                    false,
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_RATE_LIMITED");
        assert_eq!(
            *waiter.0.lock().unwrap(),
            vec![Duration::from_secs(4), Duration::from_secs(4)]
        );
    }
}

#[tokio::test]
async fn create_order_does_not_replay_partial_throttle_or_internal_graphql_errors() {
    for body in [
        json!({"data": {"orderCreate": {"order": {"id": "gid://shopify/Order/42", "name": "#42"}, "userErrors": []}}, "errors": [{"extensions": {"code": "THROTTLED"}}]}),
        json!({"errors": [{"extensions": {"code": "INTERNAL_SERVER_ERROR"}}]}),
        json!({"errors": [{"extensions": {"code": "THROTTLED"}}, {"extensions": {"code": "INTERNAL_SERVER_ERROR"}}]}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let waiter = Arc::new(RecordingWaiter::default());
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .with_retry_waiter(waiter.clone())
            .create_order(
                &credentials(),
                CreateOrderInput::from_template(
                    order_template(),
                    &format!("orderpilot/{BATCH_ID}/1"),
                    false,
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_GRAPHQL");
        assert!(!error.retryable());
        assert!(waiter.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn source_lookup_does_not_hide_extra_transport_attempts_in_one_reconciliation_query() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let waiter = Arc::new(RecordingWaiter::default());
    let error = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .with_retry_waiter(waiter.clone())
        .find_order_by_source(&credentials(), &format!("orderpilot/{BATCH_ID}/1"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_SERVER_ERROR");
    assert!(waiter.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_order_timeout_is_ambiguous_and_never_replayed() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"orderCreate": {"order": {"id": "gid://shopify/Order/42", "name": "#42"}, "userErrors": []}}})).set_delay(Duration::from_secs(60))).expect(1).mount(&server).await;
    let waiter = Arc::new(RecordingWaiter::default());
    let client = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .with_retry_waiter(waiter.clone());
    let task = tokio::spawn(async move {
        client
            .create_order(
                &credentials(),
                CreateOrderInput::from_template(
                    order_template(),
                    &format!("orderpilot/{BATCH_ID}/1"),
                    false,
                )
                .unwrap(),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_TIMEOUT");
    assert!(!error.retryable());
    assert!(waiter.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_order_connection_loss_after_receiving_request_is_never_replayed() {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        // The server may have created the order; lose the response connection.
    });
    let waiter = Arc::new(RecordingWaiter::default());
    let error = ShopifyHttpClient::new(Some(origin))
        .unwrap()
        .with_retry_waiter(waiter.clone())
        .create_order(
            &credentials(),
            CreateOrderInput::from_template(
                order_template(),
                &format!("orderpilot/{BATCH_ID}/1"),
                false,
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    peer.await.unwrap();
    assert_eq!(error.code(), "SHOPIFY_NETWORK");
    assert!(!error.retryable());
    assert!(waiter.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn find_order_by_source_uses_a_variable_and_verifies_the_exact_source() {
    let source = format!("orderpilot/{BATCH_ID}/1");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/admin/api/2026-07/graphql.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"orders": {
            "nodes": [{"id": "gid://shopify/Order/42", "name": "#1042", "sourceIdentifier": source}],
            "pageInfo": {"hasNextPage": false}
        }}}))).expect(1).mount(&server).await;
    let order = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .find_order_by_source(&credentials(), &source)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(order.name, "#1042");
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    let query = body["query"].as_str().unwrap();
    assert!(query.contains("orders(first: 2, query: $query)"));
    assert!(query.contains("sourceIdentifier"));
    assert!(!query.contains(&source));
    assert_eq!(
        body["variables"]["query"],
        format!("source_identifier:\"{source}\"")
    );
}

fn order_template() -> OrderTemplate {
    OrderTemplate {
        variant_id: "gid://shopify/ProductVariant/123".into(),
        quantity: 2,
        customer: CustomerMode::None,
        financial_status: FinancialStatus::Pending,
    }
}

#[test]
fn order_input_rejects_invalid_variant_quantity_customer_and_source() {
    let source = format!("orderpilot/{BATCH_ID}/1");
    for quantity in [0, u32::MAX] {
        let mut template = order_template();
        template.quantity = quantity;
        assert_eq!(
            CreateOrderInput::from_template(template, &source, false)
                .unwrap_err()
                .code(),
            "INVALID_ORDER_QUANTITY"
        );
    }
    for variant in [
        "",
        "gid://shopify/Product/123",
        "gid://shopify/ProductVariant/",
    ] {
        let mut template = order_template();
        template.variant_id = variant.into();
        assert_eq!(
            CreateOrderInput::from_template(template, &source, false)
                .unwrap_err()
                .code(),
            "INVALID_VARIANT_ID"
        );
    }
    let mut template = order_template();
    template.customer = CustomerMode::Existing {
        customer_id: " ".into(),
    };
    assert_eq!(
        CreateOrderInput::from_template(template, &source, false)
            .unwrap_err()
            .code(),
        "INVALID_CUSTOMER_ID"
    );
    for source in [
        "source-1".into(),
        "orderpilot/not-a-uuid/1".into(),
        format!("orderpilot/{BATCH_ID}/0"),
        format!("orderpilot/{BATCH_ID}/101"),
        format!("orderpilot/{BATCH_ID}/01"),
        format!("orderpilot/{BATCH_ID}/1/extra"),
    ] {
        assert_eq!(
            CreateOrderInput::from_template(order_template(), &source, false)
                .unwrap_err()
                .code(),
            "INVALID_SOURCE_IDENTIFIER"
        );
    }
}

#[tokio::test]
async fn create_order_sends_typed_mutation_and_returns_gid_and_name() {
    let server = MockServer::start().await;
    let source = format!("orderpilot/{BATCH_ID}/1");
    Mock::given(method("POST"))
        .and(path("/admin/api/2026-07/graphql.json"))
        .and(header("X-Shopify-Access-Token", "shpat_secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": {"orderCreate": {
                "order": {"id": "gid://shopify/Order/42", "name": "#1042"}, "userErrors": []
            }}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let order = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .create_order(
            &credentials(),
            CreateOrderInput::from_template(order_template(), &source, false).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(order.id, "gid://shopify/Order/42");
    assert_eq!(order.name, "#1042");
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let query = body["query"].as_str().unwrap();
    assert!(query.contains("$order: OrderCreateOrderInput!"));
    assert!(query.contains("orderCreate(order: $order)"));
    assert!(query.contains("userErrors { field message code }"));
    assert!(!query.contains(&source));
    assert_eq!(body["variables"]["order"]["sourceIdentifier"], source);
    assert_eq!(body["variables"]["order"]["lineItems"][0]["quantity"], 2);
}

#[tokio::test]
async fn create_order_user_errors_are_non_retryable_and_redacted() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"orderCreate": {
            "order": null,
            "userErrors": [{"field": ["order", "email"], "message": "Invalid email shpat_secret", "code": "INVALID"}]
        }}}))).expect(1).mount(&server).await;
    let error = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .create_order(
            &credentials(),
            CreateOrderInput::from_template(
                order_template(),
                &format!("orderpilot/{BATCH_ID}/1"),
                false,
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_USER_ERROR");
    assert!(error.message().contains("INVALID"));
    assert!(error.message().contains("Invalid email"));
    assert!(!error.retryable());
    assert_redacted(&error);
}

#[tokio::test]
async fn create_order_missing_result_is_invalid_and_not_retried() {
    for payload in [
        json!({"order": null, "userErrors": []}),
        json!({"order": {"id": "gid://shopify/Order/42"}, "userErrors": []}),
        json!({"order": {"id": "gid://shopify/Order/42", "name": "#1042"}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data": {"orderCreate": payload}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .create_order(
                &credentials(),
                CreateOrderInput::from_template(
                    order_template(),
                    &format!("orderpilot/{BATCH_ID}/1"),
                    false,
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_INVALID_RESPONSE");
        assert!(!error.retryable());
    }
}

#[test]
fn paid_order_requires_explicit_confirmation() {
    let mut template = order_template();
    template.financial_status = FinancialStatus::Paid;
    assert_eq!(
        CreateOrderInput::from_template(template.clone(), "source-1", false)
            .unwrap_err()
            .code(),
        "PAID_CONFIRMATION_REQUIRED"
    );
    let source = format!("orderpilot/{BATCH_ID}/1");
    let paid = CreateOrderInput::from_template(template, &source, true).unwrap();
    assert_eq!(
        serde_json::to_value(paid).unwrap()["financialStatus"],
        "PAID"
    );
    let pending = CreateOrderInput::from_template(order_template(), &source, false).unwrap();
    assert_eq!(
        serde_json::to_value(pending).unwrap(),
        json!({
            "lineItems": [{"variantId": "gid://shopify/ProductVariant/123", "quantity": 2}],
            "financialStatus": "PENDING",
            "sourceIdentifier": source,
            "customAttributes": [{"key": "OrderPilot-Batch", "value": BATCH_ID}]
        })
    );
}

#[test]
fn order_input_maps_only_selected_customer_and_filled_address_fields() {
    use orderpilot_lib::domain::{MailingAddress, ManualCustomer};
    let source = format!("orderpilot/{BATCH_ID}/1");
    let mut template = order_template();
    template.customer = CustomerMode::Existing {
        customer_id: "gid://shopify/Customer/42".into(),
    };
    let input = serde_json::to_value(
        CreateOrderInput::from_template(template.clone(), &source, false).unwrap(),
    )
    .unwrap();
    assert_eq!(
        input["customer"],
        json!({"toAssociate": {"id": "gid://shopify/Customer/42"}})
    );
    assert!(input.get("email").is_none());
    assert!(input.get("shippingAddress").is_none());
    template.customer = CustomerMode::Manual {
        customer: ManualCustomer {
            email: Some("alice@example.com".into()),
            first_name: Some("Alice".into()),
            last_name: Some(" ".into()),
            phone: None,
            shipping_address: Some(MailingAddress {
                address1: Some("1 Main Street".into()),
                city: Some("London".into()),
                address2: Some("".into()),
                ..Default::default()
            }),
        },
    };
    let input = serde_json::to_value(
        CreateOrderInput::from_template(template.clone(), &source, false).unwrap(),
    )
    .unwrap();
    assert_eq!(
        input["customer"],
        json!({"toUpsert": {"email": "alice@example.com", "firstName": "Alice"}})
    );
    assert_eq!(input["email"], "alice@example.com");
    assert_eq!(
        input["shippingAddress"],
        json!({"address1": "1 Main Street", "city": "London", "firstName": "Alice"})
    );
    assert!(input.get("phone").is_none());
    template.customer = CustomerMode::Manual {
        customer: ManualCustomer {
            shipping_address: Some(MailingAddress::default()),
            ..Default::default()
        },
    };
    let input =
        serde_json::to_value(CreateOrderInput::from_template(template, &source, false).unwrap())
            .unwrap();
    for key in ["customer", "shippingAddress", "email", "phone"] {
        assert!(input.get(key).is_none(), "unexpected {key}: {input}");
    }
}

fn credentials() -> StoreCredentials {
    StoreCredentials {
        shop_domain: "northwind.myshopify.com".into(),
        access_token: SecretString::from("shpat_secret"),
    }
}

fn connection_response() -> Value {
    json!({"data": {
        "shop": {"name": "Northwind", "myshopifyDomain": "northwind.myshopify.com"},
        "currentAppInstallation": {"accessScopes": [
            {"handle": "read_products"}, {"handle": "read_customers"},
            {"handle": "read_orders"}, {"handle": "write_orders"}
        ]}
    }})
}

fn documented_protected_customer_data_response() -> Value {
    json!({
        "data": {"customers": {"nodes": []}},
        "errors": [{
            "message": "This app is not approved to use the phoneNumber field. See https://partners.shopify.com/123/apps/456/customer_data for more details.",
            "locations": [],
            "path": ["customers", "nodes", 0, "defaultPhoneNumber", "phoneNumber"]
        }]
    })
}

fn assert_redacted(error: &orderpilot_lib::error::AppError) {
    assert!(!format!("{error:?}").contains("shpat_secret"));
    assert!(!error.to_string().contains("shpat_secret"));
    let dto = orderpilot_lib::error::AppErrorDto::from(error);
    assert!(!serde_json::to_string(&dto)
        .unwrap()
        .contains("shpat_secret"));
}

#[tokio::test]
async fn test_connection_reports_each_missing_required_scope() {
    let required = [
        "read_products",
        "read_customers",
        "read_orders",
        "write_orders",
    ];
    for missing in required {
        let server = MockServer::start().await;
        let mut body = connection_response();
        body["data"]["currentAppInstallation"]["accessScopes"] = Value::Array(
            required
                .iter()
                .filter(|scope| **scope != missing)
                .map(|scope| json!({"handle": scope}))
                .collect(),
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let report = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .test_connection(&credentials())
            .await
            .unwrap();
        assert!(!report.required_scopes_present);
        assert_eq!(report.missing_scopes, vec![missing]);
    }
}

#[tokio::test]
async fn authentication_errors_are_non_retryable_and_never_contain_token() {
    for status in [401, 403] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_string("Access denied: shpat_secret; missing write_orders"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = ShopifyHttpClient::new(Some(server.uri())).unwrap();
        let error = client.test_connection(&credentials()).await.unwrap_err();
        assert_eq!(
            error.code(),
            if status == 401 {
                "SHOPIFY_UNAUTHORIZED"
            } else {
                "SHOPIFY_FORBIDDEN"
            }
        );
        assert!(!error.retryable());
        assert_redacted(&error);
    }
}

#[tokio::test]
async fn graphql_top_level_errors_win_over_partial_data_and_are_sanitized() {
    let server = MockServer::start().await;
    let mut body = connection_response();
    body["errors"] = json!([{"message": "shpat_secret", "extensions": {"code": "THROTTLED"}}]);
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let error = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .test_connection(&credentials())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_GRAPHQL");
    assert!(!error.retryable());
    assert_redacted(&error);
}

#[derive(Default)]
struct RecordingWaiter(Mutex<Vec<Duration>>);

#[async_trait]
impl RetryWaiter for RecordingWaiter {
    async fn wait(&self, delay: Duration) {
        self.0.lock().unwrap().push(delay);
    }
}

#[tokio::test]
async fn retries_transient_http_failures_with_retry_after_and_throttle_metadata() {
    let server = MockServer::start().await;
    let counter = std::sync::atomic::AtomicUsize::new(0);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            match counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => ResponseTemplate::new(429)
                    .insert_header("Retry-After", "2")
                    .set_body_json(json!({"extensions": {"cost": {
                        "requestedQueryCost": 100,
                        "throttleStatus": {"currentlyAvailable": 0, "restoreRate": 25}
                    }}})),
                1 => ResponseTemplate::new(500)
                    .insert_header("Retry-After", "7")
                    .set_body_json(json!({"extensions": {"cost": {
                        "requestedQueryCost": 100,
                        "throttleStatus": {"currentlyAvailable": 0, "restoreRate": 25}
                    }}})),
                _ => ResponseTemplate::new(200).set_body_json(connection_response()),
            }
        })
        .expect(3)
        .mount(&server)
        .await;
    let waiter = Arc::new(RecordingWaiter::default());
    let client = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .with_retry_waiter(waiter.clone());
    assert!(
        client
            .test_connection(&credentials())
            .await
            .unwrap()
            .required_scopes_present
    );
    assert_eq!(
        *waiter.0.lock().unwrap(),
        vec![Duration::from_secs(4), Duration::from_secs(7)]
    );
}

#[tokio::test]
async fn retryable_http_failures_stop_after_three_total_attempts() {
    for status in [429, 500, 502, 503, 599] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_string("shpat_secret"))
            .expect(3)
            .mount(&server)
            .await;
        let waiter = Arc::new(RecordingWaiter::default());
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .with_retry_waiter(waiter.clone())
            .test_connection(&credentials())
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            if status == 429 {
                "SHOPIFY_RATE_LIMITED"
            } else {
                "SHOPIFY_SERVER_ERROR"
            }
        );
        assert!(error.retryable());
        assert_redacted(&error);
        assert_eq!(
            *waiter.0.lock().unwrap(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }
}

#[tokio::test]
async fn other_http_statuses_are_not_retried_and_redirects_are_not_followed() {
    for status in [302, 307, 400, 404, 408, 422] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", server.uri())
                    .set_body_string("shpat_secret"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let waiter = Arc::new(RecordingWaiter::default());
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .with_retry_waiter(waiter.clone())
            .test_connection(&credentials())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_HTTP");
        assert!(!error.retryable());
        assert_redacted(&error);
        assert!(waiter.0.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn malformed_or_missing_graphql_data_is_not_retried_and_never_leaks_token() {
    for body in [
        "shpat_secret",
        "{}",
        "{\"data\":null}",
        "{\"data\":{\"shop\":\"shpat_secret\"}}",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let error = ShopifyHttpClient::new(Some(server.uri()))
            .unwrap()
            .test_connection(&credentials())
            .await
            .unwrap_err();
        assert_eq!(error.code(), "SHOPIFY_INVALID_RESPONSE");
        assert!(!error.retryable());
        assert_redacted(&error);
    }
}

#[tokio::test]
async fn mutation_user_errors_remain_typed_data_and_echoed_tokens_are_redacted() {
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct MutationData {
        #[serde(rename = "orderCreate")]
        order_create: MutationPayload,
    }
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct MutationPayload {
        #[serde(rename = "userErrors")]
        user_errors: Vec<UserError>,
    }
    #[derive(Debug, serde::Deserialize, serde::Serialize)]
    struct UserError {
        field: Vec<String>,
        message: String,
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "orderCreate": {"order": null, "userErrors": [{"field": ["order", "email"],
                "message": "Invalid value shpat_secret"}]}
        }})))
        .expect(1)
        .mount(&server)
        .await;
    let data: MutationData = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .execute(
            &credentials(),
            "mutation Test($input: String!) { orderCreate { userErrors { field message } } }",
            &json!({"input": "example"}),
        )
        .await
        .unwrap();
    assert_eq!(
        data.order_create.user_errors[0].field,
        vec!["order", "email"]
    );
    assert_eq!(
        data.order_create.user_errors[0].message,
        "Invalid value [REDACTED]"
    );
    assert!(!format!("{data:?}").contains("shpat_secret"));
    assert!(!serde_json::to_string(&data)
        .unwrap()
        .contains("shpat_secret"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<Value>().unwrap()["variables"],
        json!({"input": "example"})
    );
}

#[tokio::test]
async fn request_times_out_after_thirty_seconds_without_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(connection_response())
                .set_delay(Duration::from_secs(60)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let waiter = Arc::new(RecordingWaiter::default());
    let client = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .with_retry_waiter(waiter.clone());
    let task = tokio::spawn(async move { client.test_connection(&credentials()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while server.received_requests().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mock should receive the request before pausing time");
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(
        !task.is_finished(),
        "request must keep waiting before its 30 second deadline"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_TIMEOUT");
    assert!(!error.retryable());
    assert_redacted(&error);
    assert!(waiter.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn retry_after_http_date_and_invalid_throttle_values_are_handled() {
    let server = MockServer::start().await;
    let retry_at = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc2822();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", retry_at)
                .set_body_json(json!({"extensions": {"cost": {
                    "requestedQueryCost": 100,
                    "throttleStatus": {"currentlyAvailable": 0, "restoreRate": 0}
                }}})),
        )
        .expect(3)
        .mount(&server)
        .await;
    let waiter = Arc::new(RecordingWaiter::default());
    let error = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .with_retry_waiter(waiter.clone())
        .test_connection(&credentials())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_RATE_LIMITED");
    let waits = waiter.0.lock().unwrap();
    assert_eq!(waits.len(), 2);
    assert!(waits
        .iter()
        .all(|delay| *delay > Duration::ZERO && *delay <= Duration::from_secs(5)));
}

#[tokio::test]
async fn network_failure_is_sanitized_and_not_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let waiter = Arc::new(RecordingWaiter::default());
    let error = ShopifyHttpClient::new(Some(format!("http://{address}")))
        .unwrap()
        .with_retry_waiter(waiter.clone())
        .test_connection(&credentials())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_NETWORK");
    assert!(!error.retryable());
    assert_redacted(&error);
    assert!(waiter.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_header_credentials_fail_without_network_and_are_sanitized() {
    let mut store = credentials();
    store.access_token = SecretString::from("shpat_secret\ninvalid");
    let error = ShopifyHttpClient::new(None)
        .unwrap()
        .test_connection(&store)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "SHOPIFY_CREDENTIALS");
    assert_redacted(&error);
}

#[tokio::test]
async fn test_connection_uses_pinned_graphql_endpoint_and_token_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/admin/api/2026-07/graphql.json"))
        .and(header("X-Shopify-Access-Token", "shpat_secret"))
        .and(header("Content-Type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(connection_response()))
        .expect(1)
        .mount(&server)
        .await;

    let client = ShopifyHttpClient::new(Some(server.uri())).unwrap();
    let gateway: &dyn ShopifyGateway = &client;
    let report = gateway.test_connection(&credentials()).await.unwrap();
    assert!(report.required_scopes_present);
    assert!(report.missing_scopes.is_empty());
    assert_eq!(report.shop_name, "Northwind");
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert!(body["query"]
        .as_str()
        .unwrap()
        .contains("currentAppInstallation"));
    assert!(!body.to_string().contains("shpat_secret"));
}

#[tokio::test]
async fn search_variants_maps_products_to_selectable_variants() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "shop": {"currencyCode": "USD"},
            "productVariants": {"nodes": [
                {
                    "id": "gid://shopify/ProductVariant/101",
                    "title": "Navy / M",
                    "sku": "HD-NV-M",
                    "price": "49.95",
                    "inventoryQuantity": 7,
                    "product": {"title": "Hoodie"}
                },
                {
                    "id": "gid://shopify/ProductVariant/102",
                    "title": "Navy / L",
                    "sku": null,
                    "price": "49.95",
                    "inventoryQuantity": null,
                    "product": {"title": "Hoodie"}
                }
            ]}
        }})))
        .expect(1)
        .mount(&server)
        .await;

    let rows = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .search_variants(&credentials(), "  hoodie  ")
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "gid://shopify/ProductVariant/101");
    assert_eq!(rows[0].product_title, "Hoodie");
    assert_eq!(rows[0].variant_title, "Navy / M");
    assert_eq!(rows[0].sku.as_deref(), Some("HD-NV-M"));
    assert_eq!(rows[0].price, "49.95");
    assert_eq!(rows[0].currency_code, "USD");
    assert_eq!(rows[0].inventory_quantity, Some(7));
    assert_eq!(rows[1].sku, None);
    assert_eq!(rows[1].inventory_quantity, None);

    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert_eq!(body["variables"]["query"], "\"hoodie\" OR sku:\"hoodie\"");
    assert!(body["query"]
        .as_str()
        .unwrap()
        .contains("productVariants(first: 20, query: $query)"));
    assert!(!body["query"].as_str().unwrap().contains("hoodie"));
}

#[tokio::test]
async fn search_customers_maps_nullable_contact_and_address_data() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "customers": {"nodes": [
                {
                    "id": "gid://shopify/Customer/201",
                    "displayName": "Alice Wu",
                    "defaultEmailAddress": {"emailAddress": "alice@example.com"},
                    "defaultPhoneNumber": null,
                    "defaultAddress": {
                        "firstName": "Alice",
                        "lastName": "Wu",
                        "company": null,
                        "address1": "1 Market Street",
                        "address2": null,
                        "city": "San Francisco",
                        "provinceCode": "CA",
                        "countryCodeV2": "US",
                        "zip": "94105"
                    }
                },
                {
                    "id": "gid://shopify/Customer/202",
                    "displayName": "No contact details",
                    "defaultEmailAddress": null,
                    "defaultPhoneNumber": null,
                    "defaultAddress": null
                }
            ]}
        }})))
        .expect(1)
        .mount(&server)
        .await;

    let rows = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .search_customers(&credentials(), "  alice  ")
        .await
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "gid://shopify/Customer/201");
    assert_eq!(rows[0].display_name, "Alice Wu");
    assert_eq!(rows[0].email.as_deref(), Some("alice@example.com"));
    assert_eq!(rows[0].phone, None);
    let address = rows[0].default_address.as_ref().unwrap();
    assert_eq!(address.address1.as_deref(), Some("1 Market Street"));
    assert_eq!(address.address2, None);
    assert_eq!(address.country_code.as_deref(), Some("US"));
    assert_eq!(rows[1].email, None);
    assert_eq!(rows[1].phone, None);
    assert_eq!(rows[1].default_address, None);

    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[0].body_json().unwrap();
    assert_eq!(body["variables"]["query"], "\"alice\"");
    assert!(body["query"]
        .as_str()
        .unwrap()
        .contains("customers(first: 20, query: $query)"));
    assert!(!body["query"].as_str().unwrap().contains("alice"));
}

#[tokio::test]
async fn search_customers_reports_protected_customer_data_separately() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(documented_protected_customer_data_response()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let error = ShopifyHttpClient::new(Some(server.uri()))
        .unwrap()
        .search_customers(&credentials(), "alice")
        .await
        .unwrap_err();

    assert_eq!(error.code(), "CUSTOMER_DATA_RESTRICTED");
    assert!(!error.retryable());
    assert_redacted(&error);
}

#[tokio::test]
async fn search_rejects_blank_and_overlong_text_before_transport_validation() {
    let client = ShopifyHttpClient::new(None).unwrap();
    let invalid_store = StoreCredentials {
        shop_domain: "not-a-shopify-domain".into(),
        access_token: SecretString::from("shpat_secret\ninvalid"),
    };
    let overlong = "界".repeat(121);

    for query in ["   ", overlong.as_str()] {
        let variant_error = client
            .search_variants(&invalid_store, query)
            .await
            .unwrap_err();
        assert_eq!(variant_error.code(), "SHOPIFY_SEARCH_QUERY");
        assert!(!variant_error.retryable());

        let customer_error = client
            .search_customers(&invalid_store, query)
            .await
            .unwrap_err();
        assert_eq!(customer_error.code(), "SHOPIFY_SEARCH_QUERY");
        assert!(!customer_error.retryable());
    }
}

#[tokio::test]
async fn search_accepts_120_characters_and_returns_empty_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {
            "shop": {"currencyCode": "USD"},
            "productVariants": {"nodes": []},
            "customers": {"nodes": []}
        }})))
        .expect(2)
        .mount(&server)
        .await;
    let client = ShopifyHttpClient::new(Some(server.uri())).unwrap();
    let query = "界".repeat(120);

    assert!(client
        .search_variants(&credentials(), &query)
        .await
        .unwrap()
        .is_empty());
    assert!(client
        .search_customers(&credentials(), &query)
        .await
        .unwrap()
        .is_empty());
}

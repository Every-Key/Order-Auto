use orderpilot_lib::domain::{BatchSize, CustomerMode, FinancialStatus, OrderTemplate};
use orderpilot_lib::repository::{NewStore, Repository, UpdateStore};
use secrecy::{ExposeSecret, SecretString};
use tempfile::TempDir;

fn template() -> OrderTemplate {
    OrderTemplate {
        variant_id: "gid://shopify/ProductVariant/123".into(),
        quantity: 2,
        customer: CustomerMode::None,
        financial_status: FinancialStatus::Pending,
    }
}

#[tokio::test]
async fn forced_attempt_migration_upgrades_v1_without_changing_existing_history() {
    let directory = tempfile::tempdir().unwrap();
    let previous_migrations = directory.path().join("v1-migrations");
    std::fs::create_dir(&previous_migrations).unwrap();
    std::fs::write(
        previous_migrations.join("0001_init.sql"),
        include_str!("../migrations/0001_init.sql"),
    )
    .unwrap();
    let database = directory.path().join("legacy.sqlite");
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::migrate::Migrator::new(previous_migrations.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO stores (id, display_name, shop_domain, access_token, created_at, updated_at) VALUES ('legacy-store', 'Legacy', 'legacy.myshopify.com', 'shpat_fixture', '2026-09-16', '2026-09-16')").execute(&pool).await.unwrap();
    let batch_id = "00000000-0000-4000-8000-000000000001";
    sqlx::query("INSERT INTO batch_jobs (id, store_id, order_template_json, requested_count, status, created_at) VALUES (?, 'legacy-store', ?, 1, 'completed', '2026-09-16')")
        .bind(batch_id).bind(serde_json::to_string(&template()).unwrap()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO batch_items (id, batch_id, sequence_number, source_identifier, status, attempt_count, shopify_order_id, shopify_order_name, created_at, updated_at) VALUES ('legacy-item', ?, 1, ?, 'succeeded', 1, 'gid://shopify/Order/42', '#42', '2026-09-16', '2026-09-16')")
        .bind(batch_id).bind(format!("orderpilot/{batch_id}/1")).execute(&pool).await.unwrap();
    pool.close().await;
    let repo = Repository::connect(&database).await.unwrap();
    assert_eq!(repo.search_stores("Legacy").await.unwrap().len(), 1);
    assert_eq!(repo.get_batch(batch_id).await.unwrap().status, "completed");
    let items = repo.list_batch_items(batch_id).await.unwrap();
    assert_eq!(items[0].shopify_order_name.as_deref(), Some("#42"));
    assert!(repo
        .list_forced_retry_attempts(batch_id)
        .await
        .unwrap()
        .is_empty());
    let reopened = Repository::connect(&database).await.unwrap();
    assert_eq!(reopened.list_batch_items(batch_id).await.unwrap(), items);
    assert!(reopened
        .list_forced_retry_attempts(batch_id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn batch_items_allow_only_legal_transitions_and_persist_results() {
    use orderpilot_lib::domain::BatchItemStatus as Status;
    use orderpilot_lib::error::AppError;
    let (_directory, repo) = test_repository().await;
    let store = batch_store(&repo).await;
    let job = repo
        .create_batch(&store, template(), BatchSize::new(4).unwrap())
        .await
        .unwrap();
    let items = repo.list_batch_items(&job.id).await.unwrap();
    assert_eq!(
        repo.next_queued_item(&job.id).await.unwrap().unwrap().id,
        items[0].id
    );
    assert_eq!(
        repo.mark_succeeded(&items[0].id, "gid://shopify/Order/1", "#1001")
            .await
            .unwrap_err()
            .code(),
        "INVALID_ITEM_TRANSITION"
    );
    for item in &items[..3] {
        repo.mark_creating(&item.id).await.unwrap();
        assert_eq!(
            repo.mark_creating(&item.id).await.unwrap_err().code(),
            "INVALID_ITEM_TRANSITION"
        );
    }
    repo.mark_succeeded(&items[0].id, "gid://shopify/Order/1", "#1001")
        .await
        .unwrap();
    repo.mark_failed(
        &items[1].id,
        &AppError::validation("INVALID", "Address invalid"),
    )
    .await
    .unwrap();
    repo.mark_uncertain(
        &items[2].id,
        &AppError::validation("SHOPIFY_TIMEOUT", "Check result"),
    )
    .await
    .unwrap();
    repo.mark_stopped(&items[3].id).await.unwrap();
    assert!(repo.next_queued_item(&job.id).await.unwrap().is_none());
    let saved = repo.list_batch_items(&job.id).await.unwrap();
    assert_eq!(
        saved.iter().map(|i| i.status).collect::<Vec<_>>(),
        vec![
            Status::Succeeded,
            Status::Failed,
            Status::Uncertain,
            Status::Stopped
        ]
    );
    assert_eq!(
        saved[0].shopify_order_id.as_deref(),
        Some("gid://shopify/Order/1")
    );
    assert_eq!(saved[0].shopify_order_name.as_deref(), Some("#1001"));
    assert_eq!(saved[1].error_code.as_deref(), Some("INVALID"));
    assert_eq!(saved[1].error_message.as_deref(), Some("Address invalid"));
    assert_eq!(saved[2].error_code.as_deref(), Some("SHOPIFY_TIMEOUT"));
    for (index, item) in saved.iter().enumerate() {
        assert_eq!(item.attempt_count, if index == 3 { 0 } else { 1 });
        assert!(item.updated_at > item.created_at);
        assert!(repo.mark_creating(&item.id).await.is_err());
        assert!(repo.mark_stopped(&item.id).await.is_err());
        assert!(repo
            .mark_failed(&item.id, &AppError::validation("NO", "No"))
            .await
            .is_err());
        assert!(repo
            .mark_uncertain(&item.id, &AppError::validation("NO", "No"))
            .await
            .is_err());
        assert!(repo
            .mark_succeeded(&item.id, "overwrite", "overwrite")
            .await
            .is_err());
    }
    assert_eq!(repo.list_batch_items(&job.id).await.unwrap(), saved);
}

async fn batch_store(repo: &Repository) -> String {
    repo.create_store(NewStore {
        display_name: "Batch store".into(),
        shop_domain: "batch-store".into(),
        access_token: SecretString::from("shpat_secret"),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn create_batch_rejects_invalid_templates_before_persisting() {
    let (_directory, repo) = test_repository().await;
    let store = batch_store(&repo).await;
    let mut invalid = template();
    invalid.quantity = 0;
    assert_eq!(
        repo.create_batch(&store, invalid, BatchSize::new(1).unwrap())
            .await
            .unwrap_err()
            .code(),
        "INVALID_ORDER_QUANTITY"
    );
    assert!(repo.list_batches().await.unwrap().is_empty());
}

#[tokio::test]
async fn competing_claims_only_increment_attempt_count_once() {
    let (_directory, repo) = test_repository().await;
    let store = batch_store(&repo).await;
    let job = repo
        .create_batch(&store, template(), BatchSize::new(1).unwrap())
        .await
        .unwrap();
    let item = repo.next_queued_item(&job.id).await.unwrap().unwrap();
    let (first, second) = tokio::join!(repo.mark_creating(&item.id), repo.mark_creating(&item.id));
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(
        repo.list_batch_items(&job.id).await.unwrap()[0].attempt_count,
        1
    );
}

#[tokio::test]
async fn create_batch_rolls_back_job_and_items_when_a_later_insert_fails() {
    let (directory, repo) = test_repository().await;
    let store_id = batch_store(&repo).await;
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        directory.path().join("orderpilot.sqlite").display()
    ))
    .await
    .unwrap();
    // Inject a storage failure after the job and first item have been written.
    sqlx::query("CREATE TRIGGER reject_second_item BEFORE INSERT ON batch_items WHEN NEW.sequence_number = 2 BEGIN SELECT RAISE(ABORT, 'injected failure'); END").execute(&pool).await.unwrap();
    let error = repo
        .create_batch(&store_id, template(), BatchSize::new(3).unwrap())
        .await
        .unwrap_err();
    assert_eq!(error.code(), "DATABASE_ERROR");
    assert!(repo.list_batches().await.unwrap().is_empty());
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM batch_items")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, 0);
}

#[tokio::test]
async fn create_batch_freezes_template_and_persists_ordered_unique_items() {
    let (directory, repo) = test_repository().await;
    let store_id = batch_store(&repo).await;
    let mut draft = template();
    let job = repo
        .create_batch(&store_id, draft.clone(), BatchSize::new(3).unwrap())
        .await
        .unwrap();
    draft.quantity = 99;
    let reopened = Repository::connect(directory.path().join("orderpilot.sqlite"))
        .await
        .unwrap();
    let saved = reopened.get_batch(&job.id).await.unwrap();
    assert_eq!(
        saved.order_template_json,
        serde_json::to_string(&template()).unwrap()
    );
    assert_eq!(saved.requested_count, 3);
    assert_eq!(saved.status, "pending");
    assert_eq!(saved.store_id.as_deref(), Some(store_id.as_str()));
    let items = reopened.list_batch_items(&job.id).await.unwrap();
    assert_eq!(items.len(), 3);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(item.sequence_number, index as i64 + 1);
        assert_eq!(
            item.source_identifier,
            format!("orderpilot/{}/{}", job.id, index + 1)
        );
        assert_eq!(item.attempt_count, 0);
        assert_eq!(serde_json::to_value(&item.status).unwrap(), "queued");
    }
    assert_eq!(
        items
            .iter()
            .map(|item| &item.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3
    );
    let second = reopened
        .create_batch(&store_id, draft, BatchSize::new(1).unwrap())
        .await
        .unwrap();
    assert_ne!(
        reopened.list_batch_items(&second.id).await.unwrap()[0].source_identifier,
        items[0].source_identifier
    );
}

async fn test_repository() -> (TempDir, Repository) {
    let directory = tempfile::tempdir().unwrap();
    let repository = Repository::connect(directory.path().join("orderpilot.sqlite"))
        .await
        .unwrap();
    (directory, repository)
}

#[tokio::test]
async fn searches_store_without_exposing_token() {
    let (_directory, repo) = test_repository().await;
    let id = repo
        .create_store(NewStore {
            display_name: "美国主店".into(),
            shop_domain: "northwind-us".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();

    let stores = repo.search_stores("northwind").await.unwrap();
    assert_eq!(stores[0].id, id);
    assert_eq!(stores[0].shop_domain, "northwind-us.myshopify.com");
    let json = serde_json::to_value(&stores[0]).unwrap();
    assert!(json.get("accessToken").is_none());
    assert!(!json.to_string().contains("shpat_secret"));
}

#[tokio::test]
async fn retrieves_store_credentials_for_rust_services() {
    let (_directory, repo) = test_repository().await;
    let id = repo
        .create_store(NewStore {
            display_name: "美国主店".into(),
            shop_domain: "northwind-us".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();

    let credentials = repo.store_credentials(&id).await.unwrap();
    assert_eq!(credentials.shop_domain, "northwind-us.myshopify.com");
    assert_eq!(credentials.access_token.expose_secret(), "shpat_secret");
}

#[tokio::test]
async fn updates_store_without_requiring_the_existing_token() {
    let (_directory, repo) = test_repository().await;
    let id = repo
        .create_store(NewStore {
            display_name: "美国主店".into(),
            shop_domain: "northwind-us".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();

    repo.update_store(
        &id,
        UpdateStore {
            display_name: "加拿大店".into(),
            shop_domain: "northwind-ca.myshopify.com".into(),
            access_token: None,
        },
    )
    .await
    .unwrap();

    let stores = repo.search_stores("加拿大").await.unwrap();
    assert_eq!(stores[0].shop_domain, "northwind-ca.myshopify.com");
    assert_eq!(
        repo.store_credentials(&id)
            .await
            .unwrap()
            .access_token
            .expose_secret(),
        "shpat_secret"
    );
}

#[tokio::test]
async fn deletes_store_profile_and_credentials() {
    let (_directory, repo) = test_repository().await;
    let id = repo
        .create_store(NewStore {
            display_name: "美国主店".into(),
            shop_domain: "northwind-us".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();

    repo.delete_store(&id).await.unwrap();

    assert!(repo.search_stores("").await.unwrap().is_empty());
    let error = match repo.store_credentials(&id).await {
        Ok(_) => panic!("deleted credentials remained accessible"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "STORE_NOT_FOUND");
}

#[tokio::test]
async fn rejects_duplicate_normalized_shop_domain() {
    let (_directory, repo) = test_repository().await;
    repo.create_store(NewStore {
        display_name: "美国主店".into(),
        shop_domain: "northwind-us".into(),
        access_token: SecretString::from("shpat_first"),
    })
    .await
    .unwrap();

    let duplicate = repo
        .create_store(NewStore {
            display_name: "重复店铺".into(),
            shop_domain: "NORTHWIND-US.MYSHOPIFY.COM".into(),
            access_token: SecretString::from("shpat_second"),
        })
        .await;

    assert!(duplicate.is_err());
    assert_eq!(repo.search_stores("").await.unwrap().len(), 1);
}

#[tokio::test]
async fn treats_like_wildcards_as_literal_search_text() {
    let (_directory, repo) = test_repository().await;
    repo.create_store(NewStore {
        display_name: "100%_真实店铺".into(),
        shop_domain: "literal-search".into(),
        access_token: SecretString::from("shpat_secret"),
    })
    .await
    .unwrap();
    repo.create_store(NewStore {
        display_name: "普通店铺".into(),
        shop_domain: "ordinary-store".into(),
        access_token: SecretString::from("shpat_other"),
    })
    .await
    .unwrap();

    let stores = repo.search_stores("%_").await.unwrap();

    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].display_name, "100%_真实店铺");
}

#[tokio::test]
async fn deleting_store_keeps_batch_history() {
    let (directory, repo) = test_repository().await;
    let store_id = repo
        .create_store(NewStore {
            display_name: "美国主店".into(),
            shop_domain: "northwind-us".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("orderpilot.sqlite").display()
    );
    let pool = sqlx::SqlitePool::connect(&database_url).await.unwrap();
    sqlx::query(
        r#"
        INSERT INTO batch_jobs (
          id, store_id, order_template_json, requested_count, status, created_at
        ) VALUES (?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("batch-1")
    .bind(&store_id)
    .bind("{}")
    .bind(1_i64)
    .bind("pending")
    .bind("2026-09-17T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    repo.delete_store(&store_id).await.unwrap();

    let batches = repo.list_batches().await.unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].id, "batch-1");
    assert_eq!(batches[0].store_id, None);
}

#[tokio::test]
async fn reconnects_to_an_existing_migrated_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("orderpilot.sqlite");
    let first = Repository::connect(&path).await.unwrap();
    first
        .create_store(NewStore {
            display_name: "持久店铺".into(),
            shop_domain: "persistent-store".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();
    drop(first);

    let reopened = Repository::connect(&path).await.unwrap();
    let stores = reopened.search_stores("持久").await.unwrap();

    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].shop_domain, "persistent-store.myshopify.com");
}

#[tokio::test]
async fn supports_isolated_in_memory_repositories() {
    let first = Repository::connect(":memory:").await.unwrap();
    first
        .create_store(NewStore {
            display_name: "临时店铺".into(),
            shop_domain: "memory-store".into(),
            access_token: SecretString::from("shpat_secret"),
        })
        .await
        .unwrap();
    assert_eq!(first.search_stores("").await.unwrap().len(), 1);
    drop(first);

    let second = Repository::connect(":memory:").await.unwrap();
    assert!(second.search_stores("").await.unwrap().is_empty());
}

#[tokio::test]
async fn schema_rejects_duplicate_source_identifiers_and_invalid_statuses() {
    let (directory, _repo) = test_repository().await;
    let database_url = format!(
        "sqlite://{}",
        directory.path().join("orderpilot.sqlite").display()
    );
    let pool = sqlx::SqlitePool::connect(&database_url).await.unwrap();
    sqlx::query(
        r#"
        INSERT INTO batch_jobs (
          id, order_template_json, requested_count, status, created_at
        ) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind("batch-1")
    .bind("{}")
    .bind(2_i64)
    .bind("pending")
    .bind("2026-09-17T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        INSERT INTO batch_items (
          id, batch_id, sequence_number, source_identifier, status, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("item-1")
    .bind("batch-1")
    .bind(1_i64)
    .bind("orderpilot/batch-1/1")
    .bind("queued")
    .bind("2026-09-17T00:00:00Z")
    .bind("2026-09-17T00:00:00Z")
    .execute(&pool)
    .await
    .unwrap();

    let duplicate_source = sqlx::query(
        r#"
        INSERT INTO batch_items (
          id, batch_id, sequence_number, source_identifier, status, created_at, updated_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("item-2")
    .bind("batch-1")
    .bind(2_i64)
    .bind("orderpilot/batch-1/1")
    .bind("queued")
    .bind("2026-09-17T00:00:00Z")
    .bind("2026-09-17T00:00:00Z")
    .execute(&pool)
    .await;
    let invalid_status = sqlx::query(
        r#"
        INSERT INTO batch_jobs (
          id, order_template_json, requested_count, status, created_at
        ) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind("batch-invalid")
    .bind("{}")
    .bind(1_i64)
    .bind("unknown")
    .bind("2026-09-17T00:00:00Z")
    .execute(&pool)
    .await;

    assert!(duplicate_source.is_err());
    assert!(invalid_status.is_err());
}

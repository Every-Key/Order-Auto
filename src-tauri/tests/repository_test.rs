use orderpilot_lib::repository::{NewStore, Repository, UpdateStore};
use secrecy::{ExposeSecret, SecretString};
use tempfile::TempDir;

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

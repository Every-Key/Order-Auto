use std::path::Path;

use chrono::Utc;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use uuid::Uuid;

use crate::{domain::normalize_shop_domain, error::AppError};

#[derive(Clone)]
pub struct Repository {
    pool: SqlitePool,
}

pub struct NewStore {
    pub display_name: String,
    pub shop_domain: String,
    pub access_token: SecretString,
}

pub struct UpdateStore {
    pub display_name: String,
    pub shop_domain: String,
    pub access_token: Option<SecretString>,
}

#[derive(Clone)]
pub struct StoreCredentials {
    pub shop_domain: String,
    pub access_token: SecretString,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct StoreSummary {
    pub id: String,
    pub display_name: String,
    pub shop_domain: String,
    pub last_connection_status: Option<String>,
    pub last_connection_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, FromRow)]
#[serde(rename_all = "camelCase")]
pub struct BatchSummary {
    pub id: String,
    pub store_id: Option<String>,
    pub requested_count: i64,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl Repository {
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        let in_memory = path == Path::new(":memory:");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .in_memory(in_memory)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(if in_memory { 1 } else { 5 })
            .connect_with(options)
            .await
            .map_err(database_error)?;

        sqlx::migrate!().run(&pool).await.map_err(database_error)?;

        Ok(Self { pool })
    }

    pub async fn create_store(&self, input: NewStore) -> Result<String, AppError> {
        let id = Uuid::new_v4().to_string();
        let shop_domain = normalize_shop_domain(&input.shop_domain)?;
        let now = Utc::now().to_rfc3339();
        let mut transaction = self.pool.begin().await.map_err(database_error)?;

        sqlx::query(
            r#"
            INSERT INTO stores (
              id, display_name, shop_domain, access_token, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(input.display_name)
        .bind(shop_domain)
        .bind(input.access_token.expose_secret())
        .bind(&now)
        .bind(&now)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;

        transaction.commit().await.map_err(database_error)?;
        Ok(id)
    }

    pub async fn search_stores(&self, query: &str) -> Result<Vec<StoreSummary>, AppError> {
        let escaped = escape_like(query.trim());
        let pattern = format!("%{escaped}%");

        sqlx::query_as::<_, StoreSummary>(
            r#"
            SELECT
              id,
              display_name,
              shop_domain,
              last_connection_status,
              last_connection_message,
              created_at,
              updated_at
            FROM stores
            WHERE display_name LIKE ? ESCAPE '\'
               OR shop_domain LIKE ? ESCAPE '\'
            ORDER BY display_name COLLATE NOCASE, id
            "#,
        )
        .bind(&pattern)
        .bind(&pattern)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)
    }

    pub async fn update_store(&self, id: &str, input: UpdateStore) -> Result<(), AppError> {
        let shop_domain = normalize_shop_domain(&input.shop_domain)?;
        let now = Utc::now().to_rfc3339();
        let access_token = input
            .access_token
            .as_ref()
            .map(|token| token.expose_secret());
        let mut transaction = self.pool.begin().await.map_err(database_error)?;

        let result = sqlx::query(
            r#"
            UPDATE stores
            SET display_name = ?,
                shop_domain = ?,
                access_token = COALESCE(?, access_token),
                updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(input.display_name)
        .bind(shop_domain)
        .bind(access_token)
        .bind(now)
        .bind(id)
        .execute(&mut *transaction)
        .await
        .map_err(database_error)?;

        if result.rows_affected() == 0 {
            return Err(store_not_found());
        }

        transaction.commit().await.map_err(database_error)
    }

    pub async fn delete_store(&self, id: &str) -> Result<(), AppError> {
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        let result = sqlx::query("DELETE FROM stores WHERE id = ?")
            .bind(id)
            .execute(&mut *transaction)
            .await
            .map_err(database_error)?;

        if result.rows_affected() == 0 {
            return Err(store_not_found());
        }

        transaction.commit().await.map_err(database_error)
    }

    pub async fn store_credentials(&self, id: &str) -> Result<StoreCredentials, AppError> {
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT shop_domain, access_token FROM stores WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_error)?
        .ok_or_else(store_not_found)?;

        Ok(StoreCredentials {
            shop_domain: row.0,
            access_token: SecretString::from(row.1),
        })
    }

    pub async fn list_batches(&self) -> Result<Vec<BatchSummary>, AppError> {
        sqlx::query_as::<_, BatchSummary>(
            r#"
            SELECT
              id,
              store_id,
              requested_count,
              status,
              created_at,
              started_at,
              finished_at
            FROM batch_jobs
            ORDER BY created_at DESC, id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn database_error(_: impl std::fmt::Display) -> AppError {
    AppError::validation("DATABASE_ERROR", "本地数据库操作失败")
}

fn store_not_found() -> AppError {
    AppError::validation("STORE_NOT_FOUND", "未找到店铺")
}

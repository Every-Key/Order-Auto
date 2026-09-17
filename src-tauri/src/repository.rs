use std::path::Path;

use chrono::Utc;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sqlx::{sqlite::SqlitePoolOptions, FromRow, SqlitePool};
use uuid::Uuid;

use crate::{
    domain::{
        normalize_shop_domain, BatchItem, BatchItemStatus, BatchJob, BatchSize, OrderTemplate,
    },
    error::AppError,
};

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
    /// Observation only: callers must successfully mark_creating before submitting.
    pub async fn next_queued_item(&self, batch_id: &str) -> Result<Option<BatchItem>, AppError> {
        sqlx::query_as::<_, BatchItem>("SELECT * FROM batch_items WHERE batch_id = ? AND status = 'queued' ORDER BY sequence_number LIMIT 1")
            .bind(batch_id).fetch_optional(&self.pool).await.map_err(database_error)
    }

    pub async fn mark_creating(&self, id: &str) -> Result<(), AppError> {
        self.transition_item(id, ItemTransition::Creating).await
    }

    pub async fn mark_succeeded(
        &self,
        id: &str,
        order_id: &str,
        order_name: &str,
    ) -> Result<(), AppError> {
        self.transition_item(
            id,
            ItemTransition::Succeeded {
                order_id,
                order_name,
            },
        )
        .await
    }

    pub async fn mark_failed(&self, id: &str, error: &AppError) -> Result<(), AppError> {
        self.transition_item(id, ItemTransition::Failed(error))
            .await
    }

    pub async fn mark_uncertain(&self, id: &str, error: &AppError) -> Result<(), AppError> {
        self.transition_item(id, ItemTransition::Uncertain(error))
            .await
    }

    pub async fn mark_stopped(&self, id: &str) -> Result<(), AppError> {
        self.transition_item(id, ItemTransition::Stopped).await
    }

    async fn transition_item(
        &self,
        id: &str,
        transition: ItemTransition<'_>,
    ) -> Result<(), AppError> {
        use BatchItemStatus::*;
        let (from, to, order_id, order_name, error) = match transition {
            ItemTransition::Creating => (Queued, Creating, None, None, None),
            ItemTransition::Succeeded {
                order_id,
                order_name,
            } => (Creating, Succeeded, Some(order_id), Some(order_name), None),
            ItemTransition::Failed(error) => (Creating, Failed, None, None, Some(error)),
            ItemTransition::Uncertain(error) => (Creating, Uncertain, None, None, Some(error)),
            ItemTransition::Stopped => (Queued, Stopped, None, None, None),
        };
        // A conditional single write guards concurrent callers and keeps results/timestamp atomic.
        let result = sqlx::query("UPDATE batch_items SET status = ?, attempt_count = attempt_count + ?, shopify_order_id = ?, shopify_order_name = ?, error_code = ?, error_message = ?, updated_at = ? WHERE id = ? AND status = ?")
            .bind(to).bind(i64::from(to == Creating)).bind(order_id).bind(order_name)
            .bind(error.map(AppError::code)).bind(error.map(AppError::message)).bind(Utc::now().to_rfc3339())
            .bind(id).bind(from).execute(&self.pool).await.map_err(database_error)?;
        if result.rows_affected() == 0 {
            return Err(AppError::validation(
                "INVALID_ITEM_TRANSITION",
                "批次项不存在或当前状态不允许此操作",
            ));
        }
        Ok(())
    }

    pub async fn create_batch(
        &self,
        store_id: &str,
        template: OrderTemplate,
        size: BatchSize,
    ) -> Result<BatchJob, AppError> {
        template.validate()?;
        let job = BatchJob {
            id: Uuid::new_v4().to_string(),
            store_id: Some(store_id.into()),
            order_template_json: serde_json::to_string(&template).map_err(database_error)?,
            requested_count: i64::from(size.get()),
            status: "pending".into(),
            created_at: Utc::now().to_rfc3339(),
            started_at: None,
            finished_at: None,
        };
        let mut transaction = self.pool.begin().await.map_err(database_error)?;
        sqlx::query("INSERT INTO batch_jobs (id, store_id, order_template_json, requested_count, status, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&job.id).bind(store_id).bind(&job.order_template_json).bind(job.requested_count)
            .bind(&job.status).bind(&job.created_at).execute(&mut *transaction).await.map_err(database_error)?;
        for sequence in 1..=size.get() {
            sqlx::query("INSERT INTO batch_items (id, batch_id, sequence_number, source_identifier, status, created_at, updated_at) VALUES (?, ?, ?, ?, 'queued', ?, ?)")
                .bind(Uuid::new_v4().to_string()).bind(&job.id).bind(i64::from(sequence))
                .bind(format!("orderpilot/{}/{sequence}", job.id)).bind(&job.created_at).bind(&job.created_at)
                .execute(&mut *transaction).await.map_err(database_error)?;
        }
        transaction.commit().await.map_err(database_error)?;
        Ok(job)
    }

    pub async fn get_batch(&self, id: &str) -> Result<BatchJob, AppError> {
        sqlx::query_as::<_, BatchJob>("SELECT * FROM batch_jobs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(database_error)?
            .ok_or_else(|| AppError::validation("BATCH_NOT_FOUND", "未找到批次"))
    }

    pub async fn list_batch_items(&self, batch_id: &str) -> Result<Vec<BatchItem>, AppError> {
        sqlx::query_as::<_, BatchItem>(
            "SELECT * FROM batch_items WHERE batch_id = ? ORDER BY sequence_number",
        )
        .bind(batch_id)
        .fetch_all(&self.pool)
        .await
        .map_err(database_error)
    }

    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        let in_memory = path == Path::new(":memory:");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .in_memory(in_memory)
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = pool_options(in_memory)
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

enum ItemTransition<'a> {
    Creating,
    Succeeded {
        order_id: &'a str,
        order_name: &'a str,
    },
    Failed(&'a AppError),
    Uncertain(&'a AppError),
    Stopped,
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn pool_options(in_memory: bool) -> SqlitePoolOptions {
    let options = SqlitePoolOptions::new().max_connections(if in_memory { 1 } else { 5 });
    if in_memory {
        options.idle_timeout(None).max_lifetime(None)
    } else {
        options
    }
}

fn database_error(_: impl std::fmt::Display) -> AppError {
    AppError::validation("DATABASE_ERROR", "本地数据库操作失败")
}

fn store_not_found() -> AppError {
    AppError::validation("STORE_NOT_FOUND", "未找到店铺")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_pool_never_retires_its_only_connection() {
        let options = pool_options(true);

        assert_eq!(options.get_max_connections(), 1);
        assert_eq!(options.get_idle_timeout(), None);
        assert_eq!(options.get_max_lifetime(), None);
    }
}

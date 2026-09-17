pub mod graphql;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{error::AppError, repository::StoreCredentials};
pub use graphql::ShopifyHttpClient;

pub const REQUIRED_SCOPES: [&str; 4] = [
    "read_products",
    "read_customers",
    "read_orders",
    "write_orders",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionReport {
    pub shop_name: String,
    pub shop_domain: String,
    pub required_scopes_present: bool,
    pub missing_scopes: Vec<String>,
}

/// Search and order methods join this seam when their domain types are introduced.
#[async_trait]
pub trait ShopifyGateway: Send + Sync {
    async fn test_connection(&self, store: &StoreCredentials)
        -> Result<ConnectionReport, AppError>;
}

#[async_trait]
impl ShopifyGateway for ShopifyHttpClient {
    async fn test_connection(
        &self,
        store: &StoreCredentials,
    ) -> Result<ConnectionReport, AppError> {
        let data: ConnectionData = self
            .execute(store, CONNECTION_QUERY, &serde_json::json!({}))
            .await?;
        let missing_scopes: Vec<String> = REQUIRED_SCOPES
            .iter()
            .filter(|required| {
                !data
                    .current_app_installation
                    .access_scopes
                    .iter()
                    .any(|scope| &scope.handle == *required)
            })
            .map(|scope| (*scope).to_owned())
            .collect();
        Ok(ConnectionReport {
            shop_name: data.shop.name,
            shop_domain: data.shop.myshopify_domain,
            required_scopes_present: missing_scopes.is_empty(),
            missing_scopes,
        })
    }
}

const CONNECTION_QUERY: &str = "query TestConnection { shop { name myshopifyDomain } currentAppInstallation { accessScopes { handle } } }";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionData {
    shop: Shop,
    current_app_installation: Installation,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shop {
    name: String,
    myshopify_domain: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Installation {
    access_scopes: Vec<Scope>,
}

#[derive(Deserialize)]
struct Scope {
    handle: String,
}

pub mod graphql;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{error::AppError, repository::StoreCredentials};
pub use graphql::{CreateOrderInput, CreatedOrder, ShopifyHttpClient};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductVariant {
    pub id: String,
    pub product_title: String,
    pub variant_title: String,
    pub sku: Option<String>,
    pub price: String,
    pub currency_code: String,
    pub inventory_quantity: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerSummary {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub default_address: Option<CustomerAddress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomerAddress {
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub company: Option<String>,
    pub address1: Option<String>,
    pub address2: Option<String>,
    pub city: Option<String>,
    pub province_code: Option<String>,
    pub country_code: Option<String>,
    pub zip: Option<String>,
}

/// Shopify operations cross this seam while credentials and transport stay in Rust.
#[async_trait]
pub trait ShopifyGateway: Send + Sync {
    async fn find_order_by_source(
        &self,
        store: &StoreCredentials,
        source: &str,
    ) -> Result<Option<CreatedOrder>, AppError>;
    async fn create_order(
        &self,
        store: &StoreCredentials,
        input: CreateOrderInput,
    ) -> Result<CreatedOrder, AppError>;
    async fn test_connection(&self, store: &StoreCredentials)
        -> Result<ConnectionReport, AppError>;
    async fn search_variants(
        &self,
        store: &StoreCredentials,
        query: &str,
    ) -> Result<Vec<ProductVariant>, AppError>;
    async fn search_customers(
        &self,
        store: &StoreCredentials,
        query: &str,
    ) -> Result<Vec<CustomerSummary>, AppError>;
}

#[async_trait]
impl ShopifyGateway for ShopifyHttpClient {
    async fn find_order_by_source(
        &self,
        store: &StoreCredentials,
        source: &str,
    ) -> Result<Option<CreatedOrder>, AppError> {
        graphql::source_batch_id(source)?;
        let data: graphql::FindOrderData = self.execute_once(store, graphql::FIND_ORDER_QUERY, &serde_json::json!({"query": format!("source_identifier:\"{}\"", escape_search_term(source))})).await?;
        data.into_order(source)
    }

    async fn create_order(
        &self,
        store: &StoreCredentials,
        input: CreateOrderInput,
    ) -> Result<CreatedOrder, AppError> {
        let data: graphql::CreateOrderData = self
            .execute_order_mutation(store, &serde_json::json!({"order": input}))
            .await?;
        data.into_order()
    }

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

    async fn search_variants(
        &self,
        store: &StoreCredentials,
        query: &str,
    ) -> Result<Vec<ProductVariant>, AppError> {
        let query = normalize_search_query(query)?;
        let data: VariantSearchData = self
            .execute(
                store,
                VARIANT_SEARCH_QUERY,
                &serde_json::json!({ "query": variant_search_query(query) }),
            )
            .await?;
        Ok(data
            .product_variants
            .nodes
            .into_iter()
            .map(|variant| ProductVariant {
                id: variant.id,
                product_title: variant.product.title,
                variant_title: variant.title,
                sku: variant.sku,
                price: variant.price,
                currency_code: data.shop.currency_code.clone(),
                inventory_quantity: variant.inventory_quantity,
            })
            .collect())
    }

    async fn search_customers(
        &self,
        store: &StoreCredentials,
        query: &str,
    ) -> Result<Vec<CustomerSummary>, AppError> {
        let query = normalize_search_query(query)?;
        let data: CustomerSearchData = self
            .execute(
                store,
                CUSTOMER_SEARCH_QUERY,
                &serde_json::json!({ "query": customer_search_query(query) }),
            )
            .await?;
        Ok(data
            .customers
            .nodes
            .into_iter()
            .map(|customer| CustomerSummary {
                id: customer.id,
                display_name: customer.display_name,
                email: customer
                    .default_email_address
                    .map(|email| email.email_address),
                phone: customer
                    .default_phone_number
                    .map(|phone| phone.phone_number),
                default_address: customer.default_address.map(CustomerAddress::from),
            })
            .collect())
    }
}

const CONNECTION_QUERY: &str = "query TestConnection { shop { name myshopifyDomain } currentAppInstallation { accessScopes { handle } } }";
const VARIANT_SEARCH_QUERY: &str = "query SearchVariants($query: String!) { shop { currencyCode } productVariants(first: 20, query: $query) { nodes { id title sku price inventoryQuantity product { title } } } }";
const CUSTOMER_SEARCH_QUERY: &str = "query SearchCustomers($query: String!) { customers(first: 20, query: $query) { nodes { id displayName defaultEmailAddress { emailAddress } defaultPhoneNumber { phoneNumber } defaultAddress { firstName lastName company address1 address2 city provinceCode countryCodeV2 zip } } } }";

fn variant_search_query(query: &str) -> String {
    let escaped = escape_search_term(query);
    format!("\"{escaped}\" OR sku:\"{escaped}\"")
}

fn customer_search_query(query: &str) -> String {
    let escaped = escape_search_term(query);
    format!("\"{escaped}\"")
}

fn escape_search_term(query: &str) -> String {
    query.replace('\\', "\\\\").replace('"', "\\\"")
}

fn normalize_search_query(query: &str) -> Result<&str, AppError> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 120 {
        return Err(AppError::validation(
            "SHOPIFY_SEARCH_QUERY",
            "搜索内容必须为 1 到 120 个字符",
        ));
    }
    Ok(query)
}

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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VariantSearchData {
    shop: VariantSearchShop,
    product_variants: VariantConnection,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VariantSearchShop {
    currency_code: String,
}

#[derive(Deserialize)]
struct VariantConnection {
    nodes: Vec<VariantNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VariantNode {
    id: String,
    title: String,
    sku: Option<String>,
    price: String,
    inventory_quantity: Option<i64>,
    product: VariantProduct,
}

#[derive(Deserialize)]
struct VariantProduct {
    title: String,
}

#[derive(Deserialize)]
struct CustomerSearchData {
    customers: CustomerConnection,
}

#[derive(Deserialize)]
struct CustomerConnection {
    nodes: Vec<CustomerNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerNode {
    id: String,
    display_name: String,
    default_email_address: Option<CustomerEmailAddress>,
    default_phone_number: Option<CustomerPhoneNumber>,
    default_address: Option<CustomerAddressNode>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerEmailAddress {
    email_address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerPhoneNumber {
    phone_number: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CustomerAddressNode {
    first_name: Option<String>,
    last_name: Option<String>,
    company: Option<String>,
    address1: Option<String>,
    address2: Option<String>,
    city: Option<String>,
    province_code: Option<String>,
    country_code_v2: Option<String>,
    zip: Option<String>,
}

impl From<CustomerAddressNode> for CustomerAddress {
    fn from(address: CustomerAddressNode) -> Self {
        Self {
            first_name: address.first_name,
            last_name: address.last_name,
            company: address.company,
            address1: address.address1,
            address2: address.address2,
            city: address.city,
            province_code: address.province_code,
            country_code: address.country_code_v2,
            zip: address.zip,
        }
    }
}

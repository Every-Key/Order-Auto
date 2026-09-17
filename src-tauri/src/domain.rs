use serde::{Deserialize, Deserializer, Serialize};

use crate::error::AppError;

pub fn normalize_shop_domain(raw: &str) -> Result<String, AppError> {
    let value = raw.trim().to_ascii_lowercase();
    let host = value.strip_suffix(".myshopify.com").unwrap_or(&value);
    let valid = !host.is_empty()
        && host.len() <= 63
        && host
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !host.starts_with('-')
        && !host.ends_with('-');

    valid
        .then(|| format!("{host}.myshopify.com"))
        .ok_or_else(|| AppError::validation("INVALID_SHOP_DOMAIN", "请输入有效的 Shopify 店铺域名"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BatchSize(u16);

impl BatchSize {
    pub fn new(value: u16) -> Result<Self, AppError> {
        (1..=100)
            .contains(&value)
            .then_some(Self(value))
            .ok_or_else(|| AppError::validation("INVALID_BATCH_SIZE", "批量数量必须为 1 到 100"))
    }

    pub fn get(self) -> u16 {
        self.0
    }
}

impl<'de> Deserialize<'de> for BatchSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FinancialStatus {
    Pending,
    Paid,
}

impl Default for FinancialStatus {
    fn default() -> Self {
        Self::Pending
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CustomerMode {
    None,
    Existing { customer_id: String },
    Manual { customer: ManualCustomer },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualCustomer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipping_address: Option<MailingAddress>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailingAddress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub province: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderTemplate {
    pub variant_id: String,
    pub quantity: u32,
    pub customer: CustomerMode,
    pub financial_status: FinancialStatus,
}

impl OrderTemplate {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.quantity == 0 || self.quantity > i32::MAX as u32 {
            return Err(AppError::validation(
                "INVALID_ORDER_QUANTITY",
                "每单商品数量必须为有效的正整数",
            ));
        }
        if !valid_shopify_gid(&self.variant_id, "ProductVariant") {
            return Err(AppError::validation(
                "INVALID_VARIANT_ID",
                "请选择具体的 Shopify 商品变体",
            ));
        }
        if let CustomerMode::Existing { customer_id } = &self.customer {
            if !valid_shopify_gid(customer_id, "Customer") {
                return Err(AppError::validation(
                    "INVALID_CUSTOMER_ID",
                    "请选择有效的 Shopify 客户",
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn valid_shopify_gid(value: &str, kind: &str) -> bool {
    value
        .strip_prefix(&format!("gid://shopify/{kind}/"))
        .is_some_and(|id| {
            !id.is_empty()
                && id.bytes().all(|byte| byte.is_ascii_digit())
                && id.parse::<u64>().is_ok_and(|id| id > 0)
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Queued,
    Creating,
    Succeeded,
    Failed,
    Uncertain,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct BatchJob {
    pub id: String,
    pub store_id: Option<String>,
    pub order_template_json: String,
    pub requested_count: i64,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem {
    pub id: String,
    pub batch_id: String,
    pub sequence_number: i64,
    pub source_identifier: String,
    pub status: BatchItemStatus,
    pub attempt_count: i64,
    pub shopify_order_id: Option<String>,
    pub shopify_order_name: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_shop_subdomain() {
        assert_eq!(
            normalize_shop_domain("northwind-us").unwrap(),
            "northwind-us.myshopify.com"
        );
    }

    #[test]
    fn rejects_non_shopify_domain() {
        assert_eq!(
            normalize_shop_domain("example.com").unwrap_err().code(),
            "INVALID_SHOP_DOMAIN"
        );
    }

    #[test]
    fn batch_size_accepts_only_one_through_one_hundred() {
        assert!(BatchSize::new(1).is_ok());
        assert!(BatchSize::new(100).is_ok());
        assert!(BatchSize::new(0).is_err());
        assert!(BatchSize::new(101).is_err());
    }

    #[test]
    fn batch_size_deserialization_rejects_values_outside_the_invariant() {
        for json in ["0", "101", "65535"] {
            assert!(serde_json::from_str::<BatchSize>(json).is_err());
        }
    }

    #[test]
    fn batch_size_deserialization_accepts_valid_boundaries() {
        assert_eq!(serde_json::from_str::<BatchSize>("1").unwrap().get(), 1);
        assert_eq!(serde_json::from_str::<BatchSize>("100").unwrap().get(), 100);
    }

    #[test]
    fn absent_customer_and_address_fields_are_omitted_from_json() {
        let customer = CustomerMode::Manual {
            customer: ManualCustomer::default(),
        };

        assert_eq!(
            serde_json::to_value(customer).unwrap(),
            serde_json::json!({
                "mode": "manual",
                "customer": {},
            })
        );
        assert_eq!(
            serde_json::to_value(MailingAddress::default()).unwrap(),
            serde_json::json!({})
        );
    }
}

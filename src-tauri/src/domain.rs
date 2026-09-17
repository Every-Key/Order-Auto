use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub phone: Option<String>,
    pub shipping_address: Option<MailingAddress>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailingAddress {
    pub address1: Option<String>,
    pub address2: Option<String>,
    pub city: Option<String>,
    pub province: Option<String>,
    pub country: Option<String>,
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
}

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct AppError {
    code: String,
    message: String,
    retryable: bool,
}

impl AppError {
    pub(crate) fn redacted(mut self, secret: &str) -> Self {
        if !secret.is_empty() {
            self.code = self.code.replace(secret, "[REDACTED]");
            self.message = self.message.replace(secret, "[REDACTED]");
        }
        self
    }

    pub(crate) fn transient(code: &'static str, message: &'static str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: true,
        }
    }

    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn retryable(&self) -> bool {
        self.retryable
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppErrorDto {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl From<AppError> for AppErrorDto {
    fn from(error: AppError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

impl From<&AppError> for AppErrorDto {
    fn from(error: &AppError) -> Self {
        Self {
            code: error.code.clone(),
            message: error.message.clone(),
            retryable: error.retryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_a_stable_safe_error_dto() {
        let dto = AppErrorDto::from(AppError::validation(
            "INVALID_BATCH_SIZE",
            "批量数量必须为 1 到 100",
        ));

        assert_eq!(
            serde_json::to_value(dto).unwrap(),
            serde_json::json!({
                "code": "INVALID_BATCH_SIZE",
                "message": "批量数量必须为 1 到 100",
                "retryable": false,
            })
        );
    }
}

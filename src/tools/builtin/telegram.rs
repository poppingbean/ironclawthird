//! Telegram notification tool for the crypto_trading skill.
//!
//! Sends messages to a configured Telegram chat via the Bot API.
//! Reads `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` from the environment.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use serde_json::Value;

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

/// Sends a text message to a Telegram chat via the Bot API.
///
/// Reads `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` from environment.
/// An explicit `chat_id` parameter overrides the env var.
pub struct TelegramNotifyTool {
    client: Client,
}

impl TelegramNotifyTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for TelegramNotifyTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TelegramNotifyTool {
    fn name(&self) -> &str {
        "telegram_notify"
    }

    fn description(&self) -> &str {
        "Send a text message to a Telegram chat using the Bot API. \
         Used by the crypto_trading skill to deliver trade signals and \
         position-close alerts. Only call when a new signal fires (score >= 5) \
         or a position is closed -- never for HOLD results. \
         Requires TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Message text. May include \\n and basic HTML (<b>, <i>, <code>). Must contain real values, not placeholders."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Override TELEGRAM_CHAT_ID for this message."
                },
                "parse_mode": {
                    "type": "string",
                    "enum": ["HTML", "Markdown"],
                    "default": "HTML"
                },
                "disable_notification": {
                    "type": "boolean",
                    "default": false,
                    "description": "Send silently."
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &JobContext) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let message = params["message"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidParameters("message is required".into()))?;
        if message.trim().is_empty() {
            return Err(ToolError::InvalidParameters(
                "message must not be empty".into(),
            ));
        }

        let bot_token = std::env::var("TELEGRAM_BOT_TOKEN")
            .map_err(|_| ToolError::NotAuthorized("TELEGRAM_BOT_TOKEN not set".into()))?;

        let chat_id = params["chat_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| std::env::var("TELEGRAM_CHAT_ID").ok())
            .ok_or_else(|| {
                ToolError::NotAuthorized("No chat_id and TELEGRAM_CHAT_ID not set".into())
            })?;

        let parse_mode = params["parse_mode"].as_str().unwrap_or("HTML");
        let silent = params["disable_notification"].as_bool().unwrap_or(false);

        let url = format!("{TELEGRAM_API_BASE}/bot{bot_token}/sendMessage");
        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": message,
            "parse_mode": parse_mode,
            "disable_notification": silent
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Telegram request failed: {e}")))?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({"ok": false}));

        if !status.is_success() || !resp_body["ok"].as_bool().unwrap_or(false) {
            let desc = resp_body["description"].as_str().unwrap_or("unknown error");
            return Err(ToolError::ExternalService(format!(
                "Telegram API {status}: {desc}"
            )));
        }

        let msg_id = resp_body["result"]["message_id"]
            .as_i64()
            .unwrap_or_default();

        let result = serde_json::json!({
            "ok": true,
            "message_id": msg_id,
            "chat_id": chat_id,
            "characters_sent": message.len()
        });
        Ok(ToolOutput::success(result, start.elapsed()))
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn requires_approval(&self, _params: &Value) -> ApprovalRequirement {
        ApprovalRequirement::UnlessAutoApproved
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_requires_message() {
        let schema = TelegramNotifyTool::new().parameters_schema();
        let req = schema["required"].as_array().unwrap();
        assert!(req.iter().any(|v| v == "message"));
    }

    #[test]
    fn test_name() {
        assert_eq!(TelegramNotifyTool::new().name(), "telegram_notify");
    }
}

//! Internal bridge for the existing PTC ISMS worker.
//!
//! The endpoint is intentionally separate from the public Teams webhook.
//! The worker submits one request and waits for the matching OpenAB reply.

use crate::schema::{ChannelInfo, GatewayEvent, GatewayReply, SenderInfo};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, Mutex};
use tracing::{info, warn};

pub const SECRET_HEADER: &str = "x-ptc-isms-secret";
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 180;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PtcIsmsRequest {
    pub request_id: String,
    pub text: String,
    #[serde(default = "default_sender_id")]
    pub sender_id: String,
    #[serde(default = "default_sender_name")]
    pub sender_name: String,
    #[serde(default = "default_channel_id")]
    pub channel_id: String,
}

fn default_sender_id() -> String {
    "jetson-isms-worker".into()
}

fn default_sender_name() -> String {
    "PTC ISMS Worker".into()
}

fn default_channel_id() -> String {
    "ptc-isms".into()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtcIsmsResponse {
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct PtcIsmsConfig {
    pub secret: String,
    pub webhook_path: String,
    pub timeout: Duration,
}

pub struct PendingResponse {
    pub request_id: String,
    pub sender: oneshot::Sender<PtcIsmsResponse>,
}

pub type PendingResponses = Arc<Mutex<HashMap<String, PendingResponse>>>;

pub fn new_pending_responses() -> PendingResponses {
    Arc::new(Mutex::new(HashMap::new()))
}

impl PtcIsmsConfig {
    pub fn from_env() -> Option<Self> {
        let secret = std::env::var("PTC_ISMS_BRIDGE_SECRET").ok()?;
        if secret.trim().is_empty() {
            return None;
        }

        let timeout_secs = std::env::var("PTC_ISMS_BRIDGE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        Some(Self {
            secret,
            webhook_path: std::env::var("PTC_ISMS_BRIDGE_WEBHOOK_PATH")
                .unwrap_or_else(|_| "/webhook/ptc-isms".into()),
            timeout: Duration::from_secs(timeout_secs),
        })
    }
}

fn valid_secret(expected: &str, received: Option<&str>) -> bool {
    let Some(received) = received else {
        return false;
    };
    expected.as_bytes().ct_eq(received.as_bytes()).into()
}

fn response(
    status: StatusCode,
    request_id: impl Into<String>,
    answer: Option<String>,
    error: Option<String>,
) -> impl IntoResponse {
    (
        status,
        Json(PtcIsmsResponse {
            request_id: request_id.into(),
            answer,
            error,
        }),
    )
}

pub async fn webhook(
    State(state): State<Arc<crate::AppState>>,
    headers: HeaderMap,
    Json(request): Json<PtcIsmsRequest>,
) -> impl IntoResponse {
    let Some(config) = state.ptc_isms.as_ref() else {
        return response(
            StatusCode::NOT_FOUND,
            request.request_id,
            None,
            Some("ptc-isms bridge is not configured".into()),
        );
    };

    if !valid_secret(
        config.secret.as_str(),
        headers.get(SECRET_HEADER).and_then(|v| v.to_str().ok()),
    ) {
        return response(
            StatusCode::UNAUTHORIZED,
            request.request_id,
            None,
            Some("invalid bridge secret".into()),
        );
    }

    if request.request_id.trim().is_empty()
        || request.request_id.len() > MAX_REQUEST_ID_BYTES
        || request.text.trim().is_empty()
        || request.text.len() > MAX_PROMPT_BYTES
    {
        return response(
            StatusCode::BAD_REQUEST,
            request.request_id,
            None,
            Some("request_id and text are required and must be within limits".into()),
        );
    }

    let event = GatewayEvent::new(
        "ptc-isms",
        ChannelInfo {
            id: request.channel_id,
            channel_type: "private".into(),
            thread_id: None,
        },
        SenderInfo {
            id: request.sender_id,
            name: request.sender_name.clone(),
            display_name: request.sender_name,
            is_bot: false,
        },
        &request.text,
        &request.request_id,
        Vec::new(),
    );
    let event_id = event.event_id.clone();
    let event_json = match serde_json::to_string(&event) {
        Ok(value) => value,
        Err(error) => {
            return response(
                StatusCode::INTERNAL_SERVER_ERROR,
                request.request_id,
                None,
                Some(format!("failed to serialize gateway event: {error}")),
            );
        }
    };

    let (sender, receiver) = oneshot::channel();
    state.ptc_isms_pending.lock().await.insert(
        event_id,
        PendingResponse {
            request_id: request.request_id.clone(),
            sender,
        },
    );

    if state.event_tx.send(event_json).is_err() {
        state.ptc_isms_pending.lock().await.remove(&event.event_id);
        return response(
            StatusCode::SERVICE_UNAVAILABLE,
            request.request_id,
            None,
            Some("OpenAB Gateway has no active WebSocket consumer".into()),
        );
    }

    info!(request_id = %request.request_id, "PTC ISMS request forwarded to OpenAB");
    match tokio::time::timeout(config.timeout, receiver).await {
        Ok(Ok(reply)) => {
            if let Some(error) = reply.error {
                response(StatusCode::BAD_GATEWAY, reply.request_id, None, Some(error))
            } else {
                response(StatusCode::OK, reply.request_id, reply.answer, None)
            }
        }
        Ok(Err(_)) => response(
            StatusCode::BAD_GATEWAY,
            request.request_id,
            None,
            Some("OpenAB response channel closed".into()),
        ),
        Err(_) => {
            state.ptc_isms_pending.lock().await.remove(&event.event_id);
            warn!(request_id = %request.request_id, "OpenAB response timed out");
            response(
                StatusCode::GATEWAY_TIMEOUT,
                request.request_id,
                None,
                Some("OpenAB response timed out".into()),
            )
        }
    }
}

pub async fn handle_reply(reply: &GatewayReply, pending: &PendingResponses) {
    if reply.content.text.trim().is_empty() || reply.command.is_some() {
        return;
    }

    let entry = pending.lock().await.remove(&reply.reply_to);
    if let Some(entry) = entry {
        let _ = entry.sender.send(PtcIsmsResponse {
            request_id: entry.request_id,
            answer: Some(reply.content.text.clone()),
            error: None,
        });
    } else {
        warn!(reply_to = %reply.reply_to, "No pending PTC ISMS request for OpenAB reply");
    }
}

#[cfg(test)]
mod tests {
    use super::valid_secret;

    #[test]
    fn compares_shared_secret_without_plain_equality() {
        assert!(valid_secret("abc", Some("abc")));
        assert!(!valid_secret("abc", Some("abd")));
        assert!(!valid_secret("abc", None));
    }
}

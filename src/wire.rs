//! Wire formats: OpenAI/Azure-style chat completions (cloud path) and Ollama chat (edge path).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::SecurityEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }
}

// ---------------------------------------------------------------- cloud / OpenAI-compatible

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    /// Reasoning models only (gpt-5 family, o-series): "minimal" | "low" | "medium" | "high".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Enterprise wrapper envelope (tenant claims, routing, compliance, grounding). Only sent to
    /// the mock gateway — real OpenAI/Azure endpoints would reject the unknown field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x_enterprise_context: Option<EnterpriseContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterpriseContext {
    pub tenant_id: String,
    pub correlation_id: String,
    pub data_classification: String,
    pub sensitivity_label: String,
    pub user_claims: Value,
    pub routing: Value,
    pub compliance: Value,
    pub grounding: Vec<String>,
    pub event: SecurityEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: i64,
    #[serde(default)]
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_filter_results: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub index: u32,
    pub message: ResponseMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    /// Azure OpenAI extension (undocumented): server-side timing of the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_checkpoint: Option<LatencyCheckpoint>,
}

/// Azure's per-call timing. Observed semantics: `engine_*` is the model engine itself, `service_*`
/// the Azure OpenAI service as a whole (routing, content filtering, queueing + engine),
/// `pre_inference_ms` the work before the engine starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencyCheckpoint {
    #[serde(default)]
    pub engine_ttlt_ms: f64,
    #[serde(default)]
    pub service_ttlt_ms: f64,
    #[serde(default)]
    pub pre_inference_ms: f64,
}

// ---------------------------------------------------------------- edge / Ollama

#[derive(Debug, Serialize)]
pub struct OllamaChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    pub keep_alive: String,
    pub options: OllamaOptions,
}

#[derive(Debug, Serialize)]
pub struct OllamaOptions {
    pub temperature: f32,
    pub num_predict: i32,
    pub num_ctx: u32,
}

#[derive(Debug, Deserialize)]
pub struct OllamaChatResponse {
    pub message: ChatMessage,
    /// Nanoseconds.
    #[serde(default)]
    pub total_duration: u64,
    #[serde(default)]
    pub load_duration: u64,
    #[serde(default)]
    pub prompt_eval_count: u64,
    #[serde(default)]
    pub prompt_eval_duration: u64,
    #[serde(default)]
    pub eval_count: u64,
    #[serde(default)]
    pub eval_duration: u64,
}

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Ollama Responses API request structure
/// Based on Ollama's responses.go format
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub background: bool, // originally: optional, default is false
    #[serde(default)]
    pub conversation: Option<Value>, // originally: optional `string | {id: string}`
    #[serde(default)]
    pub include: Vec<String>, // originally: string[], ignored
    pub input: ResponsesInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>, // optional, inserts a system message at the start
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i32>, // optional, maps to num_predict
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>, // optional, default is 1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<ResponsesText>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>, // optional, default is 1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<String>, // optional, default is "disabled"
    #[serde(default)]
    pub tools: Vec<ResponsesTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>, // optional, default is false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    Text(String),
    Items(Vec<InputItem>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        #[serde(default)]
        content: Option<ResponsesContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        #[serde(rename = "id")]
        id: Option<String>,
        #[serde(rename = "call_id")]
        call_id: String,
        name: String,
        arguments: String, // JSON arguments string
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        #[serde(rename = "call_id")]
        call_id: String,
        output: FunctionCallOutputValue,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: Option<String>,
        #[serde(rename = "encrypted_content")]
        encrypted_content: String,
        #[serde(rename = "summary")]
        summary: Option<Vec<ResponsesReasoningSummary>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Text(String),
    Array(Vec<ResponsesContentItem>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesContentItem {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        detail: String, // required
        #[serde(rename = "file_id")]
        file_id: Option<String>, // optional
        #[serde(rename = "image_url")]
        image_url: Option<String>, // optional
    },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_file")]
    InputFile {
        #[serde(rename = "file_data")]
        file_data: Option<String>,
        #[serde(rename = "file_id")]
        file_id: Option<String>,
        #[serde(rename = "file_url")]
        file_url: Option<String>,
        filename: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FunctionCallOutputValue {
    Text(String),
    Content(Vec<ResponsesContentItem>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    // originally: optional, default is per-model
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    // originally: deprecated, use `summary` instead. One of `auto`, `concise`, `detailed`
    #[serde(rename = "generate_summary", skip_serializing_if = "Option::is_none")]
    pub generate_summary: Option<String>,
    // originally: optional, one of `auto`, `concise`, `detailed`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTextFormat {
    #[serde(rename = "type")]
    pub format_type: String, // "text", "json_schema"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>, // for json_schema
    #[serde(rename = "schema", skip_serializing_if = "Option::is_none")]
    pub format_schema: Option<Value>, // for json_schema
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>, // for json_schema
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesText {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ResponsesTextFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub tool_type: String, // "function"
    pub name: Option<String>, // Make name optional to handle missing field
    pub description: Option<String>, // nullable but required
    pub strict: Option<bool>, // nullable but required
    #[serde(default)]
    pub parameters: Value, // nullable but required, with default
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoningSummary {
    #[serde(rename = "type")]
    pub summary_type: String, // "summary_text"
    pub text: String,
}

/// Ollama Responses API response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesResponse {
    pub id: String,
    #[serde(rename = "object")]
    pub response_object: String,
    #[serde(rename = "created_at")]
    pub created_at: i64,
    #[serde(rename = "completed_at")]
    pub completed_at: Option<i64>,
    pub status: String,
    #[serde(rename = "incomplete_details")]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    pub model: String,
    #[serde(rename = "previous_response_id")]
    pub previous_response_id: Option<String>,
    pub instructions: Option<String>,
    pub output: Vec<ResponsesOutputItem>,
    pub error: Option<ResponsesError>,
    pub tools: Vec<ResponsesTool>,
    #[serde(rename = "tool_choice")]
    pub tool_choice: Value,
    pub truncation: String,
    #[serde(rename = "parallel_tool_calls")]
    pub parallel_tool_calls: bool,
    pub text: ResponsesTextField,
    #[serde(rename = "top_p")]
    pub top_p: f64,
    #[serde(rename = "presence_penalty")]
    pub presence_penalty: f64,
    #[serde(rename = "frequency_penalty")]
    pub frequency_penalty: f64,
    #[serde(rename = "top_logprobs")]
    pub top_logprobs: i32,
    pub temperature: f64,
    pub reasoning: Option<ResponsesReasoningOutput>,
    pub usage: ResponsesUsage,
    #[serde(rename = "max_output_tokens")]
    pub max_output_tokens_field: Option<i32>,
    #[serde(rename = "max_tool_calls")]
    pub max_tool_calls: Option<i32>,
    pub store: bool,
    pub background: bool,
    #[serde(rename = "service_tier")]
    pub service_tier: String,
    pub metadata: Value,
    #[serde(rename = "safety_identifier")]
    pub safety_identifier: Option<String>,
    #[serde(rename = "prompt_cache_key")]
    pub prompt_cache_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesOutputItem {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String, // "message", "function_call", or "reasoning"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>, // for message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ResponsesOutputContent>>, // for message
    #[serde(rename = "call_id", skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>, // for function_call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>, // for function_call
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>, // for function_call
    // Reasoning fields
    #[serde(rename = "summary", skip_serializing_if = "Option::is_none")]
    pub summary: Option<Vec<ResponsesReasoningSummary>>,
    #[serde(rename = "encrypted_content", skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>, // for reasoning
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesOutputContent {
    #[serde(rename = "type")]
    pub content_type: String, // "output_text"
    pub text: String,
    pub annotations: Vec<Value>,
    pub logprobs: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(rename = "cached_tokens")]
    pub cached_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesOutputTokensDetails {
    #[serde(rename = "reasoning_tokens")]
    pub reasoning_tokens: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesUsage {
    #[serde(rename = "input_tokens")]
    pub input_tokens: i32,
    #[serde(rename = "output_tokens")]
    pub output_tokens: i32,
    #[serde(rename = "total_tokens")]
    pub total_tokens: i32,
    #[serde(rename = "input_tokens_details")]
    pub input_tokens_details: ResponsesInputTokensDetails,
    #[serde(rename = "output_tokens_details")]
    pub output_tokens_details: ResponsesOutputTokensDetails,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesIncompleteDetails {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoningOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTextField {
    pub format: ResponsesTextFormat,
}

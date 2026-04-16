use crate::models::openai;

// Re-export anthropic transform functions for backward compatibility
pub use crate::anthropic::transform::{
    anthropic_to_openai, clean_schema, map_stop_reason, openai_to_anthropic,
};

/// Apply model override based on config for OpenAI requests
/// Detects reasoning requests by checking reasoning_effort parameter
pub fn apply_openai_model_override(
    mut req: openai::OpenAIRequest,
    reasoning_model: Option<String>,
    completion_model: Option<String>,
) -> openai::OpenAIRequest {
    // Check if this is a reasoning request based on reasoning_effort
    let is_reasoning = req.reasoning_effort.as_ref().map_or(false, |effort| {
        !matches!(effort, openai::ReasoningEffort::None)
    });

    // Override model if provided
    let model = if is_reasoning {
        reasoning_model.clone().unwrap_or_else(|| req.model.clone())
    } else {
        completion_model
            .clone()
            .unwrap_or_else(|| req.model.clone())
    };

    req.model = model;
    req
}

use crate::config::Config;
use crate::error::Result;
use crate::hooks::pii_guardrail_hook;
use crate::models::openai;
use crate::transform;
use serde_json::Value;
use tokenx_rs::estimate_token_count;

/// Hook stage identifier - represents different stages in the request/response pipeline
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookStage {
    /// Raw request body received from client (JSON)
    RequestReceived,
    /// AnthropicRequest before transformation to OpenAI format
    BeforeTransform,
    /// OpenAIRequest before sending to upstream API
    BeforeUpstream,
    /// OpenAIResponse received from upstream API
    UpstreamResponse,
    /// AnthropicResponse before sending back to client
    BeforeResponse,
}

/// A hook is a function that transforms data at a specific stage
/// It takes the current data (as JSON Value) and config, returns modified data
pub type Hook = fn(Value, &Config) -> Result<Value>;

/// Hook executor that runs a chain of hooks
#[derive(Debug, Clone)]
pub struct HookChain {
    hooks: Vec<(HookStage, &'static str, Hook)>,
}

impl HookChain {
    /// Create a new empty hook chain
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Add a hook to the chain for a specific stage
    pub fn with_hook(mut self, stage: HookStage, name: &'static str, hook: Hook) -> Self {
        self.hooks.push((stage, name, hook));
        self
    }

    /// Execute all hooks for a given stage
    /// Runs hooks in the order they were added
    pub fn execute(&self, stage: HookStage, data: Value, config: &Config) -> Result<Value> {
        let mut result = data;
        for (hook_stage, name, hook) in &self.hooks {
            if *hook_stage == stage {
                tracing::debug!("Executing hook '{}' at stage {:?}", name, stage);
                result = hook(result, config)?;
            }
        }
        Ok(result)
    }
}

impl Default for HookChain {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Built-in Hooks
// =============================================================================

/// Hook: Log the data at current stage (for debugging)
/// Saves JSON to temp directory for analysis
pub fn logging_hook(data: Value, _config: &Config) -> Result<Value> {
    let json = serde_json::to_string_pretty(&data).unwrap_or_default();
    tracing::trace!("Hook data: {}", json);
    Ok(data)
}

/// Hook: Log provider and model information from request data
pub fn provider_model_logging_hook(data: Value, config: &Config) -> Result<Value> {
    let model_info = data.get("model").and_then(|val| val.as_str()).unwrap_or("");
    let provider_info = &config.active_provider;

    tracing::info!("Provider: {}, Model: {}", provider_info, model_info);

    Ok(data)
}

pub fn calculate_tokens_hook(data: Value, _config: &Config) -> Result<Value> {
    let text = serde_json::to_string(&data).unwrap_or_default();
    let tokens = estimate_token_count(&text);
    tracing::info!("Estimated the number of tokens: {tokens}");
    Ok(data)
}

/// Hook: Transform OpenAI response to Anthropic format
pub fn openai_to_anthropic_hook(data: Value, _config: &Config) -> Result<Value> {
    let openai_resp: openai::OpenAIResponse = serde_json::from_value(data.clone())?;
    let anthropic_resp = transform::openai_to_anthropic(openai_resp)?;
    let result: Value = serde_json::to_value(anthropic_resp)?;
    tracing::trace!(
        "Transformed Anthropic response: {}",
        serde_json::to_string_pretty(&result).unwrap_or_default()
    );
    Ok(result)
}

/// Default hook chain with logging at all stages
/// This creates a chain with logging hooks at appropriate stages
pub fn default_chain() -> HookChain {
    HookChain::new()
        .with_hook(HookStage::RequestReceived, "logging", logging_hook)
        .with_hook(HookStage::RequestReceived, "pii", pii_guardrail_hook)
        .with_hook(
            HookStage::RequestReceived,
            "calculate_tokens",
            calculate_tokens_hook,
        )
        .with_hook(HookStage::BeforeTransform, "logging", logging_hook)
        .with_hook(
            HookStage::BeforeUpstream,
            "provider_model",
            provider_model_logging_hook,
        )
        .with_hook(
            HookStage::UpstreamResponse,
            "openai_to_anthropic",
            openai_to_anthropic_hook,
        )
        .with_hook(HookStage::UpstreamResponse, "logging", logging_hook)
        .with_hook(HookStage::BeforeResponse, "logging", logging_hook)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_hook_stage_equality() {
        assert_eq!(HookStage::RequestReceived, HookStage::RequestReceived);
        assert_eq!(HookStage::BeforeTransform, HookStage::BeforeTransform);
        assert_ne!(HookStage::RequestReceived, HookStage::BeforeUpstream);
    }

    #[test]
    fn test_hook_chain_new() {
        let chain = HookChain::new();
        assert_eq!(chain.hooks.len(), 0);
    }

    #[test]
    fn test_hook_chain_with_hook() {
        let chain =
            HookChain::new().with_hook(HookStage::RequestReceived, "test_hook", logging_hook);
        assert_eq!(chain.hooks.len(), 1);
    }

    #[test]
    fn test_hook_chain_execute_runs_matching_hooks() {
        let chain = HookChain::new()
            .with_hook(HookStage::RequestReceived, "logging", logging_hook)
            .with_hook(HookStage::BeforeUpstream, "logging", logging_hook);

        let data = json!({"test": "data"});
        let config = Config::default();

        // Execute for RequestReceived stage - should run 1 hook
        let result = chain.execute(HookStage::RequestReceived, data.clone(), &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_hook_chain_execute_skips_non_matching_hooks() {
        let chain = HookChain::new().with_hook(HookStage::BeforeTransform, "logging", logging_hook);

        let data = json!({"test": "data"});
        let config = Config::default();

        // Execute for different stage - should skip the hook
        let result = chain.execute(HookStage::RequestReceived, data, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_logging_hook_passes_through_data() {
        let data = json!({
            "model": "claude-4-opus",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "hello"}]
        });
        let config = Config::default();

        let result = logging_hook(data.clone(), &config);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), data);
    }

    #[test]
    fn test_hook_chain_builder_pattern() {
        let chain = HookChain::new()
            .with_hook(HookStage::RequestReceived, "hook1", logging_hook)
            .with_hook(HookStage::BeforeTransform, "hook2", logging_hook)
            .with_hook(HookStage::BeforeUpstream, "hook3", logging_hook);

        assert_eq!(chain.hooks.len(), 3);

        let data = json!({"test": "data"});
        let config = Config::default();

        // Execute each stage
        let _ = chain.execute(HookStage::RequestReceived, data.clone(), &config);
        let _ = chain.execute(HookStage::BeforeTransform, data.clone(), &config);
        let _ = chain.execute(HookStage::BeforeUpstream, data.clone(), &config);
    }

    #[test]
    fn test_empty_chain_execute() {
        let chain = HookChain::new();
        let data = json!({"test": "data"});
        let config = Config::default();

        let result = chain.execute(HookStage::RequestReceived, data, &config);
        assert!(result.is_ok());
    }
}

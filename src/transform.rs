use crate::error::{Error as KlavaError, Result as KlavaResult};
use crate::models::{anthropic, openai, responses};
use serde_json::{Value, json};

/// Transform Anthropic request to OpenAI format
pub fn anthropic_to_openai(
    req: anthropic::AnthropicRequest,
    reasoning_model: Option<String>,
    completion_model: Option<String>,
) -> KlavaResult<openai::OpenAIRequest> {
    // Determine model based on thinking parameter
    let has_thinking = req
        .extra
        .get("thinking")
        .and_then(|v| v.as_object())
        .map(|o| o.get("type").and_then(|t| t.as_str()) == Some("enabled"))
        .unwrap_or(false);

    // Use provided model or fall back to the model from the request
    let model = if has_thinking {
        reasoning_model
            .clone()
            .or_else(|| Some(req.model.clone()))
            .unwrap_or_else(|| req.model.clone())
    } else {
        completion_model
            .clone()
            .or_else(|| Some(req.model.clone()))
            .unwrap_or_else(|| req.model.clone())
    };

    // Convert messages
    let mut openai_messages = Vec::new();

    // Add system message if present
    if let Some(system) = req.system {
        match system {
            anthropic::SystemPrompt::Single(text) => {
                openai_messages.push(openai::Message {
                    role: "system".to_string(),
                    content: Some(openai::MessageContent::Text(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            anthropic::SystemPrompt::Multiple(messages) => {
                for msg in messages {
                    openai_messages.push(openai::Message {
                        role: "system".to_string(),
                        content: Some(openai::MessageContent::Text(msg.text)),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
        }
    }

    // Convert user/assistant messages
    for msg in req.messages {
        let converted = convert_message(msg)?;
        openai_messages.extend(converted);
    }

    // Convert tools
    let tools = req.tools.and_then(|tools| {
        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|t| t.tool_type.as_deref() != Some("BatchTool"))
            .collect();

        if filtered.is_empty() {
            None
        } else {
            Some(
                filtered
                    .into_iter()
                    .map(|t| openai::Tool {
                        tool_type: "function".to_string(),
                        function: openai::Function {
                            name: t.name,
                            description: t.description,
                            parameters: clean_schema(t.input_schema),
                        },
                    })
                    .collect(),
            )
        }
    });

    Ok(openai::OpenAIRequest {
        model,
        messages: openai_messages,
        max_tokens: Some(req.max_tokens),
        temperature: req.temperature,
        top_p: req.top_p,
        stop: req.stop_sequences,
        stream: req.stream,
        tools,
        tool_choice: None,
        reasoning_effort: None,
    })
}

/// Convert a single Anthropic message to one or more OpenAI messages
fn convert_message(msg: anthropic::Message) -> KlavaResult<Vec<openai::Message>> {
    let mut result = Vec::new();

    match msg.content {
        anthropic::MessageContent::Text(text) => {
            result.push(openai::Message {
                role: msg.role,
                content: Some(openai::MessageContent::Text(text)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        anthropic::MessageContent::Blocks(blocks) => {
            let mut current_content_parts = Vec::new();
            let mut tool_calls = Vec::new();

            for block in blocks {
                match block {
                    anthropic::ContentBlock::Text { text, .. } => {
                        current_content_parts.push(openai::ContentPart::Text { text });
                    }
                    anthropic::ContentBlock::Image { source } => {
                        let data_url = format!("data:{};base64,{}", source.media_type, source.data);
                        current_content_parts.push(openai::ContentPart::ImageUrl {
                            image_url: openai::ImageUrl { url: data_url },
                        });
                    }
                    anthropic::ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(openai::ToolCall {
                            id,
                            call_type: "function".to_string(),
                            function: openai::FunctionCall {
                                name,
                                arguments: serde_json::to_string(&input)
                                    .map_err(|e| KlavaError::Serialization(e))?,
                            },
                        });
                    }
                    anthropic::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        // Tool results become separate messages with role "tool"
                        let text = serde_json::to_string(&content)
                            .map_err(|e| KlavaError::Serialization(e))?;
                        result.push(openai::Message {
                            role: "tool".to_string(),
                            content: Some(openai::MessageContent::Text(text)),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id),
                            name: None,
                        });
                    }
                    anthropic::ContentBlock::Thinking { .. } => {
                        // Skip thinking blocks in request
                    }
                    anthropic::ContentBlock::Unknown => {
                        // Skip unknown content block types
                    }
                }
            }

            // Add message with content and/or tool calls
            if !current_content_parts.is_empty() || !tool_calls.is_empty() {
                let content = if current_content_parts.is_empty() {
                    None
                } else if current_content_parts.len() == 1 {
                    match &current_content_parts[0] {
                        openai::ContentPart::Text { text } => {
                            Some(openai::MessageContent::Text(text.clone()))
                        }
                        _ => Some(openai::MessageContent::Parts(current_content_parts)),
                    }
                } else {
                    Some(openai::MessageContent::Parts(current_content_parts))
                };

                result.push(openai::Message {
                    role: msg.role,
                    content,
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                    name: None,
                });
            }
        }
    }

    Ok(result)
}

/// Clean JSON schema by removing unsupported formats
fn clean_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        // Remove "format": "uri"
        if obj.get("format").and_then(|v| v.as_str()) == Some("uri") {
            obj.remove("format");
        }

        // Recursively clean nested schemas
        if let Some(properties) = obj.get_mut("properties").and_then(|v| v.as_object_mut()) {
            for (_, value) in properties.iter_mut() {
                *value = clean_schema(value.clone());
            }
        }

        if let Some(items) = obj.get_mut("items") {
            *items = clean_schema(items.clone());
        }
    }

    schema
}

/// Transform OpenAI response to Anthropic format
pub fn openai_to_anthropic(
    resp: openai::OpenAIResponse,
) -> KlavaResult<anthropic::AnthropicResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| KlavaError::Transform("No choices in response".to_string()))?;

    let mut content = Vec::new();

    // Add text content if present
    if let Some(text) = &choice.message.content {
        if !text.is_empty() {
            content.push(anthropic::ResponseContent::Text {
                content_type: "text".to_string(),
                text: text.clone(),
            });
        }
    }

    // Add tool calls if present
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tool_call in tool_calls {
            let input: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            content.push(anthropic::ResponseContent::ToolUse {
                content_type: "tool_use".to_string(),
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
                input,
            });
        }
    }

    let stop_reason = choice
        .finish_reason
        .as_ref()
        .map(|r| match r.as_str() {
            "tool_calls" => "tool_use",
            "stop" => "end_turn",
            "length" => "max_tokens",
            _ => "end_turn",
        })
        .map(String::from);

    Ok(anthropic::AnthropicResponse {
        id: resp.id,
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: resp.model,
        stop_reason,
        stop_sequence: None,
        usage: anthropic::Usage {
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
        },
    })
}

/// Map OpenAI finish reason to Anthropic stop reason
pub fn map_stop_reason(finish_reason: Option<&str>) -> Option<String> {
    finish_reason.map(|r| {
        match r {
            "tool_calls" => "tool_use",
            "stop" => "end_turn",
            "length" => "max_tokens",
            _ => "end_turn",
        }
        .to_string()
    })
}

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

/// Transform Responses request to OpenAI format
pub fn responses_to_openai(req: responses::ResponsesRequest) -> KlavaResult<openai::OpenAIRequest> {
    // Convert input items to OpenAI messages
    let mut openai_messages = Vec::new();

    // Handle the ResponsesInput enum which can be either a single string or array of InputItems
    match req.input {
        responses::ResponsesInput::Text(text) => {
            // Single text input becomes a user message
            openai_messages.push(openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text(text)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        responses::ResponsesInput::Items(input_items) => {
            for input_item in input_items {
                match input_item {
                    responses::InputItem::Message { role, content } => {
                        // Convert the content field which is Option<ResponsesContent>
                        let content_text = match content {
                            Some(responses::ResponsesContent::Text(text)) => text,
                            Some(responses::ResponsesContent::Array(content_array)) => {
                                // Convert array of content items to text representation
                                content_array
                                    .iter()
                                    .map(|item| match item {
                                        responses::ResponsesContentItem::InputText { text } => {
                                            text.clone()
                                        }
                                        responses::ResponsesContentItem::OutputText { text } => {
                                            text.clone()
                                        }
                                        responses::ResponsesContentItem::InputImage { .. } => {
                                            "[IMAGE]".to_string()
                                        }
                                        responses::ResponsesContentItem::InputFile { .. } => {
                                            "[FILE]".to_string()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            }
                            None => "".to_string(), // Empty content if none provided
                        };

                        openai_messages.push(openai::Message {
                            role,
                            content: Some(openai::MessageContent::Text(content_text)),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::InputItem::FunctionCall {
                        id: _,
                        call_id,
                        name,
                        arguments,
                    } => {
                        // Convert function call to tool call in a message
                        openai_messages.push(openai::Message {
                            role: "assistant".to_string(),
                            content: None,
                            tool_calls: Some(vec![openai::ToolCall {
                                id: call_id, // Use the provided call_id
                                call_type: "function".to_string(),
                                function: openai::FunctionCall { name, arguments },
                            }]),
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::InputItem::FunctionCallOutput { call_id, output } => {
                        // Add as tool message
                        let content_str = match output {
                            responses::FunctionCallOutputValue::Text(text) => text,
                            responses::FunctionCallOutputValue::Content(content_array) => {
                                // Convert array of content items to text representation
                                content_array
                                    .iter()
                                    .map(|item| match item {
                                        responses::ResponsesContentItem::InputText { text } => {
                                            text.clone()
                                        }
                                        responses::ResponsesContentItem::OutputText { text } => {
                                            text.clone()
                                        }
                                        responses::ResponsesContentItem::InputImage { .. } => {
                                            "[IMAGE]".to_string()
                                        }
                                        responses::ResponsesContentItem::InputFile { .. } => {
                                            "[FILE]".to_string()
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            }
                        };

                        openai_messages.push(openai::Message {
                            role: "tool".to_string(),
                            content: Some(openai::MessageContent::Text(content_str)),
                            tool_calls: None,
                            tool_call_id: Some(call_id),
                            name: None,
                        });
                    }
                    responses::InputItem::Reasoning {
                        id: _,
                        encrypted_content,
                        summary: _,
                    } => {
                        // Add reasoning content as a message
                        openai_messages.push(openai::Message {
                            role: "assistant".to_string(),
                            content: Some(openai::MessageContent::Text(format!(
                                "[REASONING: {}]",
                                encrypted_content
                            ))),
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                }
            }
        }
    }

    // Convert tools - req.tools is Vec<ResponsesTool> so we need to handle it differently
    let tools = if req.tools.is_empty() {
        None
    } else {
        Some(
            req.tools
                .into_iter()
                .filter_map(|t| {
                    // Only include tools with "function" type for compatibility with most providers
                    // Some providers don't support other types like "web_search"
                    if t.tool_type == "function" {
                        Some(openai::Tool {
                            tool_type: t.tool_type,
                            function: openai::Function {
                                name: t.name.unwrap_or_else(|| "unnamed_tool".to_string()),
                                description: t.description,
                                parameters: clean_schema(t.parameters),
                            },
                        })
                    } else {
                        // For non-function tools, map them to function type for broader compatibility
                        Some(openai::Tool {
                            tool_type: "function".to_string(), // Map all tools to function type for compatibility
                            function: openai::Function {
                                name: t.name.unwrap_or_else(|| "unnamed_tool".to_string()),
                                description: t.description,
                                parameters: clean_schema(t.parameters),
                            },
                        })
                    }
                })
                .collect(),
        )
    };

    Ok(openai::OpenAIRequest {
        model: req.model,
        messages: openai_messages,
        max_tokens: req
            .max_output_tokens
            .and_then(|x| if x > 0 { Some(x as u32) } else { None }), // Convert i32 to u32, only if > 0
        temperature: req.temperature.and_then(|x| {
            if x >= 0.0 && x <= 2.0 {
                Some(x as f32)
            } else {
                None
            } // Standard OpenAI temperature range
        }),
        top_p: req.top_p.and_then(|x| {
            if x >= 0.0 && x <= 1.0 {
                Some(x as f32)
            } else {
                None
            } // Standard OpenAI top_p range
        }),
        stop: None, // ResponsesRequest doesn't have an equivalent stop field
        stream: req.stream,
        tools,
        tool_choice: None,
        reasoning_effort: None, // Don't map reasoning effort directly, as many providers don't support it
    })
}

/// Transform OpenAI response to Responses format
pub fn openai_to_responses(
    resp: openai::OpenAIResponse,
) -> KlavaResult<responses::ResponsesResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| KlavaError::Transform("No choices in response".to_string()))?;

    // Create output items based on the OpenAI response
    let mut output_items = Vec::new();

    // Add the main message content if present
    if let Some(content) = &choice.message.content {
        output_items.push(responses::ResponsesOutputItem {
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            item_type: "message".to_string(),
            status: Some("completed".to_string()),
            role: Some(choice.message.role.clone()),
            content: Some(vec![responses::ResponsesOutputContent {
                content_type: "output_text".to_string(),
                text: content.clone(),
                annotations: vec![],
                logprobs: vec![],
            }]),
            call_id: None,
            name: None,
            arguments: None,
            summary: None,
            encrypted_content: None,
        });
    }

    // Add tool calls if present
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tool_call in tool_calls {
            let args: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            output_items.push(responses::ResponsesOutputItem {
                id: tool_call.id.clone(),
                item_type: "function_call".to_string(),
                status: Some("completed".to_string()),
                role: Some(choice.message.role.clone()),
                content: None,
                call_id: Some(tool_call.id.clone()),
                name: Some(tool_call.function.name.clone()),
                arguments: Some(serde_json::to_string(&args).unwrap_or_default()),
                summary: None,
                encrypted_content: None,
            });
        }
    }

    // Add tool calls if present
    if let Some(tool_calls) = &choice.message.tool_calls {
        for tool_call in tool_calls {
            let args: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            output_items.push(responses::ResponsesOutputItem {
                id: tool_call.id.clone(),
                item_type: "function_call".to_string(),
                status: Some("completed".to_string()),
                role: Some(choice.message.role.clone()),
                content: None,
                call_id: Some(tool_call.id.clone()),
                name: Some(tool_call.function.name.clone()),
                arguments: Some(serde_json::to_string(&args).unwrap_or_default()),
                summary: None,
                encrypted_content: None,
            });
        }
    }

    // Generate current timestamp as i64 (Unix timestamp)
    let now = chrono::Utc::now().timestamp();

    Ok(responses::ResponsesResponse {
        id: resp.id,
        response_object: "response".to_string(),
        created_at: now,
        completed_at: Some(now), // Set completed_at to the same time for now
        status: "completed".to_string(),
        incomplete_details: None,
        model: resp.model,
        previous_response_id: None,
        instructions: None,
        output: output_items,
        error: None,
        tools: vec![],          // We could populate this if needed
        tool_choice: json!({}), // Default empty object
        truncation: "disabled".to_string(),
        parallel_tool_calls: false,
        text: responses::ResponsesTextField {
            format: responses::ResponsesTextFormat {
                format_type: "text".to_string(),
                name: None,
                format_schema: None,
                strict: None,
            },
        },
        top_p: 1.0,
        presence_penalty: 0.0,
        frequency_penalty: 0.0,
        top_logprobs: 0,
        temperature: 1.0,
        reasoning: None,
        usage: responses::ResponsesUsage {
            input_tokens: resp.usage.prompt_tokens as i32,
            output_tokens: resp.usage.completion_tokens as i32,
            total_tokens: resp.usage.total_tokens as i32,
            input_tokens_details: responses::ResponsesInputTokensDetails {
                cached_tokens: 0, // Could be populated if cache info is available
            },
            output_tokens_details: responses::ResponsesOutputTokensDetails {
                reasoning_tokens: 0, // Could be populated if reasoning info is available
            },
        },
        max_output_tokens_field: None,
        max_tool_calls: None,
        store: false,
        background: false,
        service_tier: "default".to_string(),
        metadata: json!({}),
        safety_identifier: None,
        prompt_cache_key: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses;

    #[test]
    fn test_responses_to_openai_conversion() {
        let responses_req = responses::ResponsesRequest {
            model: "gpt-4".to_string(),
            background: false,
            conversation: None,
            include: vec![],
            input: responses::ResponsesInput::Text("Hello, world!".to_string()),
            instructions: None,
            max_output_tokens: Some(100),
            reasoning: Some(responses::ResponsesReasoning {
                effort: None,
                generate_summary: None,
                summary: None,
            }),
            temperature: Some(0.7),
            text: None,
            top_p: Some(0.9),
            truncation: None,
            tools: vec![],
            stream: Some(false),
        };

        let result = responses_to_openai(responses_req);
        assert!(result.is_ok());

        let openai_req = result.unwrap();
        assert_eq!(openai_req.model, "gpt-4");
        assert_eq!(openai_req.messages.len(), 1);
        assert_eq!(openai_req.max_tokens, Some(100));
        assert_eq!(openai_req.temperature, Some(0.7));
        assert_eq!(openai_req.top_p, Some(0.9));
    }

    #[test]
    fn test_responses_input_items_conversion() {
        let input_items = vec![responses::InputItem::Message {
            role: "user".to_string(),
            content: Some(responses::ResponsesContent::Text("Hello".to_string())),
        }];

        let responses_req = responses::ResponsesRequest {
            model: "gpt-4".to_string(),
            background: false,
            conversation: None,
            include: vec![],
            input: responses::ResponsesInput::Items(input_items),
            instructions: None,
            max_output_tokens: Some(100),
            reasoning: Some(responses::ResponsesReasoning {
                effort: None,
                generate_summary: None,
                summary: None,
            }),
            temperature: Some(0.7),
            text: None,
            top_p: Some(0.9),
            truncation: None,
            tools: vec![],
            stream: Some(false),
        };

        let result = responses_to_openai(responses_req);
        assert!(result.is_ok());

        let openai_req = result.unwrap();
        assert_eq!(openai_req.model, "gpt-4");
        assert_eq!(openai_req.messages.len(), 1);
        if let Some(content) = &openai_req.messages[0].content {
            match content {
                openai::MessageContent::Text(text) => {
                    assert_eq!(text, "Hello");
                }
                _ => panic!("Expected text content"),
            }
        } else {
            panic!("Expected content to be present");
        }
    }
}

use crate::error::{Error, Result};
use crate::models::responses::ResponsesRequest;
use crate::models::{openai, responses};
use crate::utils::clean_schema;
use serde_json::{Value, json};

/// Transform Responses request to OpenAI format
pub fn responses_to_openai(req: ResponsesRequest) -> Result<openai::OpenAIRequest> {
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
            .and_then(|x| if x > 0 { Some(x as u32) } else { None }),
        temperature: req
            .temperature
            .and_then(|x| (0.0..=2.0).contains(&x).then_some(x as f32)),
        top_p: req
            .top_p
            .and_then(|x| (0.0..=1.0).contains(&x).then_some(x as f32)),
        stop: None, // ResponsesRequest doesn't have an equivalent stop field
        stream: req.stream,
        tools,
        tool_choice: None,
        reasoning_effort: None, // Don't map reasoning effort directly, as many providers don't support it
    })
}

// TODO: NOT FINISHED & TESTED METHOD
/// Transform OpenAI response to Responses format
pub fn openai_to_responses(resp: openai::OpenAIResponse) -> Result<responses::ResponsesResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| Error::Transform("No choices in response".to_string()))?;

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

#[derive(Debug, thiserror::Error)]
pub enum ConverterError {
    #[error("Request configuration is required")]
    MissingRequest,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::responses::{self, ResponsesRequest};
    use serde_json::json;

    #[ignore]
    #[test]
    fn converts_responses_to_chat_basic() {
        let v = json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role":"system","content":"You are helpful."},
                {"role":"user","content":"Hi"},
                {"role":"assistant","content":"Hello"}
            ],
            "max_output_tokens": 123,
            "tools": [{
                "type":"function",
                "function": {
                    "name":"lookup",
                    "description":"Lookup a value",
                    "parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}
                }
            }],
            "tool_choice": {"type":"function","function":{"name":"lookup"}},
            "response_format": {"type":"json_object","schema":{"type":"object"}},
            "stream": false
        });

        let request: ResponsesRequest = serde_json::from_value(v).unwrap();
        let out = responses_to_openai(request).unwrap();
        println!("OYTPUT {:?}", out);
        assert_eq!(out.model, "gpt-4o-mini");
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.max_tokens, Some(123));
        assert!(out.tools.as_ref().unwrap().len() == 1);
        assert!(out.tool_choice.is_some());
        // assert!(out.response_format.is_some());
        assert_eq!(out.stream, Some(false));
    }

    #[test]
    fn from_responses_request_tools() {
        let req_json = json!({
            "model": "gpt-oss:20b",
            "input": "hello",
            "tools": [
                {
                    "type": "function",
                    "name": "shell",
                    "description": "Runs a shell command",
                    "strict": false,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "command": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "The command to execute"
                            }
                        },
                        "required": ["command"]
                    }
                }
            ]
        });

        let req: ResponsesRequest = serde_json::from_value(req_json).unwrap();

        // Check that tools were parsed
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name.as_deref(), Some("shell"));

        // Convert and check
        let chat_req = responses_to_openai(req).unwrap();

        let tools = chat_req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);

        let tool = &tools[0];
        assert_eq!(tool.tool_type, "function");
        assert_eq!(tool.function.name, "shell");
        assert_eq!(
            tool.function.description.as_deref(),
            Some("Runs a shell command")
        );

        let params = &tool.function.parameters;
        assert_eq!(params.get("type").unwrap().as_str(), Some("object"));
        let required = params.get("required").unwrap().as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("command"));
    }

    #[test]
    fn from_responses_request_function_call_output() {
        // Test a complete tool call round-trip:
        // 1. User message asking about weather
        // 2. Assistant's function call (from previous response)
        // 3. Function call output (the tool result)
        let req_json = json!({
            "model": "gpt-oss:20b",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what is the weather?"}]},
                {"type": "function_call", "call_id": "call_abc123", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_abc123", "output": "sunny, 72F"}
            ]
        });

        let req: ResponsesRequest = serde_json::from_value(req_json).unwrap();

        // Check that input items were parsed
        if let responses::ResponsesInput::Items(items) = &req.input {
            assert_eq!(items.len(), 3);

            // Verify the function_call item
            if let responses::InputItem::FunctionCall { name, .. } = &items[1] {
                assert_eq!(name, "get_weather");
            } else {
                panic!("items[1] is not a FunctionCall");
            }

            // Verify the function_call_output item
            if let responses::InputItem::FunctionCallOutput { call_id, .. } = &items[2] {
                assert_eq!(call_id, "call_abc123");
            } else {
                panic!("items[2] is not a FunctionCallOutput");
            }
        } else {
            panic!("expected Items input");
        }

        // Convert and check
        let chat_req = responses_to_openai(req).unwrap();
        assert_eq!(chat_req.messages.len(), 3);

        // Check the user message
        assert_eq!(chat_req.messages[0].role, "user");

        // Check the assistant message with tool call
        assert_eq!(chat_req.messages[1].role, "assistant");
        let tool_calls = chat_req.messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_abc123");
        assert_eq!(tool_calls[0].function.name, "get_weather");

        // Check the tool response message
        assert_eq!(chat_req.messages[2].role, "tool");
        match chat_req.messages[2].content.as_ref() {
            Some(openai::MessageContent::Text(text)) => assert_eq!(text, "sunny, 72F"),
            _ => panic!("expected text content"),
        }
        assert_eq!(
            chat_req.messages[2].tool_call_id.as_deref(),
            Some("call_abc123")
        );
    }

    #[test]
    fn from_responses_request_function_call_output_content_array() {
        let req_json = json!({
            "model": "gpt-oss:20b",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "what is the weather?"}]},
                {"type": "function_call", "call_id": "call_abc123", "name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
                {"type": "function_call_output", "call_id": "call_abc123", "output": [
                    {"type": "input_text", "text": "sunny"},
                    {"type": "input_text", "text": ", 72F"}
                ]}
            ]
        });

        let req: ResponsesRequest = serde_json::from_value(req_json).unwrap();
        let chat_req = responses_to_openai(req).unwrap();

        assert_eq!(chat_req.messages.len(), 3);

        let tool_msg = &chat_req.messages[2];
        assert_eq!(tool_msg.role, "tool");
        match tool_msg.content.as_ref() {
            Some(openai::MessageContent::Text(text)) => assert_eq!(text, "sunny , 72F"),
            _ => panic!("expected text content"),
        }
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_abc123"));
    }

    #[test]
    fn from_responses_request_function_call_output_content_array_with_image() {
        // 1x1 red PNG pixel
        let png_base64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg==";

        let req_json = json!({
            "model": "gpt-oss:20b",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "inspect the image"}]},
                {"type": "function_call", "call_id": "call_abc123", "name": "inspect_image", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_abc123", "output": [
                    {"type": "input_text", "text": "attached image"},
                    {"type": "input_image", "detail": "auto", "image_url": format!("data:image/png;base64,{}", png_base64)}
                ]}
            ]
        });

        let req: ResponsesRequest = serde_json::from_value(req_json).unwrap();
        let chat_req = responses_to_openai(req).unwrap();

        assert_eq!(chat_req.messages.len(), 3);

        let tool_msg = &chat_req.messages[2];
        assert_eq!(tool_msg.role, "tool");
        // Images are converted to [IMAGE] placeholder in the text content
        match tool_msg.content.as_ref() {
            Some(openai::MessageContent::Text(text)) => {
                assert!(text.contains("attached image"));
                assert!(text.contains("[IMAGE]"));
            }
            _ => panic!("expected text content"),
        }
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("call_abc123"));
    }

    #[test]
    #[ignore = "waiting for reasoning+assistant+tool_call merge logic"]
    fn from_responses_request_function_call_merges_with_thinking() {
        // reasoning → assistant (gets thinking) → function_call → should merge
        let req_json = json!({
            "model": "gpt-oss:20b",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "think and act"}]},
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "Let me think...", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I thought about it."}]},
                {"type": "function_call", "call_id": "call_1", "name": "do_thing", "arguments": "{}"}
            ]
        });

        let req: ResponsesRequest = serde_json::from_value(req_json).unwrap();
        let chat_req = responses_to_openai(req).unwrap();

        // Should have 2 messages: user and assistant (thinking + content + tool call merged)
        assert_eq!(chat_req.messages.len(), 2);

        let asst = &chat_req.messages[1];
        assert_eq!(asst.role, "assistant");
        // Content should include reasoning + assistant text
        match asst.content.as_ref() {
            Some(openai::MessageContent::Text(text)) => {
                assert!(text.contains("Let me think..."));
                assert!(text.contains("I thought about it."));
            }
            _ => panic!("expected text content"),
        }
        // Tool call should be on the same assistant message
        let tool_calls = asst.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "do_thing");
    }

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

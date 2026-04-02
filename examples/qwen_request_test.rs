//! Qwen API Request Test
//!
//! Tests both streaming and non-streaming requests using Qwen auth headers.
//!
//! Run with:
//! ```bash
//! cargo run --example qwen_request_test --features qwen-free
//! ```

use futures::StreamExt;
use klava::qwen_auth::QwenAuth;
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize, Clone)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    // id: String,
    // object: String,
    // created: u64,
    model: String,
    choices: Vec<Choice>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct Choice {
    // index: u32,
    message: ChatMessage,
    // finish_reason: String,
}

#[derive(Debug, Deserialize)]
struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Qwen API Request Test Suite");
    println!();

    let mut qwen_auth = QwenAuth::new();

    if !qwen_auth.is_authenticated() {
        println!("❌ Not authenticated. Please run the auth flow first:");
        println!("   cargo run --example qwen_auth_flow --features qwen-free\n");
        return Ok(());
    }

    // Check if token needs refresh and handle it
    if qwen_auth.needs_refresh() {
        println!("⚠️  Token is expiring or expired, attempting to refresh...\n");
        match qwen_auth.get_token().await {
            Ok(_) => println!("✓ Token refreshed successfully\n"),
            Err(e) => {
                println!("❌ Failed to refresh token: {}\n", e);
                println!(
                    "   Please run: cargo run --example qwen_auth_flow --features qwen-free\n"
                );
                return Ok(());
            }
        }
    }

    println!("✓ Authenticated\n");

    // Get the base URL and auth headers
    let base_url = qwen_auth.get_base_url();
    println!("Using base URL: {}", base_url);

    let chat_url = format!("{}/chat/completions", base_url);

    // Create HTTP client
    let client = Client::new();

    // === Test: Non-streaming request ===
    println!("Test: Non-streaming request");
    println!();

    let request = ChatRequest {
        model: klava::qwen_auth::SUPPORTED_MODELS[0].to_string(),
        messages: vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are Qwen Code, an interactive CLI agent developed by Alibaba Group, specializing in software engineering tasks. Your primary goal is to help users safely and efficiently, adhering strictly to the following instructions and utilizing your available tools.".to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "What is 2+2? Keep your answer very brief.".to_string(),
            },
        ],
        stream: false,
        temperature: 0.7,
        max_tokens: 100,
    };

    println!("Sending request...");
    println!();

    let start_time = std::time::Instant::now();

    // Get headers from QwenAuth
    let headers = qwen_auth.get_auth_headers().await?;

    let response = client
        .post(&chat_url)
        .headers(headers)
        .header("Content-Type", "application/json")
        .json(&request)
        .send()
        .await?;

    let elapsed = start_time.elapsed();
    let status = response.status();

    if status.is_success() {
        let response_body: ChatResponse = response.json().await?;
        println!("✓ Non-streaming request successful!");
        println!("  Status: {}", status);
        println!("  Time: {:?}", elapsed);
        println!("  Model: {}", response_body.model);
        println!(
            "  Tokens: {} (prompt: {}, completion: {})",
            response_body.usage.total_tokens,
            response_body.usage.prompt_tokens,
            response_body.usage.completion_tokens
        );
        println!("  Response: {}", response_body.choices[0].message.content);
    } else {
        let error_text = response.text().await?;
        println!("✗ Non-streaming request failed!");
        println!("  Status: {}", status);
        println!("  Error: {}", error_text);
    }

    println!();
    println!();

    // === Test: Streaming request ===
    println!("Test 2: Streaming request");
    println!();

    let streaming_request = ChatRequest {
        stream: true,
        ..request.clone()
    };

    println!("Sending streaming request...");
    println!();
    println!("Streaming response:");
    println!("---");

    let start_time = std::time::Instant::now();

    // Get fresh headers for streaming
    let headers = qwen_auth.get_auth_headers().await?;

    let response = client
        .post(&chat_url)
        .headers(headers)
        .header("Content-Type", "application/json")
        .json(&streaming_request)
        .send()
        .await?;

    let elapsed = start_time.elapsed();
    let status = response.status();

    println!();

    if status.is_success() {
        println!("✓ Streaming request successful!");
        println!("  Status: {}", status);
        println!("  Time: {:?}", elapsed);
        println!();

        // Process streaming response
        let mut full_content = String::new();
        let mut chunk_count = 0;

        // Create the stream that takes ownership of response
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            for line in chunk_str.lines() {
                if line.starts_with("data: ") {
                    let data = &line[6..];
                    if data != "[DONE]" {
                        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                            if let Some(choices) = parsed.get("choices").and_then(|c| c.as_array())
                            {
                                if let Some(choice) = choices.first() {
                                    if let Some(delta) = choice.get("delta") {
                                        if let Some(content) =
                                            delta.get("content").and_then(|c| c.as_str())
                                        {
                                            print!("{}", content);
                                            std::io::Write::flush(&mut std::io::stdout()).ok();
                                            full_content.push_str(content);
                                            chunk_count += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        println!();
        println!();
        println!("✓ Streaming completed!");
        println!("  Chunks received: {}", chunk_count);
        println!("  Total length: {} chars", full_content.len());
        println!("  Full content: {}", full_content);
    } else {
        let error_text = response.text().await?;
        println!("✗ Streaming request failed!");
        println!("  Status: {}", status);
        println!("  Error: {}", error_text);
    }

    Ok(())
}

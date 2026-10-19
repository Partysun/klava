//! End-to-end probe: run the extension the way klava does.
//!
//! ```sh
//! cargo run --release --example probe -- /path/to/model-dir
//! ```
//!
//! This launches the built binary as a child process, writes the JSON request
//! klava would write, and reads the JSON response back. That means it tests the
//! real installation path — protocol, process boundary, and exit code — not an
//! in-process call.
//!
//! Prints savings and wall time per content type, then runs two safety checks:
//!
//! 1. **Needle** — an `URGENT` line and an error code must survive.
//! 2. **Meaning** — negations and identifiers must survive, because deleting
//!    them inverts or corrupts the message.

use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

/// Run the extension binary exactly as klava does and return its decision.
fn run(binary: &str, model_dir: &str, text: &str, threshold: f32) -> (Option<String>, f64) {
    let request = json!({
        "protocol": 1,
        "config": {
            "enabled": true,
            "model_dir": model_dir,
            "threshold": threshold,
            "min_tokens": 1,
            "max_tokens": 100_000,
            "min_savings": 0.0,
            "roles": ["tool"],
            "skip_structured": true,
        },
        "payload": {
            "model": "probe",
            "messages": [{"role": "tool", "content": text}],
        },
        "reason": "continuation",
        "source_api": "openai",
        "model": "probe",
        "nested_calls_used": 0,
    });

    let started = Instant::now();
    let mut child = Command::new(binary)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn extension");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(request.to_string().as_bytes())
        .expect("write request");

    let output = child.wait_with_output().expect("wait");
    let ms = started.elapsed().as_secs_f64() * 1000.0;

    assert!(
        output.status.success(),
        "extension exited {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let response: Value = serde_json::from_slice(&output.stdout).expect("valid response JSON");
    let payload = match response["decision"].as_str() {
        Some("replace") => response["payload"]["messages"][0]["content"]
            .as_str()
            .map(str::to_string),
        _ => None,
    };
    (payload, ms)
}

fn words(text: &str) -> usize {
    text.split_whitespace().count()
}

fn main() {
    let model_dir = std::env::args()
        .nth(1)
        .expect("usage: probe <model-dir> [binary]");
    let binary = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "target/release/klava-compress".to_string());

    let logs = (0..80)
        .map(|i| {
            format!(
                "2026-10-18T12:00:{:02}Z INFO req={} status=200 ms={} path=/api/v1/items/{}",
                i % 60,
                i,
                20 + i,
                "abcdefgh".chars().nth(i % 8).unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let cases: Vec<(&str, String)> = vec![
        ("dense logs", logs),
        (
            "repeated JSON",
            (0..60)
                .map(|i| format!(r#"{{"id":{},"name":"item_{}","status":"ok"}}"#, i, i))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        (
            "prose",
            "The quarterly report shows growth. Some details follow about the analysis. \
             We examined the data carefully and reached a conclusion about the results. "
                .repeat(8),
        ),
        (
            "source code",
            "fn main() {\n    let x = compute(1, 2);\n    println!(\"{}\", x);\n}\n".repeat(10),
        ),
        (
            "SQL",
            "SELECT id, name FROM users WHERE active = true ORDER BY id;".to_string(),
        ),
        (
            "needle",
            format!(
                "{}\nURGENT: disk quota exceeded on /var, error code EIO-7734\n{}",
                "filler line about routine scheduling and background maintenance\n".repeat(40),
                "more filler about routine scheduling and background maintenance\n".repeat(40)
            ),
        ),
    ];

    for threshold in [0.5f32, 0.7] {
        println!("\n--- threshold {threshold} ---");
        for (label, text) in &cases {
            let (out, ms) = run(&binary, &model_dir, text, threshold);
            match out {
                Some(compressed) => {
                    let before = words(text);
                    let after = words(&compressed);
                    let saved = 100.0 * (1.0 - after as f32 / before as f32);
                    println!(
                        "  {label:<16} {before:>5} -> {after:>5} words ({saved:>5.1}% saved) {ms:>8.1} ms"
                    );
                }
                None => {
                    println!("  {label:<16} unchanged (skipped)                   {ms:>8.1} ms")
                }
            }
        }
    }

    println!("\n--- safety @ threshold 0.7 ---");

    let (_, needle) = &cases[5];
    match run(&binary, &model_dir, needle, 0.7).0 {
        Some(compressed) => println!(
            "  needle facts kept: {} (URGENT={}, EIO-7734={})",
            compressed.contains("URGENT") && compressed.contains("EIO-7734"),
            compressed.contains("URGENT"),
            compressed.contains("EIO-7734")
        ),
        None => println!("  needle: unchanged (safe by default)"),
    }

    let meaning_cases = [
        (
            "negation",
            "Do not delete the production database; the migration must never run twice.",
        ),
        (
            "requirement",
            "Configuration is missing the required field api_url, so startup aborts.",
        ),
        (
            "identifier",
            "Call deploy_service() with cluster=prod-eu-1 and retries=3.",
        ),
    ];
    for (label, text) in meaning_cases {
        match run(&binary, &model_dir, text, 0.7).0 {
            Some(compressed) => {
                let lost_negation = ["not", "never", "no"].iter().any(|n| {
                    text.split_whitespace().any(|w| w.eq_ignore_ascii_case(n))
                        && !compressed
                            .split_whitespace()
                            .any(|w| w.eq_ignore_ascii_case(n))
                });
                let lost_id = [
                    "api_url",
                    "deploy_service",
                    "prod-eu-1",
                    "cluster",
                    "retries",
                ]
                .iter()
                .any(|id| text.contains(id) && !compressed.contains(id));
                println!("  {label:<12} \"{compressed}\"");
                if lost_negation {
                    println!("      !! LOST NEGATION");
                }
                if lost_id {
                    println!("      !! LOST IDENTIFIER");
                }
            }
            None => println!("  {label:<12} unchanged (safe by default)"),
        }
    }
}

//! Probe the decide-driven escalation gate against representative agent states.
//!
//! `klava-model-escalation` owns the question and the evidence; klava owns the
//! decide model. This example exercises the same contract the extension uses:
//! build a bounded excerpt of the conversation, ask the host for a `noul`
//! judgment, and only escalate when it clears the threshold.
//!
//! ```sh
//! cargo run --example nested_gate_probe
//! ```
//!
//! It needs a klava session (`klava login`) or `KLAVA_API_KEY`. Nothing is sent
//! anywhere except the one small classification request per case.

use klava::config::Config;
use klava::decision;
use klava::ext_api::{DecideQuestion, DecideRequest};
use serde_json::json;

struct Case {
    name: &'static str,
    /// `Some` for a control whose verdict is known; `None` for a calibration
    /// case where the point is to observe the model's judgment, not assert it.
    expect: Option<bool>,
    context: &'static str,
}

const QUESTION: &str = "A code agent is stuck and the escalation extension proposes routing this \
                        turn to a stronger, substantially more expensive model. Based only on the \
                        evidence in the context, is the stronger model genuinely required to make \
                        progress — that is, can the cheap model not plausibly fix this from the \
                        same information with a concrete hint? Answer yes only when the task needs \
                        reasoning beyond the cheap model's ability; repeated tool errors alone are \
                        not enough, and a concise hint or a different approach usually is.";

const CASES: &[Case] = &[
    Case {
        name: "identical empty results (the real false positive)",
        expect: Some(false),
        context: "Escalation target: ~anthropic/claude-opus-latest\n\
                  Extension reason: stuck detected (3x identical result)\n\
                  Recent conversation (oldest first):\n\
                  [user] run the tests\n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo test\"})\n\
                  [tool] \n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo test\"})\n\
                  [tool] \n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo test\"})\n\
                  [tool] ",
    },
    Case {
        name: "same command, same compile error",
        expect: Some(false),
        context: "Escalation target: ~anthropic/claude-opus-latest\n\
                  Extension reason: stuck detected (3 consecutive errors)\n\
                  Recent conversation (oldest first):\n\
                  [user] fix the build\n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo build\"})\n\
                  [tool] error[E0308]: mismatched types --> src/main.rs:12:5 expected `u32`, found `&str`\n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo build\"})\n\
                  [tool] error[E0308]: mismatched types --> src/main.rs:12:5 expected `u32`, found `&str`\n\
                  [assistant] tool calls: bash({\"cmd\":\"cargo build\"})\n\
                  [tool] error[E0308]: mismatched types --> src/main.rs:12:5 expected `u32`, found `&str`",
    },
    Case {
        name: "genuinely hard design task, no error at all",
        expect: Some(false),
        context: "Escalation target: ~anthropic/claude-opus-latest\n\
                  Extension reason: stuck detected (3x identical result)\n\
                  Recent conversation (oldest first):\n\
                  [user] Design a distributed rate limiter that stays correct across three regions, \
                  survives a region partition, and keeps p99 latency under 20ms. Explain the tradeoffs \
                  of the consistency model you pick and prove the invariant you rely on.\n\
                  [assistant] tool calls: bash({\"cmd\":\"ls src\"})\n\
                  [tool] main.rs\nlib.rs",
    },
    Case {
        name: "deep reasoning failure after real attempts",
        expect: None,
        context: "Escalation target: ~anthropic/claude-opus-latest\n\
                  Extension reason: stuck detected (5 consecutive errors)\n\
                  Recent conversation (oldest first):\n\
                  [user] Our raft implementation loses committed entries when a leader crashes during \
                  a snapshot transfer. Find the bug and explain why the invariant breaks.\n\
                  [assistant] I checked the log append path and the snapshot install path; commit index \
                  is persisted after applying, which may be the issue. tool calls: \
                  bash({\"cmd\":\"cargo test raft::snapshot -- --nocapture\"})\n\
                  [tool] FAILED: committed entry 42 missing after install_snapshot; leader crash at \
                  term 7. assertion failed: committed <= last_applied\n\
                  [assistant] Retried with extra logging; the failure reproduces at the same point and \
                  I cannot explain why the follower truncates entries below commit_index.\n\
                  [tool] error: assertion failed: committed <= last_applied",
    },
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::default();
    let model = config.resolve_decide_model();
    // The extension owns its threshold; this probe uses a deliberately strict
    // one, mirroring what klava-model-escalation recommends.
    let threshold = 0.8f32;
    println!("decide model: {model}\nthreshold: {threshold}\n");

    let mut failures = 0usize;
    let mut total_cost = 0.0f64;
    for case in CASES {
        let request = DecideRequest {
            state: json!({ "context": case.context }),
            questions: vec![DecideQuestion {
                id: "needs_stronger_model".to_string(),
                kind: "noul".to_string(),
                instructions: QUESTION.to_string(),
                criteria: None,
            }],
            threshold,
            approve_question: None,
        };

        let outcome = decision::ask(&config, &request).await;
        let (approve, detail) = match &outcome {
            Ok(result) => {
                if let Some(cost) = result.cost_usd {
                    total_cost += cost;
                }
                (
                    result.approve,
                    format!(
                        "p={:.3} threshold={:.2} model={}",
                        result.probability.unwrap_or(-1.0),
                        threshold,
                        result.model
                    ),
                )
            }
            Err(e) => (false, format!("unavailable: {e}")),
        };

        let verdict = if approve { "ESCALATE" } else { "stay cheap" };
        let mark = match case.expect {
            Some(expected) if expected == approve => "ok  ",
            Some(_) => {
                failures += 1;
                "MISS"
            }
            None => "cal ",
        };
        println!("{mark} {:<52} -> {:<10} {detail}", case.name, verdict);
    }

    println!(
        "\ncases: {}, controls failed: {failures}, decide cost: ${total_cost:.6}",
        CASES.len()
    );
    println!(
        "controls are the repeated-result patterns the old extension escalated on; \
         a control failure means the gate would still spend on them."
    );
    Ok(())
}

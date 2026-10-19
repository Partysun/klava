# klava-model-escalation

A "detect a stuck agent and escalate" extension for
[klava](https://github.com/Partysun/klava).

This standalone extension watches the conversation in each request and, when
the agent is clearly stuck, asks klava to route the turn to a stronger model
via a nested call. Detection is deterministic and stateless — it reads the
message history already present in the request, so it needs no session store.

klava launches this binary as a child process and speaks JSON over stdio; it is
never compiled into the klava binary. Installing needs no rebuild of klava.

> [!WARNING]
> **Escalation costs real money, and it can cost it fast.** Every escalation
> sends the conversation to a second, deliberately *more expensive* model and
> bills it per token on top of the turn you already paid for. A long session, a
> retry loop, or a generous `max_escalations` can burn through a balance before
> you notice. Start with `max_escalations = 1` and a model you can actually
> afford, watch your provider's usage page, and disable the extension if the
> spend surprises you. Detection is heuristic — it can fire on a legitimately
> repetitive task, not only on a truly stuck agent.

## Install

```sh
cargo install klava-model-escalation
klava extensions install ~/.cargo/bin/klava-model-escalation --trust
```

## Configuration

All keys live under `[extensions.settings.klava-model-escalation]` in the klava
config. `klava extensions show klava-model-escalation` prints every key with its
default and shows the cost warning above:

```toml
[extensions.settings.klava-model-escalation]
enabled = true
model = "~anthropic/claude-opus-latest"
# provider = "klava"            # omit to use the active provider
# mode = "append"               # "append" or "take_over"
# max_escalations = 1
# repeat_tool_call_limit = 3
# repeat_result_limit = 2
# error_streak_limit = 3
# scan_last_messages = 40
# max_tokens = 2048
# verify = true                 # ask the decide model first
# verify_threshold = 0.8
# verify_window = 12
# verify_max_chars = 8000
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Master switch. |
| `model` | `""` | The stronger model to escalate to. Empty = never escalate. Pick one whose price per token you are comfortable paying repeatedly. |
| `provider` | active provider | Provider that serves `model`. |
| `mode` | `"append"` | How the answer comes back: `append` folds the review as a user message and the original model continues; `take_over` returns the review as the answer. |
| `max_escalations` | `1` | Escalate at most this many times per client request. **Each one is a paid call to the escalated model** — raising this multiplies cost. |
| `repeat_tool_call_limit` | `3` | Identical tool calls (same name + arguments) that mean stuck. |
| `repeat_result_limit` | `2` | Identical tool results that mean stuck. |
| `error_streak_limit` | `3` | Consecutive failing tool results that mean stuck. |
| `scan_last_messages` | `40` | How much history to inspect. |
| `system` | senior-reviewer prompt | System prompt for the advised model. |
| `max_tokens` | `2048` | Max tokens for the escalated call. |
| `verify` | `true` | Ask the host's decide model to confirm escalation is genuinely needed before spending. |
| `verify_threshold` | `0.8` | Probability the decide model must report. Higher = fewer escalations. |
| `verify_window` | `12` | Recent messages included as evidence for the decide call. |
| `verify_max_chars` | `8000` | Character budget for that evidence. |

Failure markers are conservative: `Error:`, `Traceback`, `panicked at`,
`command not found`, `Permission denied`, `fatal:`, `FAILED`, `error[E…]`, and
similar.

## The decide step

The extension detects a stuck agent **deterministically**, then — by default —
asks klava's decide model whether the expensive model is genuinely required.
The extension owns the question and the bounded evidence; klava owns the model,
credentials, timeout and cost accounting. If the decide model says no (or is
unavailable), the turn stays on the cheap model. Set `verify = false` only if
you accept unverified spend.

The same mechanism is available to any extension: see
`decision: "decide"` in klava's `EXTENSIONS.md`.

The extension never performs the model call itself — it returns a `decide` or
`nested_call` decision and klava does it, so credentials, routing and timeouts
stay in the proxy.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

# klava-model-escalation

A "detect a stuck agent and escalate" extension for
[klava](https://github.com/Partysun/klava).

This standalone extension watches the conversation in each request and, when
the agent is clearly stuck, asks klava to route the turn to a stronger model
via a nested call. Detection is deterministic and stateless — it reads the
message history already present in the request, so it needs no session store.

klava launches this binary as a child process and speaks JSON over stdio; it is
never compiled into the klava binary. Installing needs no rebuild of klava.

## Install

```sh
cargo install klava-model-escalation
klava extensions install ~/.cargo/bin/klava-model-escalation --trust
```

## Configuration

All keys live under `[extensions.settings.klava-model-escalation]` in the klava
config:

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
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Master switch. |
| `model` | `""` | The stronger model to escalate to. Empty = never escalate. |
| `provider` | active provider | Provider that serves `model`. |
| `mode` | `"append"` | How the answer comes back: `append` folds the review as a user message and the original model continues; `take_over` returns the review as the answer. |
| `max_escalations` | `1` | Escalate at most this many times per client request. |
| `repeat_tool_call_limit` | `3` | Identical tool calls (same name + arguments) that mean stuck. |
| `repeat_result_limit` | `2` | Identical tool results that mean stuck. |
| `error_streak_limit` | `3` | Consecutive failing tool results that mean stuck. |
| `scan_last_messages` | `40` | How much history to inspect. |
| `system` | senior-reviewer prompt | System prompt for the advised model. |
| `max_tokens` | `2048` | Max tokens for the escalated call. |

Failure markers are conservative: `Error:`, `Traceback`, `panicked at`,
`command not found`, `Permission denied`, `fatal:`, `FAILED`, `error[E…]`, and
similar.

The extension never performs the model call itself — it returns a
`nested_call` decision and klava does it, so credentials, routing and timeouts
stay in the proxy.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

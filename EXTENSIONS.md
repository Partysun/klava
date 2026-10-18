# Extensions

klava ships with **no extensions**. Install one at any time, in any language,
without rebuilding klava.

An extension is an executable that reads one JSON object on stdin and writes one
JSON object on stdout. That is the whole contract.

## Install

Put an executable in the extensions directory:

```text
~/.config/klava/extensions/<name>          # the program
~/.config/klava/extensions/<name>.toml     # optional manifest
```

The file name (without extension) is the extension's name. Override the
directory with `KLAVA_EXTENSIONS_DIR`.

```sh
mkdir -p ~/.config/klava/extensions
install -m 755 my-extension ~/.config/klava/extensions/my-extension
klava extensions list
```

A file is picked up when it is a regular executable, is not hidden, and is not
a `.toml` file. Nothing else is required: no entry point, no registration, no
recompile.

## Enable

Nothing runs until it is enabled. `enabled` is the only key klava itself
interprets.

```toml
[extensions.settings.my-extension]
enabled = true
```

or `klava extensions enable my-extension`. The lookup order is:

1. `[extensions.settings.<name>].enabled` in the klava config,
2. `enabled` in the extension's own manifest,
3. otherwise disabled.

## Manifest

Optional, and entirely owned by the extension. klava reads `enabled`,
`fail_closed`, and `timeout_secs`; everything else is passed straight through in
the request's `config` field.

```toml
enabled = true
fail_closed = false     # false: a failure is logged and the request continues
timeout_secs = 20       # default 20; the child is killed after this
threshold = 0.7         # your own key, opaque to klava
```

## Protocol

### Request

klava writes exactly one JSON object to stdin and closes it:

```json
{
  "protocol": 1,
  "config": { "enabled": true, "threshold": 0.7 },
  "payload": {
    "model": "…",
    "messages": [{ "role": "user", "content": "…" }]
  },
  "reason": "user",
  "source_api": "openai",
  "model": "…",
  "nested_calls_used": 0
}
```

| Field | Meaning |
|---|---|
| `protocol` | Always `1` for now. Reject anything you do not understand. |
| `config` | Manifest merged with the user's settings table. |
| `payload` | The OpenAI-shaped chat-completions request. |
| `reason` | `user`, `continuation`, `retry`, or `direct`. |
| `source_api` | `openai`, `anthropic`, or `responses`. |
| `model` | The model the request will use. |
| `nested_calls_used` | Nested calls already made for this request. |

### Response

Write one JSON object to stdout and exit 0:

```json
{ "decision": "continue" }
```

| `decision` | Effect | Extra fields |
|---|---|---|
| `continue` | Leave the request alone. | — |
| `replace` | Use `payload` instead. | `payload` |
| `append` | Add `messages` to the conversation. | `messages` |
| `deny` | Reject the request. | `reason`, optional `fail_closed` |
| `nested_call` | Ask klava to run a second model and fold the answer in. | `nested_call` |

An unknown `decision`, or `replace` without a `payload`, is treated as
`continue`. klava never blanks a request because an extension was confused.

`nested_call` shape:

```json
{
  "decision": "nested_call",
  "nested_call": {
    "provider": "klava",
    "model": "~anthropic/claude-opus-latest",
    "mode": "append",
    "system": "You are a senior reviewer.",
    "instruction": "Point out what is wrong.",
    "max_tokens": 2048
  }
}
```

`mode` is `append` (default; the answer is added as a user message and the
original model continues) or `take_over` (the answer is returned to the client
as the assistant turn). The extension never makes the call itself: credentials,
routing, timeouts and the recursion boundary stay in klava, so an extension
cannot recurse.

### stderr

Anything on stderr is forwarded to klava's log as a warning, one line each.
Use it for diagnostics; never write it to stdout.

## Failure and safety

- A crash, nonzero exit, timeout, invalid JSON, or oversized output is a
  failure. By default klava logs it and sends the request unchanged.
- Set `fail_closed = true` (manifest, or per-response for `deny`) to reject the
  request instead. Only do that for a genuine gate.
- Output is capped at 8 MiB and stderr at 64 KiB.
- One process per request that has an enabled extension. Startup cost is a
  `spawn`, which is negligible next to a model call.

## Example

A complete extension that appends a note to the last user message:

```python
#!/usr/bin/env python3
import json, sys

request = json.load(sys.stdin)
payload = request["payload"]

for message in reversed(payload.get("messages", [])):
    if message.get("role") == "user" and isinstance(message.get("content"), str):
        message["content"] += "\n\n(Reviewed by my-extension.)"
        break

json.dump({"decision": "replace", "payload": payload}, sys.stdout)
```

```sh
chmod +x my-extension
mv my-extension ~/.config/klava/extensions/
klava extensions enable my-extension
```

## CLI

```sh
klava extensions              # list installed extensions
klava extensions show <name>  # status, path, effective config
klava extensions enable <name>
klava extensions disable <name>
klava extensions set <name> key=value
```

`set` parses values as JSON when it can, so `threshold=0.7` and
`roles=["tool"]` both work.

# klava-compress

Context compression for [klava](https://github.com/Partysun/klava).

Large tool results are mostly boilerplate: repeated log lines, JSON scaffolding,
stack frames nobody reads. This standalone extension scores every token with the
[`kompress-v2-base`](https://huggingface.co/chopratejas/kompress-v2-base) model
and drops low-value words, shrinking what is sent to the model.

klava launches this binary as a child process and speaks JSON over stdio; it is
never compiled into the klava binary. Installing needs no rebuild of klava.

## Install

```sh
cargo install klava-compress

# Install it; klava asks the binary for its [setup] hook and fetches the model:
klava extensions install ~/.cargo/bin/klava-compress --trust
```

No Rust toolchain? Install the prebuilt binary from npm instead — the package
name carries your platform:

```sh
klava extensions install npm:@zatsepin/klava-compress-linux-x64-gnu --trust
klava extensions install npm:@zatsepin/klava-compress-linux-arm64-gnu --trust
klava extensions install npm:@zatsepin/klava-compress-darwin --trust
klava extensions install npm:@zatsepin/klava-compress-win32-x64-msvc --trust
```

The `--trust` above also consents to the model download, which runs right after
the extension is enabled (see [Model](#model)). To install and enable without
downloading anything, or to fetch later:

```sh
klava extensions install ~/.cargo/bin/klava-compress --trust --no-setup
klava extensions setup klava-compress
```

`--no-setup` leaves the extension `pending`; it fails open until the model
arrives. The same hook can be run by hand:

```sh
klava-compress --fetch-model              # into the cache
klava-compress --fetch-model --dir /models
```

The download resumes a partial `.part` file, so an interrupted 573 MB fetch does
not start over.

### Model

The model lands in `$XDG_CACHE_HOME/klava/kompress` (default
`~/.cache/klava/kompress`) and is never committed. The directory is resolved in
this order: `--dir`, `model_dir` (from your klava settings, passed to the hook in
`$KLAVA_EXTENSION_CONFIG`), `$KLAVA_COMPRESS_MODEL_DIR`, then the cache.
`scripts/fetch-model.sh` does the same with `curl` and remains for manual use.

## Configuration

All keys live under `[extensions.settings.klava-compress]` in the klava config.
`klava extensions show klava-compress` prints them with their defaults:

```toml
[extensions.settings.klava-compress]
enabled = true
# model_dir = "~/.cache/klava/kompress"
# threshold = 0.7
# min_tokens = 0
# max_tokens = 0
# min_savings = 0.10
# budget_mb = 0
# budget_ms = 0
# roles = ["tool"]
# skip_structured = true
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Master switch. The model is a ~580 MB download, so this is an explicit choice. |
| `model_dir` | cache dir | Where `kompress-fp32.onnx` and `tokenizer.json` live. Empty = `$KLAVA_COMPRESS_MODEL_DIR`, else `$XDG_CACHE_HOME/klava/kompress`. |
| `threshold` | `0.7` | Keep a word when any of its tokens scores ≥ this. Lower keeps more (and saves less). |
| `min_tokens` | `0` = derived | Skip shorter messages. Derived from `min_savings`: a message must be long enough to remove about 200 bytes, which lands near 100 tokens. |
| `max_tokens` | `0` = derived | A ceiling *below* what the memory budget allows. `0` uses the budget's own ceiling; a larger value is clamped to it, because memory is quadratic and no setting may override that. |
| `min_savings` | `0.10` | Discard a result unless it removes at least this fraction of words **and** at least 200 bytes. |
| `budget_mb` | `0` = detected | Megabytes one request may spend on inference. `0` measures the machine — Linux cgroup v2 headroom then `MemAvailable`, macOS free + inactive + speculative Mach pages, Windows `ullAvailPhys` — and keeps a quarter back for the rest of the machine. |
| `budget_ms` | `0` = 8000 | Milliseconds one request may spend on inference. klava kills the extension at 20 s, so returning partial work needs a smaller number. |
| `roles` | `["tool"]` | Which message roles may be rewritten. Never touches system or assistant instructions. |
| `skip_structured` | `true` | Skip content that is mostly code, SQL, JSON, or diffs. |

### Why there is a budget instead of a token window

Inference memory and time are **quadratic** in the token count. Measured on the
fp32 model with a default ONNX Runtime thread pool:

| tokens | peak RSS | wall |
|---|---|---|
| 2,000 | 1.2 GB | 2.6 s |
| 4,000 | 2.9 GB | 6.5 s |
| 8,000 | 8.6 GB | 20 s |
| 11,000 | 15.7 GB | 34 s |
| 14,000 | 26.7 GB | 54 s |

A 14k-token message therefore reached 26.7 GB and ~41k asked ONNX Runtime for a
single 80 GB buffer. Threads are not a lever: 28, 20 and 4 threads all measured
the same peak at 8k tokens.

So the extension does not use a fixed window. It:

1. tokenizes every candidate and applies **every cheap rejection first** —
   role, content shape, structured content, and the size window — before the
   ONNX graph is ever mapped;
2. predicts what each remaining message would cost, from the table above;
3. **plans** the request cheapest-message-first, charging cumulative megabytes
   and seconds against the budget, and stops when the next one would not fit;
4. re-checks the wall clock while working, so a slower-than-predicted machine
   returns its partial result instead of being killed with nothing.

Small messages go first because cost per saved token falls as the message
shrinks, and because a request that runs out of time then still returns its best
work. The memory charge is deliberately pessimistic: ONNX Runtime's arena keeps
what it takes and fragments across different input shapes, so five identically
sized 2.7k-token messages reused one arena (2.1 GB) while a real 20-message
request with varied sizes reached 26 GB.

The budget itself is measured, not configured: Linux takes the tightest cgroup v2
`memory.max - memory.current` on the process's path and falls back to
`MemAvailable`; macOS sums the free, inactive and speculative Mach pages; Windows
reads `ullAvailPhys`. A quarter is held back (at least 1 GB) so the machine stays
usable, and the result is capped at 2,560 MB — enough for the 90th percentile of
real tool output (2,716 tokens) while one message can never ask for more. On a
target that cannot measure memory the extension uses a conservative 1,300 MB
rather than guessing, and `budget_mb` raises it deliberately.

## Safety

- Removal is **word-level**, never token-level, so identifiers (`EIO-7734`),
  numbers and whitespace structure survive.
- Every rewrite is checked against the original and **discarded** if it would
  drop a negation, a number, an identifier, an ALL-CAPS marker, or a severity
  word.
- Fails **open**: missing model, bad tokenizer, inference error, structured
  content, or an unsafe rewrite all leave the request unchanged.
- Never runs inference on a message it has not measured, and never past the
  memory or time budget it detected, so a large tool result degrades to "no
  compression" rather than to an OOM kill.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

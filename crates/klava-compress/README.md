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

# Fetch the model into the cache (kompress-fp32.onnx + tokenizer.json):
crates/klava-compress/scripts/fetch-model.sh

klava extensions install ~/.cargo/bin/klava-compress --trust
```

The model lands in `$XDG_CACHE_HOME/klava/kompress` (default
`~/.cache/klava/kompress`) and is never committed.

## Configuration

All keys live under `[extensions.settings.klava-compress]` in the klava config:

```toml
[extensions.settings.klava-compress]
enabled = true
# model_dir = "~/.cache/klava/kompress"
# threshold = 0.7
# min_tokens = 200
# max_tokens = 1500
# min_savings = 0.10
# roles = ["tool"]
# skip_structured = true
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Master switch. The model is a ~580 MB download, so this is an explicit choice. |
| `model_dir` | cache dir | Where `kompress-fp32.onnx` and `tokenizer.json` live. Empty = `$KLAVA_COMPRESS_MODEL_DIR`, else `$XDG_CACHE_HOME/klava/kompress`. |
| `threshold` | `0.7` | Keep a word when any of its tokens scores ≥ this. Lower keeps more (and saves less). |
| `min_tokens` | `200` | Skip shorter messages; the fixed inference cost dominates. |
| `max_tokens` | `1500` | Skip longer messages; inference is superlinear (a 3.4k-token message costs ~6.5 s). |
| `min_savings` | `0.10` | Discard a result unless it removes at least this fraction of words. |
| `roles` | `["tool"]` | Which message roles may be rewritten. Never touches system or assistant instructions. |
| `skip_structured` | `true` | Skip content that is mostly code, SQL, JSON, or diffs. |

## Safety

- Removal is **word-level**, never token-level, so identifiers (`EIO-7734`),
  numbers and whitespace structure survive.
- Every rewrite is checked against the original and **discarded** if it would
  drop a negation, a number, an identifier, an ALL-CAPS marker, or a severity
  word.
- Fails **open**: missing model, bad tokenizer, inference error, structured
  content, or an unsafe rewrite all leave the request unchanged.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

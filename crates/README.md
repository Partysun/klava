# klava extension crates

Standalone extensions for [klava](https://github.com/Partysun/klava), kept in
the same repository so they are easy to manage.

These are ordinary Rust crates, but klava does **not** compile them in and has
no dependency on them. Each is a standalone executable that speaks klava's
JSON-over-stdio protocol: klava launches one as a child process, writes a
request to its stdin, and reads a response from its stdout. There is no plugin
ABI, no `.so`, no `inventory` — just a documented wire format (`EXTENSIONS.md`).

Keeping them as separate crates under `crates/` means the heavy dependencies a
single extension needs (ONNX, the PII analyzer) never reach klava's default
build, and each publishes independently on crates.io.

## Layout

```
klava-extensions/
└── crates/
    ├── klava-compress/          # context compression (kompress-v2-base)
    ├── klava-pii-guardrail/     # PII redaction for tool output
    └── klava-model-escalation/  # route a stuck agent to a stronger model
```

Each crate is a standalone `[[bin]]` speaking the klava stdio protocol. They
build and publish **independently** on crates.io; nothing is compiled into the
klava binary.

```sh
cargo build --release -p klava-compress -p klava-pii-guardrail -p klava-model-escalation
cargo publish -p klava-pii-guardrail
cargo publish -p klava-model-escalation
cargo publish -p klava-compress
```

## How klava uses these

These crates live in this repository under `crates/` but are **not** part of
the klava binary. Each is a standalone executable that builds and publishes
independently on crates.io; klava itself has no dependency on them and never
compiles them in.

They are **standalone executables**: klava launches one as a child process,
writes a JSON request to its stdin, and reads a JSON response from its stdout.
No shared library, no ABI, no compiled-in crate, and no dependency on
any klava crate.

That means the install order is what you would expect:

```sh
# 1. install klava (prebuilt binary, any release)
# 2. build the extension and install it
cargo build --release -p klava-compress
klava extensions install target/release/klava-compress --trust
```

klava is never rebuilt. The protocol is documented in klava's `EXTENSIONS.md`;
an extension can be written in any language that can read stdin and write stdout.
klava searches `~/.local/share/klava/extensions` first (and a few system
directories after it); `klava extensions path` lists them.

### Cost of the process boundary

One process per request that has an enabled extension. For this extension that
is about **1.0 s of startup**, almost all of it loading the 573 MB ONNX model.
That fixed cost is why the extension rejects a message on its token count
*before* mapping the graph: a message outside the window must not pay for the
model. Below that, inference itself is quadratic in the token count, so the
extension plans each request against a memory and time budget rather than a
fixed window — see `crates/klava-compress/README.md`.

## klava-compress

Drops low-value tokens from tool output using
[`chopratejas/kompress-v2-base`](https://huggingface.co/chopratejas/kompress-v2-base)
(`kompress-fp32.onnx`, ~573 MB).

### Install

```sh
# 1. Build the extension (klava is untouched):
cargo build --release -p klava-compress

# 2. Install it (writes to ~/.local/share/klava/extensions). klava asks the
#    binary for its [setup] hook and downloads the model after enabling it:
klava extensions install target/release/klava-compress --trust
```

Add `--no-setup` to skip the download and run `klava extensions setup
klava-compress` later; `klava-compress --fetch-model` does the same by hand.

The model lands in `$XDG_CACHE_HOME/klava/kompress` (default
`~/.cache/klava/kompress`) and is never committed.

### Enable

Nothing runs until it is enabled. `enabled` is the only key klava interprets:

```toml
[extensions.settings.klava-compress]
enabled = true
```

or:

```sh
klava extensions enable klava-compress
klava extensions list
klava extensions show klava-compress
```

### Settings

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Off by default: the model is a large download. |
| `model_dir` | cache dir | Where `kompress-fp32.onnx` and `tokenizer.json` live. |
| `threshold` | `0.7` | Keep a token when its score is ≥ this. Lower keeps more. |
| `min_tokens` | `0` = derived | Skip shorter messages. Derived from the 200-byte savings floor, near 100 tokens. |
| `max_tokens` | `0` = derived | Only lowers the budget's own ceiling; a larger value is clamped to it. |
| `min_savings` | `0.10` | Discard a result below this saving fraction or below 200 bytes, measured on the rebuilt text. |
| `budget_mb` | `0` = detected | Megabytes one request may spend on inference. `0` measures the machine: cgroup v2 headroom then `MemAvailable` on Linux, Mach VM statistics on macOS, `ullAvailPhys` on Windows. Only lowers the ceiling; capped at 2,560 MB. |
| `budget_ms` | `0` = 8000 | Milliseconds one request may spend on inference; clamped to 17,000 because klava kills the extension at 20 s and a killed extension prints nothing. |
| `roles` | `["tool"]` | Which message roles may be rewritten. |
| `skip_structured` | `true` | Skip content that is mostly code, SQL, JSON, or diffs. |

### Measured behavior

`cargo run --release --example probe -- /path/to/model-dir`, laptop CPU,
`kompress-fp32.onnx`:

| Content | thr 0.5 | thr 0.7 |
|---|---|---|
| dense logs (480 words) | skipped | **15.6%** saved |
| repeated JSON | skipped | skipped (structured) |
| prose | 8.7% | 21.2% |
| source code | skipped (structured) | skipped (structured) |
| SQL | skipped (structured) | skipped (structured) |
| needle test | 16.0% | 76.7% |

`URGENT` and the error code `EIO-7734` survive at 0.7.

### Two guards, both learned the hard way

Token-level removal was tried first and **corrupted text**: byte-level BPE
splits identifiers across tokens, so `EIO-7734` became `E-7734` and
`information routine` became `informationroutine`. The unit of removal is
therefore a **word**, and whitespace is never deleted (so words cannot merge and
newlines/indentation survive).

Word-level removal alone was still unsafe. Measured:

```
in : Configuration is missing the required field api_url, so startup aborts.
out: Configuration api_url,

in : Do not delete the production database; the migration must never run twice.
out: Do not delete production database; never
```

`"Do not delete ..."` → `"Do not delete"` reads as permission. So every rewrite
is checked against the original, and is **discarded** if it loses:

- a negation or modal (`not`, `never`, `no`, `must`, `without`, …),
- any number (ports, codes, versions, dates),
- an identifier (`api_url`, `deploy_service`, `prod-eu-1`, paths),
- an ALL-CAPS marker (`URGENT`, `WARNING`),
- a severity word (`fatal`, `error`, `deprecated`, …).

Plus a deterministic pre-check skips content that is mostly code, SQL, JSON, or
diff. Compression is skipped in those cases — the request is sent unchanged.

This is deliberately conservative. It means kompress helps most on redundant
tool output (logs, repeated dumps) and rarely on instructions. That is the
honest trade: no silent corruption.

### Cost

Inference is CPU-bound and quadratic: measured peak RSS and wall time are
2k → 1.2 GB / 2.6 s, 4k → 2.9 GB / 6.5 s, 8k → 8.6 GB / 20 s, 14k → 26.7 GB /
54 s, and ~41k asks ONNX Runtime for a single 80 GB buffer. Thread count does not
change this (28, 20 and 4 threads all measured 8.6 GB at 8k tokens).

So the token ceiling is **derived**, not configured: it is the largest message
that fits the memory budget this process detects, capped at 4,000 tokens as a
backstop. `budget_mb` and `budget_ms` override the detection; `min_tokens` and
`max_tokens` can only narrow the result.

### Known limits

- **`int8` is unsupported here.** `kompress-int8.onnx` needs ONNX Runtime ≥ 1.24;
  `ort 2.0.0-rc.10` ships 1.23.2 and fails at run time. `fp32` works.
- Only Chat Completions payloads are touched. `tools`, `system`, and assistant
  messages are never rewritten.
- Fails **open** everywhere: missing model, bad tokenizer, inference error,
  structured content, or a failed safety check all leave the request untouched.

## Adding an extension

Everything happens in this repository. **Nothing in `klava/` changes.**

1. `cargo new --lib crates/klava-<name>` with `edition.workspace = true`.
2. Depend on `klava-ext-api` and implement `Extension`.
3. `inventory::submit!` an `ExtensionDescriptor` with the crate's `name` and
   `default_config`.
4. In `crates/klava-extensions/Cargo.toml`, add the crate as an optional
   dependency plus a feature for it.
5. In `crates/klava-extensions/src/lib.rs`, add `use klava_<name> as _;` behind
   that feature. This is what makes the crate discoverable at runtime; the
   bundle's own test fails if you forget it.
6. Add tests for the pure logic, and an example that measures real behavior.

Then rebuild klava with `--features extensions`.

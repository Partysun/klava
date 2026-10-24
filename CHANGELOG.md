# Changelog

## [Unreleased]

### Added

- **npm distribution.** `npm install --global @zatsepin/klava` (or `npx @zatsepin/klava`) now installs a prebuilt binary picked from a per-platform package (`@zatsepin/klava-linux-x64-gnu`, `-linux-arm64-gnu`, `-darwin` universal2, `-win32-x64-msvc`) — no Rust toolchain involved. Alpine/musl is not packaged yet; use `cargo install klava` there.
- **PyPI wheels for every supported platform.** Alongside the sdist, releases now ship `manylinux_2_17` wheels for x86_64 and aarch64 (aarch64 cross-compiled on the amd64 agent with zig), a macOS universal2 wheel and a Windows wheel, so `pip install klava` / `uvx klava` no longer compile from source. The macOS wheel had been missing since 0.2.0 because the release step passed a flag maturin does not have.

## [klava-compress 0.1.7] - 2026-10-24

### Added

- **`klava-compress` reports what it has saved.** The extension now appends one
  JSON line per request to `$XDG_DATA_HOME/klava/compress-stats.jsonl`
  (`~/.local/share/klava/` by default) and prints the totals back with
  `klava-compress --stats` (`--json` for scripts) — or `klava extensions stats
  klava-compress`, which runs the installed binary. The output has a per-day
  table and the skip mix, so "where did the savings go" is answerable from the
  same file. Two totals are reported: one summed over runs, and one deduplicated
  by content hash, because the agent re-sends its history every turn and summing
  counts the same tool output again each time. Counting lives in the extension
  rather than in log scraping: klava forwards at most 20 stderr lines and the
  summary line was printed last, so a noisy request lost exactly the numbers
  worth measuring. Recording a run is best-effort — a failed write warns and never
  affects compression — the file rotates at 16 MiB, and `stats = false` turns it
  off. A new `stats` setting declares this.

## [klava-pii-guardrail 0.1.6] - 2026-10-24

### Fixed

- **`klava-pii-guardrail` timed out on large tool output and failed the request
  closed.** Redaction called `pii::Anonymizer::anonymize`, which rewrites spans
  back-to-front with `String::replace_range` — every match moved the rest of the
  string, so cost was O(matches × length). A 16 MB binary-RE dump with 400k
  matches took 57 s against klava's 20 s deadline, and because the manifest sets
  `fail_closed = true` a timed-out guardrail rejects the request outright;
  `klava-compress`, which runs first and fails open, had already handed the same
  payload on. Redaction is now one forward splice instead of a replacement per
  span: the same 400k-match input measures 12.1 s through the crate and 21 ms
  through the splice, so the linear path stays three orders of magnitude away
  from the deadline. The operators are local copies (the crate keeps them
  private), and a test pins their output to the crate's, byte for byte.
- **`klava-pii-guardrail` no longer analyses unbounded payloads.** Requests with
  more than 3 MiB of text are refused with `decision: "deny"` and a reason
  instead of being passed through unredacted. 3 MiB keeps the worst case inside
  klava's 8 MiB `MAX_STDOUT` reply cap even when redaction roughly doubles the
  text, and bounds peak memory (the analyzer allocates a token per word).

### Changed

- `klava-pii-guardrail` is now versioned in its own `Cargo.toml` instead of
  inheriting `[workspace.package]`, so a guardrail fix no longer forces a bump on
  `klava-model-escalation`. `klava-compress` already pins its own version the
  same way.

## [klava-compress 0.1.6] - 2026-10-23

### Fixed

- **`klava-compress` could answer with an empty message.** The savings floors were
  applied to the model's per-token keep-flags, not to the text that would be sent,
  so at a high `threshold` a message whose words all scored low was rebuilt to
  nothing and returned as `replace` with `content: ""` — reported as a 100%
  saving, and invisible to the meaning guard because there was no text left to
  lose. Measured on 0.1.5: a 432-token CJK message became 0 tokens. The floors now
  run on the rebuilt text, and an empty rewrite is refused outright.
- **`klava-compress` could be killed by klava's 20 s deadline with no reply at
  all.** `budget_ms` was not clamped against the host timeout, so `budget_ms =
  60000` over a long request ran to 20.05 s, was killed, and printed nothing —
  klava saw "produced no output" and passed the payload through unchanged. The
  setting is now clamped to 17 s, and a forward pass is only started when the
  measurement says it can finish before the deadline (a pass cannot be
  interrupted, so starting one late loses the whole reply, not just that
  message). `600 small messages` now stops itself at 16 s and returns 202
  compressed messages.
- **`klava-compress` halved the sessions it exists for.** The planner summed the
  quadratic memory cost of every message as if each one allocated a fresh arena,
  so 8 × 1,500 tokens predicted 2,822 MB and ran only 4 of the 8, while the
  measured peak was 1.6 GB. Memory is now charged as arena *reuse* (largest shape
  in full, smaller ones at a fraction), and all 8 run; 12 × 1,100-token messages
  went from 6 to 12.
- **`klava-compress` under-predicted the time of many-message requests.** The cost
  model charged only the quadratic term, so 120 × 200 tokens forecast 2.8 s while
  taking 8.2 s. Each message now also pays a fixed per-inference cost.
- **`klava-compress` logged one line per message.** A 40-message payload produced
  40 identical "dropped marker" lines, and klava forwards at most 20 stderr lines,
  so the summary that says what happened was flushed out. Rejections are now
  counted by reason and reported once per request.
- Documented that `klava-compress` is not a redaction tool: it removes low-scoring
  text, and a secret that looks like prose is scored like prose. Card and account
  numbers survive only because numbers and identifiers are protected, not because
  the tool looks for secrets.

### Changed

- `klava-compress` is now versioned in its own `Cargo.toml` instead of inheriting
  `[workspace.package]`, so a fix that touches one extension no longer forces a
  version bump on the other two. `klava-pii-guardrail` and
  `klava-model-escalation` stay at 0.1.5.

## [klava-compress 0.1.5] - 2026-10-23

### Added

- **`klava-compress` plans a request instead of using a fixed token window.** `min_tokens = 200` / `max_tokens = 1500` matched *none* of the real tool output it was configured for (p50 188, p90 2,716, max 5,090 tokens), and memory is quadratic in the token count — 14k tokens measured 26.7 GB, ~41k asked ONNX Runtime for a single 80 GB buffer, and 24 OOM kills later the kernel always shot the extension. The extension now derives its window from two budgets (`budget_mb`, `budget_ms`), detects the first from what the machine actually has — cgroup v2 headroom then `MemAvailable` on Linux, Mach VM statistics on macOS, `GlobalMemoryStatusEx` on Windows — keeps a quarter back for the rest of the machine, and plans each request cheapest-message-first so a request that runs out of time returns partial work rather than nothing. A 14k-token message went from 26.7 GB / 54 s to 60 MB / 0.1 s, and a production-shaped 20-message request from 26 GB to 1.8 GB.

### Fixed

- **`klava-compress` ran inference on messages it had already decided to discard.** The size guard ran *after* the forward pass, so a request the extension was certain to leave untouched still mapped the 600 MB graph and paid full inference — measured, 93% of all messages. Rejection now happens on the tokenizer's output, before the graph is loaded.
- `klava-compress` no longer rewrites a message to remove less than 200 bytes. A percentage-only floor let a 40-token message "save 50%" and contribute nothing while the inference that produced it could have removed far more elsewhere.

## [0.3.0] - 2026-10-20

### Added

- **`[[models]]` replaces `[[providers]]` + `active_provider`.** Every model is one self-contained entry (`name`, `type`, `base_url`, `api_key`/`api_key_name`, optional `chat_completions_path` and `upstream_model`), so a model name — not a provider — is the unit of routing. `active_provider` is replaced by the top-level `completion_model` and `reasoning_model`, which name entries.
- **Per-request routing.** A request whose body names a configured model goes to that model; an unknown or placeholder name (e.g. an agent still sending `klava`) falls back to the configured completion/reasoning model. `reasoning` requests are those with `reasoning_effort` above `none` (OpenAI), `thinking.type` enabled/adaptive (Anthropic) or `reasoning.effort` above `minimal` (Responses).
- **Duplicate names form a pool**, picked per request by the new `routing` key: `shuffle` (default, random), `round_robin`, or `first` (declaration order = failover priority).
- **Real model names in harnesses.** The proxy serves `GET /v1/models`, and the setup for opencode, pi and codex writes the configured model names instead of the synthetic `klava`, so the model picker and the model label show what actually answers.
- **`klava providers` is the onboarding wizard.** `add [klava|openrouter|cloudflare|qwen-code|custom]` logs in, lists the provider catalog, checks the connection with a tiny completion, lets you pick models with a multi-select, then asks for the default completion/reasoning/decide model and writes the entries — storing keys in an env var by default. Also `list`, `check [model]`, `remove <model>`, `set [model]`, and the existing `klava login|logout|status` / `qwen-code login|logout|status`.
- Cloudflare Workers AI is a first-class wizard option: it collects the account id, lists text-generation models through `/ai/models/search` and stores the account-scoped `…/accounts/<id>/ai/v1` base URL.
- **jigor backend auto-detection.** The decide backend is derived from the `decide_model` entry's type and URL — `type = "klava"` → klava gateway, `openrouter.ai` → OpenRouter, `api.cloudflare.com` → Workers AI, anything else → klava wire — with credentials taken from the entry and falling back to the saved `klava login`. The optional `jigor` field exists only for self-hosted gateways.
- An `openai-compatible` entry with neither `api_key` nor `api_key_name` is treated as an unauthenticated endpoint (local Ollama/llama.cpp) and sends no `Authorization` header; naming an `api_key_name` whose variable is empty is reported as an error.
- `KLAVA_CONFIG_FILE=/path/config.toml` pins the config file, so a second profile or a hermetic test run never touches `~/.config/klava/config.toml`.

### Changed

- Config migration is automatic: a config with `[[providers]]` is rewritten as `[[models]]` on load, after copying the original to `config.toml.bak`. Every distinct model id a provider named becomes its own entry; the previously active provider supplies the top-level defaults.
- The decide model must name exactly one entry: a duplicate name is a startup error, unlike regular models, because a classifier has to be deterministic. With no entry, the built-in klava gateway is used.
- Web search is gated on the *search model's* type (`type = "klava"` plus a key or saved login), not on a global active provider; `search_model` defaults to the completion model. When the name is a mixed pool, search always uses the klava entry instead of the one `routing` picks for chat.
- Extensions' `nested_call` resolves its connection from the model entry it names (falling back to the completion model); the old `provider` field is gone, since nothing shipped depending on it.
- `klava check [--provider <model>]` iterates model entries; `klava launch --provider <model>` selects the active model for that launch.

### Removed

- `enable_pii` and `[extensions] disable_pii`. PII redaction has moved out of the core into the `klava-pii-guardrail` extension, which is configured by its own settings (`enabled`, `entities`, `operators`), so the host no longer needs a switch of its own. Both keys are now ignored (and dropped on the next save); to turn PII filtering off, disable the extension instead.

## [0.2.24] - 2026-10-19

### Added

- The decide model is now a **host capability any extension can call**: `decision: "decide"` carries typed `noul`/`choice`/`score` questions and an arbitrary state, and the host answers by re-running that same extension with `decide` filled in. klava owns the model, credentials, timeout and cost accounting; the extension owns the question and the evidence. Each extension gets at most two decide rounds per request, and every call is bounded to 16 questions. This removes the previous coupling where the host hard-coded an escalation question and built escalation-specific evidence.
- `decide_model` is the single config key for every classifier call (web-search tuning and extension `decide`). `search_decide_model` is still accepted as a legacy alias on load.
- `require_decide_for_nested_calls` (default `true`) makes the host refuse a `nested_call` from an extension that has not received an approved `decide` in the same request. The flag is sent to extensions as `require_decide` so they ask for the judgment when the host will require it; `false` still runs such calls but logs them as `UNVERIFIED`.
- `examples/nested_gate_probe.rs` measures the decide gate against real agent states, including the repeated-identical-result patterns that made the old extension escalate.
- `klava extensions show <name>` now lists the settings the extension **accepts**, with each default and the value currently in effect, before the effective config. The list comes from a new optional `[settings]` table in the manifest, so klava still interprets nothing: it prints the keys and strips the table before the config reaches the extension. Extensions without the table (anything installed before it existed) get an honest "the manifest declares none" message and can be updated in place with the new `klava extensions refresh-manifest <name>`.
- A manifest may declare `notice`, a warning shown (wrapped) by `install`, `enable` and `show`. `klava-model-escalation` uses it to state, before the first request, that escalation bills a second and deliberately more expensive model per token, that a looping session can burn a balance quickly, and that `max_escalations = 1` is the safe starting point. Its README carries the same warning in a callout.
- `klava-pii-guardrail` and `klava-model-escalation` now answer `--print-manifest`, declaring their settings and — for the guardrail — `fail_closed = true`. Previously they had no manifest at all, so klava could not show their keys and, worse, treated them as fail-open.

### Changed

- The decide model is no longer search-specific in name or ownership: `src/decision.rs` is the shared host API, and `src/search/decide.rs` only maps its typed answers onto search settings.
- `install` now validates a `--print-manifest` result through `toml::from_str` (the same parser the runtime uses) and writes it atomically, so a binary that prints invalid TOML leaves no partial file.
- `klava-pii-guardrail` fails **closed** at the host level: if it crashes, times out, or answers with malformed output, the request is rejected rather than sent unfiltered. Within a run, an analysis error still leaves the text untouched. The README explains the two layers and how to opt out.

## [0.2.23] - 2026-10-19

### Added

- Extension provisioning hooks. A manifest may declare a `[setup]` table (`args`, `summary`, `network`, `timeout_secs`); klava runs it — the extension's own binary, never a shell — after the extension is enabled. `klava extensions install … --trust` and the first `klava extensions enable` run it, `--no-setup` opts out, and the new `klava extensions setup <name>` runs it on demand (the retry path). The binary, its arguments and the network warning are printed before it runs; without `--trust` and without a terminal, setup is skipped. The hook receives the extension's effective config as JSON in `$KLAVA_EXTENSION_CONFIG`, so it can honour settings such as `model_dir`.
- Setup state is recorded in `<name>.setup.toml` next to the artifact (`args`, `artifact_sha256`, `ran_at`); a later enable skips an unchanged hook, and a changed argument list or a new artifact re-runs it. Failed runs are not recorded. `klava extensions list` gained a `SETUP` column (`none`/`pending`/`done`/`stale`) and `show` explains it.
- `install` asks a freshly installed executable for `<binary> --print-manifest` and stores valid TOML as its manifest, so a `cargo:`/`npm:` install can learn about a `[setup]` hook that the package itself could not carry. An existing manifest is never overwritten, and a missing flag, timeout or invalid TOML simply means "no manifest".
- `klava-compress --fetch-model` downloads the kompress model itself (resumable, with the directory resolved exactly as the runtime resolves `model_dir`), and `--print-manifest` exposes its `[setup]` hook. The model no longer needs a separate shell script.
- Yandex is selected automatically only when the query is specifically about Russia/CIS (law, government, companies, products, local services, events) or the user asks for it by name ("search this on Yandex", "найди в яндексе"). Writing in Russian is **not** a signal: a Russian-language question about a global topic stays on the default engine. Explicit "do not use Yandex" wins over both automatic signals. The provider-named-in-text check reads the user's own turn, not the model-rewritten query, and only conversation history of the current turn is consulted.
- `search_engine = "yandex"` selects the Yandex Search provider. klava sends the search request to the klava gateway with `plugins[0].engine = "yandex"`; the gateway runs Yandex Search, synthesizes an answer from the results, and returns the usual annotated completion, so the existing search loop, streaming splice, and Claude Code native `web_search` blocks work unchanged. Requires the klava provider (web search is already gated on it) and a gateway with Yandex configured.

### Changed

- An explicit `search_engine` is no longer overridden by the per-query decision model: the `engine_family` answer only applies when the config (or `--search-engine`) leaves the engine on `auto`. `web_fast`/`web_deep` describe retrieval depth, so a Russia/CIS query selects Yandex over them while keeping the model's context/breadth tuning.
- `klava extensions enable`/`install` accept `--trust`/`--yes`/`-y` and `--no-setup` through one parser; `--trust` on `install` now also consents to the extension's setup hook (previously it only consented to installing and running it).

### Fixed

- A printed manifest was always discarded: `str::parse::<toml::Value>()` uses toml 1.x's `FromStr` for `Value`, which rejects every document ("unexpected content, expected nothing"), so `[setup]` hooks were never seen after an install. Parsing now goes through `toml::from_str`.
- The kompress downloader no longer caps the whole body at 300 s (`timeout_recv_body` is a total budget, not an idle timeout) and no longer advertises gzip, whose `content-length` describes the compressed body and would break both resume and the size check. A server that ignores `Range` now truncates the partial file instead of appending to a stale prefix, a 416 resume removes the unusable `.part` instead of promoting it, and a short body is reported as incomplete rather than installed.
- `klava-compress` expands `~` in `$KLAVA_COMPRESS_MODEL_DIR` the same way it does in `model_dir`, so a fetch and a later run cannot disagree about where the model lives.
- Source distributions are now built from git (`sdist-generator = "git"`) so the workspace extension crates under `crates/` are included. maturin's cargo sdist drops `workspace.members` while the shipped `Cargo.lock` still lists those members, so `maturin build` from the sdist failed with `cannot update the lock file ... because --locked was passed`.
- A just-installed or just-updated executable can fail to start with `Text file busy` (os error 26) when another thread forked while the file was still open. Extension startup, `--print-manifest` probing and the setup hook now retry that transient error briefly instead of reporting the extension as broken.

## [0.2.21] - 2026-10-16

### Fixed

- PII guardrail no longer corrupts the request into invalid JSON. The guardrail serialized the whole request to one JSON string and redacted spans inside it, so the email recognizer could match the `n`/`t`/`r` of a structural escape (`\n`, `\t`, `\r`) when a redactable value followed a newline/tab/CR. Redaction then consumed the escape letter and left a lone backslash, and parsing the result failed with `400 JSON error: invalid escape` — most visible with large agent histories (e.g. pi) where a line ending immediately precedes an email or other matchable value. Redaction now walks the parsed JSON and processes each string *value*, so escapes are structurally impossible to corrupt and serde re-escapes on the way out.

## [0.2.20] - 2026-10-16

### Added

- Web search for launched agents: `klava launch --search` gives the agent a search tool that klava runs itself through the klava gateway (no agent-side setup); `--search-model MODEL` selects the model used for searches (implies `--search`). Search behavior is configurable via `enable_websearch`, `search_engine` (`web_fast`/`web_deep`/`web_native`/`yandex`/`auto`), `search_context` (`low`/`medium`/`high`), `search_max_results`, `search_prompt`, `search_include_domains`/`search_exclude_domains`, and `search_decide`/`search_decide_model` for model-decided per-query tuning.
- Managed services carry the search flag: systemd units and launchd plists render `--search` when web search is enabled, so the background proxy exposes the search tool to every agent that connects to it.

### Fixed

- `/v1/messages`: Anthropic **server** tools (e.g. Claude Code's `web_search_20250305`, `computer_20241022`, `bash_20241022`), which identify themselves by `type` and carry no `input_schema`, no longer fail the whole request with `400 JSON error: missing field 'input_schema'`. Such tools are parsed and dropped before the OpenAI transform (they can only run on Anthropic's backend), while the client's own function tools are forwarded as before.
- Claude Code's built-in `WebSearch` tool now counts as a real search. Claude sends a native sub-request (a `web_search_20250305` server tool plus `tool_choice: {type: "tool", name: "web_search"}`) and expects the answer as `server_tool_use` + `web_search_tool_result` content blocks; klava previously returned plain text, so Claude reported "Did 0 searches" even though the search ran. klava now detects the sub-request and answers with the native blocks (plus the answer text), in both streaming and non-streaming form, so Claude reports "Did 1 search" and surfaces the cited sources.

## [0.2.19] - 2026-10-16

### Added

- Managed background service: `klava launch` and `klava up --daemon` now provision a per-port systemd **user** service (`klava-<port>.service`) when a systemd user session is available. The unit is created/refreshed on first use (binary path, port and env keys), enabled for login auto-start, started idempotently and health-checked via `/health`. `Restart=always` makes the proxy self-healing — it survives agent sessions, terminals, crashes and reboots, so clients never see `ConnectionRefused` on `localhost:<port>` again. Several ports = several independent managed instances. `--no-service` on `launch` restores the old short-lived child-proxy behavior.
- macOS: the same managed-service behavior via launchd — per-port LaunchAgent `~/Library/LaunchAgents/klava-<port>.plist` with `RunAtLoad` (login auto-start) and `KeepAlive` (crash restart), rendered with env keys and rewritten when stale. `klava status` and `klava stop` cover launchd instances; `launchctl list | grep klava` shows their state. Falls back to the child proxy when no service manager exists (Linux without systemd, Windows).
- `klava status` now lists every managed instance and its systemd state when systemd is available.

### Changed

- `klava up --daemon` prefers the managed systemd service over the legacy fork-based daemon when systemd is available.
- Upstream error responses now carry actionable hints (401/403 → `klava login`, 429 → back off, 5xx → `klava check`), and connect failures mention `klava check`.

### Fixed

- `klava status` (legacy daemon path) no longer uses `pgrep -P` (which looked for children of the PID); it probes the process directly with `kill -0`.

## [0.2.18] - 2026-10-15

### Changed

- PII guardrail now masks inline image payloads by field instead of scanning the serialized request for `;base64,` markers. Only genuine encoded fields are masked (OpenAI/Responses `image_url` data URLs and Anthropic `{"type":"base64","data":…}` image sources), and payloads are restored after redaction. This also covers Anthropic image `source.data` (raw base64, with no `;base64,` marker) and Responses `input_image` URLs, and a `;base64,`-looking literal in code no longer shields a nearby crypto address from redaction.

## [0.2.17] - 2026-10-15

### Fixed

- PII guardrail no longer corrupts base64 image payloads. The request is serialized to one JSON string before redaction, so a `CryptoAddress`-shaped run inside an image's base64 (e.g. a 40-hex-char `0x…` sequence) was replaced with `<CryptoAddress>`, making the data URL invalid and the provider reject the whole request with `Invalid image data URL in messages[].content[].image_url.url: payload is not base64-encoded data`.
- `/v1/messages`: image blocks are now converted to a valid `data:<media-type>;base64,<data>` URL. The header was previously emitted as `;base,` (missing `64`), so the provider could not decode the image.

## [0.2.16] - 2026-10-15

### Fixed

- Streaming proxy no longer swallows upstream error events: `data: {"error": ...}` SSE payloads from the provider are now forwarded verbatim instead of being dropped, so clients (opencode, Codex) surface the real upstream error instead of "stream ended without finish_reason"
- The request body cap for streaming requests was raised from 1 MB to 128 MB, and the fixed 300 s per-request timeout was replaced with a 300 s read timeout, so long-lived streams are no longer cut off between tokens
- `/v1/responses`: top-level `instructions` (the Codex system prompt) is now forwarded to the provider as a leading system message instead of being dropped
- Anthropic `stop_reason` `"refusal"` is now produced for upstream `finish_reason: "content_filter"`, with the `stop_details` object emitted on both the message and the streaming `message_delta` (previously mapped to `end_turn`)

### Added

- `/v1/responses`: `reasoning.effort`, `tool_choice`, `parallel_tool_calls` and `text.format` (→ `response_format`) are now forwarded upstream
- `/v1/messages`: `tool_choice` (`auto`/`any`/`none`/named tool), `disable_parallel_tool_use` (→ `parallel_tool_calls`) and `metadata` are now forwarded upstream
- `/v1/messages`: `thinking.type` `"adaptive"` (newer Claude models) now selects the configured reasoning model, alongside the existing `"enabled"`
- `/v1/responses` streaming now emits `response.reasoning_summary_part.added` / `.done` around reasoning summary text, matching the upstream event order

### Added (tests)

- Router-level integration tests with an in-process fake upstream covering chat, Anthropic and Responses streaming paths (including upstream error forwarding, `instructions`, `content_filter` → refusal, adaptive thinking and reasoning-summary-part events), plus unit tests for `error`, `provider`, `pii_guardrail`, `proxy`, `models` and the two stream converters

## [0.2.15] - 2026-10-15

### Fixed

- `klava login` stores the catalog `openrouter_id` (e.g. `~deepseek/deepseek-flash-latest`) as the default model instead of the internal record id, fixing "is not a valid model ID" errors from the proxy; the non-interactive (`$KLAVA_MODEL`) and interactive model pickers now select the same ID the chat endpoint accepts
- `klava balance` and `klava whoami` parse the balance amount whether the server sends it as a JSON number or a decimal string; `whoami` reads the working `/v1/balance` endpoint instead of `/v1/me` (which the server does not expose)

## [0.2.13] - 2026-10-14

### Added

- `klava launch pi` launches the Pi agent harness (https://pi.dev): `setup` registers a `klava` provider (OpenAI-chat-completions wire API) in `~/.pi/agent/models.json`, preserving existing providers, registering the active klava provider's completion and reasoning models (deduplicated) as selectable models. `~/.pi/agent/settings.json` gets `defaultProvider: "klava"` and `defaultModel: <active model>`, so pi starts on the klava proxy with the right model out of the box; `run` passes `--provider klava --model <active model>` as well

## [0.2.12] - 2026-09-23

### Fixed

- Per-request overhead reduced: the PII guardrail and token estimation hooks are now gated by new `enable_pii` (default `true`) and `enable_token_stats` (default `false`) config options, and the PII analyzer is built once per process and reused instead of per request; the request logging hook no longer pretty-prints the full payload when verbose logging is disabled

## [0.2.11] - 2026-09-23

### Added

- Per-provider `chat_completions_path` config field for providers that expose the endpoint at a nested path (e.g. Immerse's `/v1/endpoints/generate/chat/completions`); defaults to `/v1/chat/completions`; `klava check` uses the same URL logic

### Fixed

- `klava check` now probes each provider's **own** `base_url`/`chat_completions_path` instead of always checking against the active provider; a missing base URL is reported as an error instead of panicking
- `klava check` now accepts reasoning-only responses (OpenRouter/DeepSeek/Qwen thinking models often return `content: null` when the output budget goes to chain-of-thought) and uses a larger `max_tokens` probe; any non-empty answer is valid when the HTTP status is good

## [0.2.10] - 2026-09-23

### Fixed
- Codex (`/v1/responses`) requests now select the configured reasoning model when `reasoning.effort` is set (previously the model override never fired on the Responses path)
- Non-streaming responses: provider `reasoning_content` (GLM/Qwen/CloudRu chain-of-thought) is now mapped to Anthropic `thinking` blocks and Responses API `reasoning` items; usage details (`thinking_tokens`, cached tokens) are propagated to both formats
- Anthropic `tool_result` blocks with `is_error: true` now prefix the content with `Error:` (OpenAI tool messages have no error flag); string tool results are no longer quoted as JSON
- Non-streaming `/v1/responses` responses now normalize upstream `chatcmpl-` ids to the standard `resp_` form and echo request parameters (`tools`, `tool_choice`, `temperature`, `top_p`, `truncation`, `parallel_tool_calls`, `store`, `metadata`, `instructions`, `service_tier`, `previous_response_id`)

## [0.2.9] - 2026-08-16

### Added

- When the configured port is busy, `klava up` and `klava launch` now fall back to the next available port (+1)

### Changed

- Renamed `hooks::hooks` module to `hooks::chain`
- Refactored `diagnostic.rs`, agent (claude/codex) and streaming code, cleaned up clippy warnings
- Requests now no longer dump JSONL stream logs into `tests/fixtures`

## [0.2.8] - 2026-08-03

### Added

- New `check` CLI command (`klava check`) to test provider connectivity and API responses

## [0.2.7] - 2026-08-03

### Fixed

- Fixed Codex agent configuration to support Codex CLI versions >= 0.134.0
- Profile config now written to `~/.codex/klava.config.toml` instead of modifying `~/.codex/config.toml`
- Legacy `[profiles.klava]` table in `config.toml` is automatically cleaned up on run
- Updated agent launch to use correct `--profile klava` flag with separate profile file

### Added

- Passthrough `extra` field on `OpenAIRequest` to forward unknown provider params (`stream_options`, `parallel_tool_calls`, `metadata`, `max_completion_tokens`, etc.)
- `openai_to_call_id()` utility to normalize vLLM/CloudRu/Qwen `chatcmpl-tool-…` tool-call ids into the standard Responses API `call_…` format
- Non-streaming handler for `/v1/responses` endpoint (`handle_non_streaming_responses`)

### Changed

- Qwen streaming requests now only set `incremental_output: true` without forcing `enable_thinking: false`
- Streaming Responses API converter takes model directly from the request instead of the `x-model` header
- `response.completed` output now includes both text message and function_call items when both are present (previously text was dropped)

### Fixed

- Tool-call id normalization for non-streaming `/v1/responses` responses (vLLM/CloudRu/Qwen `chatcmpl-tool-…` → `call_…`)
- Removed duplicate function_call aggregation in `openai_to_responses` transform

## [0.2.5] - 2026-08-03

### Fixes

- Fix of claude launcher new version
- New claude version works

## [0.2.2] - 2026-05-07

### Changed

- Refactored Qwen provider and streaming architecture
- Consolidated streaming logic into specialized modules
- Improved streaming test coverage with new fixtures
- Updated feature flags for better provider configuration

## [0.2.1] - 2026-04-11

### Added

- Add port arg to launch command

### Changed

- Updated build script

## [0.2.0] - 2026-04-10

### Added

- Support for multiple providers (Qwen, OpenRouter, Cloud.ru)
- Security guardrails to filter secret keys
- OpenCode agent support
- Improved CLI interface and configuration

## [0.1.0] - 2026-03-XX

### Added

- Initial release with Claude Code support
- Basic proxy functionality
- Configuration management

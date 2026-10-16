# Changelog

## [0.2.20] - 2026-10-16

### Added

- Web search for launched agents: `klava launch --search` gives the agent a search tool that klava runs itself through the klava gateway (no agent-side setup); `--search-model MODEL` selects the model used for searches (implies `--search`). Search behavior is configurable via `enable_websearch`, `search_engine` (`web_fast`/`web_deep`/`web_native`/`auto`), `search_context` (`low`/`medium`/`high`), `search_max_results`, `search_prompt`, `search_include_domains`/`search_exclude_domains`, and `search_decide`/`search_decide_model` for model-decided per-query tuning.
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

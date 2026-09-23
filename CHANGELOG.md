# Changelog

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

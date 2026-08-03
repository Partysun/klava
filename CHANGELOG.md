# Changelog

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

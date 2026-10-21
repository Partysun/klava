# @zatsepin/klava — run code agents against any provider

Installs the `klava` binary: launch Claude Code, OpenCode, Pi or Codex against
any OpenAI-compatible provider (OpenRouter, Qwen, Cloud.ru, local LLMs) with a
local proxy in between, and keep API keys and other secrets out of what the
agent can read.

Node ≥ 18 is needed once, at install time, to pick the platform package — the
installed `klava` is a standalone binary with no Node runtime.

## Install

```bash
npm install --global @zatsepin/klava
klava --help
```

One-off, no install:

```bash
npx @zatsepin/klava --help
```

The binary comes from an optional dependency matched to your OS and CPU —
`@zatsepin/klava-linux-x64-gnu`, `@zatsepin/klava-linux-arm64-gnu`,
`@zatsepin/klava-darwin` (universal2: Apple Silicon + Intel) or
`@zatsepin/klava-win32-x64-msvc` — installed automatically, never a source
build. Alpine/musl and other platforms are not packaged: use
`cargo install klava` there.

## Quick start

```bash
klava launch                 # pick an agent and a model, start the proxy, launch it
klava launch claude          # Claude Code against your configured model
klava launch claude --provider ~deepseek/deepseek-flash-latest
klava serve                  # proxy only, on :8085
```

The first `launch` walks you through picking a provider and a model, stores it
in the config, and from then on the agent talks to `klava`, which translates
between the agent's wire format (Anthropic or OpenAI) and your provider's.

## Secrets filtering

`klava` inspects the files the agent is about to send and redacts leaked
credentials — API keys, private keys, tokens — before the request leaves your
machine. Every request and response is optionally logged (`--verbose`).

## Platforms

| OS | Package | Notes |
|----|---------|-------|
| Linux x64 / arm64 | `klava-linux-*-gnu` | glibc ≥ 2.17, no build tools needed |
| macOS (Intel + Apple Silicon) | `klava-darwin` | one universal2 binary |
| Windows x64 | `klava-win32-x64-msvc` | |

Source, Rust crates and the pip package: https://github.com/Partysun/klava

# klava

_README file written by hands primarily_

Klava is a CLI tool for using code agents with any provider. Use Claude Code with your OpenAI-compatible provider. Make code agents more secure by filtering out leaked secret keys and cryptographic keys from your filesystem.

## Features

- **Fast & Lightweight**: Written in Rust with async I/O
- **Universal**: Works with any OpenAI-compatible API (OpenRouter, Qwen, Cloud.ru, local LLMs)
- **Security Guardrails**: Filters out leaked secret keys and cryptographic keys from your filesystem before sending requests to LLM APIs

## Installation

You can install klava from [pip](https://pypi.org/), [cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html), [mise](https://github.com/jdx/mise)!

```bash
pip install klava
cargo install klava --locked
mise use -g cargo:klava
```

Or you can build klava from source. You need to install rustup, clone the repository and then

```bash
cargo install --path . --locked
```

### Launch AI Code Agent

The `launch` command starts the proxy server and launches an AI code agent with appropriate environment variables:

```bash
klava launch
klava launch claude
klava launch claude --provider qwen
klava launch opencode
```

Notice: When launching opencode, you need to manually select the "klava" model using the model command in opencode after it starts.

Supported code agents:

| Agent      | Description    | Link                      |
| ---------- | -------------- | ------------------------- |
| `claude`   | Claude Code    | <https://code.claude.com> |
| `opencode` | OpenCode Agent | <https://opencode.ai/>    |

### Configuration File Location

To find the config file path on your system, run:

```bash
klava config
```

Platform-specific config file locations:

- **Linux**: `~/.config/klava/config.toml`
- **macOS**: `~/Library/Application Support/klava/config.toml`
- **Windows**: `%APPDATA%\klava\config.toml`

#### Config file

```toml
port = 48017
verbose = false
active_provider = "qwen"

[[providers]]
name = "qwen"
type = "qwen-code"

[[providers]]
name = "cloudru"
type = "openai-compatible"
base_url = "https://foundation-models.api.cloud.ru"
api_key_name = "CLOUD_RU_KEY"
reasoning_model = "zai-org/GLM-4.7"
completion_model = "zai-org/GLM-4.7"

[[providers]]
name = "openrouter"
type = "openai-compatible"
base_url = "https://openrouter.ai/api"
api_key_name = "OPENROUTER_API_KEY"
reasoning_model = "z-ai/glm-5.1"
completion_model = "z-ai/glm-5.1"
```

Notice: base_url should not include /v1 at the end!

Notice: You can use api_key_name if you have the api_key in your environment,
if you want you can set api_key in the toml file with the api_key variable.

Notice: The type can be openai-compatible or qwen-code.
qwen-code uses an authentication flow involving reverse engineering of the Qwen Code application. You can use 1000 free requests per day of the great large model, but with risks due to this being an unofficial implementation of qwen-code.

### Manual Claude Execution

If you run `klava up`, and you want to run claude manually use this command

```
ANTHROPIC_BASE_URL=http://localhost:48017 claude
```

Don't forget to change the port to your klava proxy server port.

### View Proxy Logs

View or follow proxy server logs:

```bash
klava logs          # View recent logs
klava logs -f       # Follow logs (like tail -f)
klava logs --follow # Follow logs (long form)
```

## Known Limitations

The following Anthropic API features are **not supported** currently (Claude Code and similar tools work without these parameters):
Many features like images are not ready yet...
It's a work-in-progress tool.

## Troubleshooting & Known Pitfalls

- ❌ Wrong: `https://openrouter.ai/api/v1`
- ✅ Correct: `https://openrouter.ai/api`
- The proxy automatically adds `/v1/chat/completions`

## Roadmap & Contributing

Contributions welcome!

Current vision is:

- Refactoring -> split codebase into many crates like proxy, cli, providers ...)
- Add more Guardrails
- Add Token optimization algorithms. Reduce cost is a crucial goal
- Creating extensions mechanism as first class citizen.
- Add more providers and agents
- Add benchmark wars between agents
- Add remote hand control using Telegram and Mobile

If you like it you can support me:

- Subscribe -> <https://zatsepin.dev/subscribe>

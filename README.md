# klava

High-performance Rust proxy that translates Anthropic API requests to OpenAI-compatible format. Use Claude Code with OpenRouter, native OpenAI, or any OpenAI-compatible endpoint.

## Features

- **Fast & Lightweight**: Written in Rust with async I/O
- **Universal**: Works with any OpenAI-compatible API (OpenRouter, Cloud.ru, local LLMs)

### Launch AI Code Agent

The `launch` command starts the proxy server and launches an AI code agent with appropriate environment variables:

```bash
# Interactive selection - shows menu to choose agent
klava launch

# Launch specific agent directly
klava launch claude

# Launch with verbose logging
klava launch claude --verbose
```

Supported code agents:

| Agent      | Description    | Link                      |
| ---------- | -------------- | ------------------------- |
| `claude`   | Claude Code    | <https://code.claude.com> |
| `opencode` | OpenCode Agent | Coming soon               |

The launch command automatically:

- Starts the proxy server in the background
- Sets necessary environment variables (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, etc.)
- Enables demo mode for the agent
- Shuts down the proxy when the agent exits

If you run `klava up`, and you want to run claude manually use this command

```
ANTHROPIC_BASE_URL=http://localhost:8085 claude
```

Dont' forget to change port to your PORT of klava proxy server.

## Known Limitations

The following Anthropic API features are **not supported** currently (Claude Code and similar tools working without these parameters):):
Many of features like images, full list is not ready yet...
It's a WIP tool.

## Troubleshooting & Known Pitfalls

**Error: `KLAVA_BASE_URL is required`**  
→ You must set the upstream endpoint URL. Examples:

- OpenRouter: `https://openrouter.ai/api`
- OpenAI: `https://api.openai.com`
- Local: `http://localhost:11434`

**Error: `405 Method Not Allowed`**  
→ Your `KLAVA_BASE_URL` likely ends with `/v1`. Remove it!

- ❌ Wrong: `https://openrouter.ai/api/v1`
- ✅ Correct: `https://openrouter.ai/api`
- The proxy automatically adds `/v1/chat/completions`

**Model not found errors**  
→ Set `REASONING_MODEL` and `COMPLETION_MODEL` to override the models from client requests

## Testing

The project includes integration tests using [Hurl](https://hurl.dev/), a command line tool that can run HTTP requests defined in simple text files.

### Installing Hurl

```bash
# macOS with Homebrew
brew install hurl

# Linux
curl --proto '=https' --tlsv1.2 -sSf https://hurl.dev/install.sh | sh

# Or from releases
# https://github.com/Orange-OpenSource/hurl/releases
```

### Running Tests

The `tests.hurl` file contains comprehensive tests for all proxy endpoints:

```bash
# Run tests against local development server
hurl --variable host=http://localhost:3000 --test tests.hurl

# Run with mise task (uses localhost:8085)
mise run test-hurl

# Run without host variable (uses default from hurl file if set)
hurl --test tests.hurl
```

## Contributing

Contributions welcome!

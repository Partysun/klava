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
klava launch claude --provider ~deepseek/deepseek-flash-latest
klava launch opencode
klava launch pi
```

The first run has no models yet: `klava launch` (or `klava providers add`)
opens the onboarding wizard, which logs in to a provider, lists its models,
checks the connection and writes the ones you pick.

`--provider <model>` uses that configured model for this launch.

Harnesses see the **real model names** (`~deepseek/deepseek-flash-latest`,
`glm-5.1`, …) — the proxy advertises them on `GET /v1/models` and each agent's
config lists them, so the model picker and the model label in the UI show what
actually answers. A request picks the model named in its body; a name that is
not configured falls back to the configured defaults.

When several `[[models]]` entries share a name they form a pool. Pick how it is
used with `routing`:

| `routing`          | Behaviour                                                          |
| ------------------ | ------------------------------------------------------------------ |
| `"shuffle"` (default) | random entry per request — spreads load/cost across endpoints   |
| `"round_robin"`    | cycles through the pool in declaration order                       |
| `"first"`          | always the first entry (declaration order = failover priority)     |

#### Managed background service (Linux/systemd, macOS/launchd)

`klava launch` (and `klava up --daemon`) manage the proxy as a per-port
background service whenever a supported service manager is available:

- **Linux**: systemd user unit `klava-<port>.service` — auto-started at
  login, `Restart=always` on crash;
- **macOS**: launchd LaunchAgent `~/Library/LaunchAgents/klava-<port>.plist`
  — `RunAtLoad` at login, `KeepAlive` on crash.

In both cases the unit/plist is created on first use and kept up to date
(binary path, port, env keys), the service is started idempotently and
health-checked via `/health`, and each port is its own instance — so several
proxies can run side by side. Since the service takes over the port, agents
started later just reuse it — no per-session proxy spawn, no
"proxy is dead" surprises.

```bash
klava status                                   # list managed instances
klava restart                                  # restart the proxy on the configured port
klava stop                                     # stop the proxy on the configured port
systemctl --user status klava-48017            # Linux: unit status
journalctl --user -u klava-48017 -f            # Linux: live proxy logs
launchctl list | grep klava                    # macOS: instance state
tail -f ~/Library/Logs/klava-48017.log         # macOS: live proxy logs
```

Pass `--no-service` to `klava launch` (or use plain `klava up`) to run a
short-lived child/foreground proxy instead — e.g. for CI or custom ports.
On Windows the managed service is not available yet; `klava up` keeps
working there.

Supported code agents:

| Agent      | Description    | Link                      |
| ---------- | -------------- | ------------------------- |
| `claude`   | Claude Code    | <https://code.claude.com> |
| `opencode` | OpenCode Agent | <https://opencode.ai/>    |
| `pi`       | Pi Agent       | <https://pi.dev>          |

### Configuration File Location

To find the config file path on your system, run:

```bash
klava config
```

Platform-specific config file locations:

- **Linux and macOS**: `~/.config/klava/config.toml`
- **Windows**: `%APPDATA%\klava\config\config.toml`

Set `KLAVA_CONFIG_FILE=/path/to/config.toml` to use another file (a second
profile, or a throwaway config for tests and CI).

#### Config file

```bash
klava providers add       # onboarding wizard: Klava, OpenRouter, Cloudflare, Qwen, custom
klava providers list      # what is configured, which model is the default
klava providers check     # one tiny completion per model
klava providers set       # choose the default model
klava providers remove m  # drop a model (all entries with that name)
```

The wizard writes self-contained `[[models]]` entries — each carries its own
connection (type, base URL, key), so the same name on two endpoints is a pool
and no global "active provider" switch exists:

```toml
port = 48017
verbose = false
completion_model = "~deepseek/deepseek-flash-latest"
reasoning_model = "~deepseek/deepseek-flash-latest"
routing = "shuffle"                      # shuffle | round_robin | first
decide_model = "typesafe/jev-1.13"       # one entry; backend auto-detected

[[models]]
name = "~deepseek/deepseek-flash-latest"
type = "klava"                           # klava login, no key in the file

[[models]]
name = "~deepseek/deepseek-flash-latest"  # same name = pool with the entry above
type = "openai-compatible"
base_url = "https://openrouter.ai/api"
api_key_name = "OPENROUTER_API_KEY"

[[models]]
name = "glm-5.1"
type = "openai-compatible"
base_url = "https://foundation-models.api.cloud.ru"
api_key_name = "CLOUD_RU_KEY"

[[models]]
name = "deepseek-v4-flash"
type = "openai-compatible"
base_url = "https://chat.immers.cloud"
api_key_name = "IMMERSE_TOKEN"
chat_completions_path = "/v1/endpoints/generate/chat/completions"

[[models]]
name = "qwen3-coder-plus"
type = "qwen-code"
```

Notice: base_url should not include /v1 at the end!

Notice: If your provider exposes the chat completions endpoint at a nested
path (e.g. `…/v1/endpoints/generate/chat/completions`), set
`chat_completions_path` to that absolute path. It defaults to
`/v1/chat/completions` otherwise.

Notice: You can use api_key_name if you have the api_key in your environment,
if you want you can set api_key in the toml file with the api_key variable.
For `type = "klava"` no key is needed: the wizard uses `klava login` and klava
reads the saved session.

Notice: an entry with neither `api_key` nor `api_key_name` sends no
`Authorization` header — that is how a local server (Ollama, llama.cpp) is
configured. Naming an `api_key_name` whose variable is empty is an error.

Notice: `upstream_model` overrides the id sent upstream when the public name
differs (e.g. a friendly alias). `name` is what agents send and see.

Notice: klava model names may start with `~`, which the shell expands. Quote
them on the command line: `klava providers set '~deepseek/deepseek-flash-latest'`.

Notice: The type can be `openai-compatible`, `klava` or `qwen-code`.
qwen-code uses an authentication flow involving reverse engineering of the Qwen Code application. You can use 1000 free requests per day of the great large model, but with risks due to this being an unofficial implementation of qwen-code.

#### Cloudflare Workers AI

Cloudflare has no OpenAI-style `/v1/models` listing, so the wizard asks for the
account id and stores the account-scoped endpoint:

```toml
[[models]]
name = "@cf/meta/llama-4-scout-17b-16e-instruct"
type = "openai-compatible"
base_url = "https://api.cloudflare.com/client/v4/accounts/<ACCOUNT_ID>/ai/v1"
api_key_name = "CLOUDFLARE_AUTH_TOKEN"
```

#### Decide model (System One / jigor)

`decide_model` names exactly one `[[models]]` entry. Every classifier call
klava makes — the web-search tuning and any extension that asks for a `decide` —
uses it. The jigor backend is **detected from that entry**, so there is no
backend switch to set:

| Entry type / URL                                        | Backend                |
| ------------------------------------------------------- | ---------------------- |
| `type = "klava"` (the default)                          | klava gateway          |
| host `openrouter.ai`                                    | OpenRouter             |
| host `api.cloudflare.com` (`…/accounts/<id>/ai/…`)      | Cloudflare Workers AI  |
| any other OpenAI-compatible gateway                     | klava wire (default)   |

Credentials come from the entry (`api_key` / `api_key_name`), falling back to
the saved `klava login`. Without a configured entry the built-in klava gateway
is used, so `decide` works out of the box after `klava login`. Set `jigor =`
on the entry only for a self-hosted gateway that cannot be recognised.

Because a classifier must be deterministic, a `decide_model` name shared by
several entries is rejected with an error — unlike regular models, which pool.

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

Note: The `-v` flag works only with the `klava up` command to enable verbose logging. When used with `klava logs`, it doesn't change the output. To see verbose logs, start the server with `klava up -v` first, then view the logs.

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
- Extensions: installable at runtime, in any language. See [EXTENSIONS.md](EXTENSIONS.md).
- Add more providers and agents
- Add benchmark wars between agents
- Add remote hand control using Telegram and Mobile

If you like it you can support me:

- Subscribe -> <https://zatsepin.dev/subscribe>

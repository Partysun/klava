# Extensions

klava ships with **no extensions**. Install one at any time, in any language,
without rebuilding klava.

An extension is an executable that reads one JSON object on stdin and writes one
JSON object on stdout. That is the whole contract.

## Install

### With the built-in installer

```sh
klava extensions install ./my-extension --trust
klava extensions install cargo:klava-compress --trust
klava extensions install cargo:klava-compress@0.1.0 --trust
klava extensions install cargo-path:./my-crate --trust
klava extensions install cargo-git:https://github.com/user/repo --trust
klava extensions install npm:my-extension --trust
klava extensions install npm:@scope/pkg@1.2.3 --trust
```

| Source | What klava does |
|---|---|
| `./path`, `/abs/path` | Copies the executable (keeps a symlink if given). A directory works if it contains exactly one executable. |
| `cargo:<crate>[@ver]` | Runs `cargo install --root <staging>`. |
| `cargo-path:<dir>` | Same, from a local crate directory. |
| `cargo-git:<url>[#rev]` | Same, from git. |
| `npm:<pkg>[@ver]` | Runs `npm pack`, extracts, and takes the `bin` from `package.json`. |

Installing runs third-party code, so klava prints the source and target and
requires `--trust` (or an interactive `y`). The exact command is printed before
it runs. Everything is staged and moved into place atomically: a failed install
cannot leave a half-written executable.

`--name <name>` overrides the extension name. `--no-enable` installs without
activating it. `--no-setup` enables it but skips its provisioning hook (see
[Provisioning](#provisioning-setup)); an extension with a hook and no model yet
is `pending` in `klava extensions list`.

Other commands:

```sh
klava extensions path      # where klava looks, in order
klava extensions list      # name, status, setup, version, source, path
klava extensions verify    # re-check recorded checksums
klava extensions setup <name>   # run the [setup] hook
klava extensions remove <name>  # also removes <name>.toml, .source.toml, .setup.toml
```

### Manually

Drop an executable into the data directory:

```sh
mkdir -p ~/.local/share/klava/extensions
install -m 755 my-extension ~/.local/share/klava/extensions/my-extension
```

## Where klava looks

Directories are searched in order, and the **first** match for a name wins:

| Order | Directory | Purpose |
|---|---|---|
| 1 | `$KLAVA_EXTENSIONS_DIR` | Explicit override. When set, it is the only directory searched. |
| 2 | `$XDG_DATA_HOME/klava/extensions` (`~/.local/share/klava/extensions`) | Where `extensions install` writes. |
| 3 | `/usr/local/share/klava/extensions` | Installed system-wide by an admin. |
| 4 | `/usr/share/klava/extensions` | Distribution packages. |

On Windows the data directory is `%LOCALAPPDATA%\klava\extensions`; on macOS,
`~/.local/share/klava/extensions` (klava uses the XDG layout on every platform).

A file is picked up when it is a regular executable, is not hidden, and is not
a `.toml` file. Nothing else is required: no entry point, no registration, no
recompile.

## Provenance

`install` writes `<name>.source.toml` next to the artifact:

```toml
source = "cargo:klava-compress@0.1.0"
version = "0.1.0"
sha256 = "..."
installed_at = "2026-10-18T12:00:00+00:00"
```

`klava extensions verify` re-hashes the artifact and reports a mismatch. A
changed binary is not blocked at load time — that would cost a full hash on
every request — but `verify` makes tampering visible.

## Enable

Nothing runs until it is enabled. `enabled` is the only key klava itself
interprets.

```toml
[extensions.settings.my-extension]
enabled = true
```

or `klava extensions enable my-extension`. The lookup order is:

1. `[extensions.settings.<name>].enabled` in the klava config,
2. `enabled` in the extension's own manifest,
3. otherwise disabled.

## Manifest

Optional, and entirely owned by the extension. klava reads `enabled`,
`fail_closed`, `timeout_secs`, and the `[setup]` table; everything else is passed
straight through in the request's `config` field.

```toml
enabled = true
fail_closed = false     # false: a failure is logged and the request continues
timeout_secs = 20       # default 20; the child is killed after this
threshold = 0.7         # your own key, opaque to klava

[setup]                 # optional provisioning hook; see below
args = ["--fetch-model"]
```

### Provisioning: `[setup]`

Some extensions need a large asset that should not be part of the download or
of every request — `klava-compress` needs a ~573 MB model. Such an extension
declares how to provision itself:

```toml
[setup]
args = ["--fetch-model"]            # argv for this extension's own binary
summary = "Download the model (~573 MB)"   # shown before it runs
network = true                      # shown as a network-download warning
timeout_secs = 3600                 # default 300; the hook is killed after this
```

The hook is the extension binary itself with `args`, never a shell command. klava
runs it **after the extension is enabled**, not while installing:

* `klava extensions install <source> --trust` → runs it after enabling,
* `klava extensions enable <name>` → runs it on first activation or after the
  binary changes,
* `klava extensions setup <name>` → always runs it (the retry path).

Consent works like this: `--trust`/`--yes`/`-y` on `install` or `enable`
authorises the hook; `--no-setup` opts out; an interactive `enable` without
either asks. The binary, its arguments, and the network warning are printed
before anything runs. Without a terminal and without `--trust`, setup is skipped
and the retry command is printed — an unattended run never starts a download.

The hook inherits the extension's **effective config** (manifest merged with
`[extensions.settings.<name>]`) as JSON in `$KLAVA_EXTENSION_CONFIG`, so it can
honour settings such as where to put the asset.

A successful run is recorded in `<name>.setup.toml` next to the artifact:

```toml
args = ["--fetch-model"]
artifact_sha256 = "df6e..."          # from `<name>.source.toml`, when present
ran_at = "2026-10-19T14:40:00+03:00"
```

klava re-runs the hook when the arguments or the artifact change, so an upgrade
provisions itself again, and it never records a failed run. While a hook runs,
`.<name>.setup.lock` keeps a second concurrent setup from writing the same
files; a lock older than the hook's timeout is treated as abandoned.

Setup is best effort: a failed hook leaves the extension **enabled** and failing
open, with a message naming the retry command. `klava extensions list` shows a
`SETUP` column — `none`, `pending`, `done`, `stale` — and `show` explains it.

## Protocol

### Request

klava writes exactly one JSON object to stdin and closes it:

```json
{
  "protocol": 1,
  "config": { "enabled": true, "threshold": 0.7 },
  "payload": {
    "model": "…",
    "messages": [{ "role": "user", "content": "…" }]
  },
  "reason": "user",
  "source_api": "openai",
  "model": "…",
  "nested_calls_used": 0
}
```

| Field | Meaning |
|---|---|
| `protocol` | Always `1` for now. Reject anything you do not understand. |
| `config` | Manifest merged with the user's settings table. |
| `payload` | The OpenAI-shaped chat-completions request. |
| `reason` | `user`, `continuation`, `retry`, or `direct`. |
| `source_api` | `openai`, `anthropic`, or `responses`. |
| `model` | The model the request will use. |
| `nested_calls_used` | Nested calls already made for this request. |

### Response

Write one JSON object to stdout and exit 0:

```json
{ "decision": "continue" }
```

| `decision` | Effect | Extra fields |
|---|---|---|
| `continue` | Leave the request alone. | — |
| `replace` | Use `payload` instead. | `payload` |
| `append` | Add `messages` to the conversation. | `messages` |
| `deny` | Reject the request. | `reason`, optional `fail_closed` |
| `nested_call` | Ask klava to run a second model and fold the answer in. | `nested_call` |

An unknown `decision`, or `replace` without a `payload`, is treated as
`continue`. klava never blanks a request because an extension was confused.

`nested_call` shape:

```json
{
  "decision": "nested_call",
  "nested_call": {
    "provider": "klava",
    "model": "~anthropic/claude-opus-latest",
    "mode": "append",
    "system": "You are a senior reviewer.",
    "instruction": "Point out what is wrong.",
    "max_tokens": 2048
  }
}
```

`mode` is `append` (default; the answer is added as a user message and the
original model continues) or `take_over` (the answer is returned to the client
as the assistant turn). The extension never makes the call itself: credentials,
routing, timeouts and the recursion boundary stay in klava, so an extension
cannot recurse.

### stderr

Anything on stderr is forwarded to klava's log as a warning, one line each.
Use it for diagnostics; never write it to stdout.

## Failure and safety

- A crash, nonzero exit, timeout, invalid JSON, or oversized output is a
  failure. By default klava logs it and sends the request unchanged.
- Set `fail_closed = true` (manifest, or per-response for `deny`) to reject the
  request instead. Only do that for a genuine gate.
- Output is capped at 8 MiB and stderr at 64 KiB.
- One process per request that has an enabled extension. Startup cost is a
  `spawn`, which is negligible next to a model call.

## Example

A complete extension that appends a note to the last user message:

```python
#!/usr/bin/env python3
import json, sys

request = json.load(sys.stdin)
payload = request["payload"]

for message in reversed(payload.get("messages", [])):
    if message.get("role") == "user" and isinstance(message.get("content"), str):
        message["content"] += "\n\n(Reviewed by my-extension.)"
        break

json.dump({"decision": "replace", "payload": payload}, sys.stdout)
```

```sh
chmod +x my-extension
mv my-extension ~/.config/klava/extensions/
klava extensions enable my-extension
```

## CLI

```sh
klava extensions                        # list: name, status, setup, version, source
klava extensions show <name>            # status, setup state, path, effective config
klava extensions install <source> --trust
klava extensions install <source> --trust --no-setup   # enable, but do not provision
klava extensions setup <name>           # run the [setup] hook now (re-runs it)
klava extensions enable <name> [--trust|--no-setup]
klava extensions disable <name>
klava extensions set <name> key=value
klava extensions verify
klava extensions remove <name>
```

`set` parses values as JSON when it can, so `threshold=0.7` and
`roles=["tool"]` both work.

`install` adopts a manifest the extension prints: after a `cargo:`/`npm:`
install, klava runs `<binary> --print-manifest` once and stores valid TOML as
`<name>.toml` (an existing manifest is never overwritten). That is how a
package-manager install learns about a `[setup]` hook it could not carry.


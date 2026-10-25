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
klava extensions install npm:@zatsepin/klava-compress-linux-x64-gnu --trust
```

| Source | What klava does |
|---|---|
| `./path`, `/abs/path` | Copies the executable (keeps a symlink if given). A directory works if it contains exactly one executable. |
| `cargo:<crate>[@ver]` | Runs `cargo install --root <staging>`. |
| `cargo-path:<dir>` | Same, from a local crate directory. |
| `cargo-git:<url>[#rev]` | Same, from git. |
| `npm:<pkg>[@ver]` | Runs `npm pack`, extracts, and takes the `bin` from `package.json`. Because `npm pack` installs no dependencies and runs no postinstall, the package must contain the executable itself — that is why the klava extensions publish one package per crate *and* platform, e.g. `npm:@zatsepin/klava-compress-linux-x64-gnu`. |

Installing runs third-party code, so klava prints the source and target and
requires `--trust` (or an interactive `y`). The exact command is printed before
it runs. Everything is staged and moved into place atomically: a failed install
cannot leave a half-written executable.

`--name <name>` overrides the extension name. `--no-enable` installs without
activating it. `--no-setup` enables it but skips its provisioning hook (see
[Provisioning](#provisioning-setup)); an extension with a hook and no model yet
is `pending` in `klava extensions list`.

## Update

An installed extension records where it came from in `<name>.source.toml` right
next to the artifact. `update` replays that recorded source through the same
staged, atomic installer — klava never guesses a registry name or re-derives a
URL:

```sh
klava extensions update                  # every installed extension
klava extensions update <name>…         # just these
klava extensions update --check         # report only; nothing is built
klava extensions update --force         # rebuild even when already at the latest
klava extensions update --trust         # consent to building + setup, as on install
```

What one update does:

- **Skips** extensions klava does not manage (installed into `/usr/share` by a
  package manager — update them there) and extensions with no recorded source
  (reinstall them explicitly). Both are reported with a reason.
- **Compares versions first** so a no-op costs nothing: `npm:` packages and
  `cargo:` crates are checked against the registry (`npm view`,
  `cargo search`), local files are compared by content, and a local crate's
  `[package].version` is read from its manifest. Nothing is built when the
  recorded version already is the latest, unless `--force` says otherwise. The
  comparison is best effort: no network or registry access, a git branch (which
  moves without a version bump), or a missing manifest all mean "rebuild".
- **Drops pins**: `cargo:x@0.1.0` moves to the newest release and the record is
  rewritten as `cargo:x`; `cargo-git:url#rev` tracks the default branch. The
  move is printed. Pin again with `install …@ver`.
- **Never touches settings or enabled state** — both live in the klava config,
  which update only reads.
- **Provisions again** when the update landed a new binary: the setup record
  fingerprints the artifact, so the hook re-runs with the same consent and
  network warning as an install. The extension stays enabled if a model
  download fails; `klava extensions setup <name>` is the retry.
- **Continues after a failure**: one broken source does not stop the batch.
  The summary prints `updated` / `up to date` / `skipped` / `failed` and the
  exit code is non-zero exactly when something failed. A failed update leaves
  the previous binary in place — `commit` moves it aside first, never halfway.

Consent works like `install`: updating rebuilds third-party code, so without a
terminal and without `--trust` the whole run is refused before anything is
built. With a terminal, one prompt lists every source and covers the batch;
`--trust` also authorises re-running setup hooks, `--no-setup` opts out.

`--check` answers "what would change" without building or writing anything: the
table shows `current`, `available` (with the version it would move to), or
`rebuild (version unknown)` for sources whose version cannot be learned cheaply.
It exits 0 even when updates are available; having updates is not an error.

The resolved version — what the installer actually picked, not what the source
requested — is recorded at install time (`cargo install`'s ledger for crates,
the extracted `package.json` for npm), so `extensions list`'s `VERSION` column
and the comparison both work for unpinned installs. Extensions installed before
this existed report `<unknown>` until their first update.

Other commands:

```sh
klava extensions path      # where klava looks, in order
klava extensions list      # name, status, setup, version, source, path
klava extensions verify    # re-check recorded checksums
klava extensions setup <name>   # run the [setup] hook
klava extensions refresh-manifest <name>  # re-ask the binary for its manifest
klava extensions remove <name>  # also removes <name>.toml, .source.toml, .setup.toml
```

### Manually

Drop an executable into the data directory:

```sh
mkdir -p ~/.local/share/klava/extensions
install -m 755 my-extension ~/.local/share/klava/extensions/my-extension
```

### The klava extensions on npm

The three extensions klava publishes are also on npm as prebuilt binaries, one
package per crate and platform. They install with no Rust toolchain and no
build step:

| Extension | Package (append the platform) |
|---|---|
| `klava-compress` | `@zatsepin/klava-compress-<platform>` |
| `klava-pii-guardrail` | `@zatsepin/klava-pii-guardrail-<platform>` |
| `klava-model-escalation` | `@zatsepin/klava-model-escalation-<platform>` |

Platforms: `linux-x64-gnu`, `linux-arm64-gnu`, `darwin` (universal2),
`win32-x64-msvc`. For example:

```sh
klava extensions install npm:@zatsepin/klava-pii-guardrail-linux-x64-gnu --trust
```

There is deliberately no per-extension meta package: `npm pack` (what the npm
installer runs) installs no dependencies and runs no postinstall, so a meta
package would hand klava a shim with no binary next to it. The platform is part
of the name for the same reason the CLI ships `@zatsepin/klava-linux-x64-gnu`.

`klava-compress` links ONNX Runtime statically, so the package stays one file.
That is not automatic: `ort-sys` asks `pkg-config` for `libonnxruntime` before
falling back to its own prebuilt download and accepts any version ≥ 1.22, so a
build machine that happens to have ONNX Runtime installed (Homebrew on macOS,
distro packages elsewhere) would produce a binary linked against a shared
library — broken once copied out of the package. The release scripts shadow that
`.pc` with a deliberately stale one and fail the build if a
`ldd`/`otool`/import-table check still shows a shared ONNX Runtime.

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

`source` is exactly what the user typed (an update may rewrite it when it drops
a pin). `version` is the **resolved** version the installer actually picked —
the crate version `cargo install` chose, or the npm `package.json` version —
which is what `extensions update` compares and `extensions list` shows.

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
`fail_closed`, `timeout_secs`, `notice`, and the `[setup]` and `[settings]`
tables; everything else is passed straight through in the request's `config`
field. The last two are **host-only**: klava strips them before handing the
config to the extension (see [Declaring settings](#declaring-settings)) — as it
does `setup`, which is already host-only.

```toml
enabled = true
fail_closed = false     # false: a failure is logged and the request continues
timeout_secs = 20       # default 20; the child is killed after this
threshold = 0.7         # your own key, opaque to klava

notice = "Costs money: each call bills a second model."   # shown on enable

[setup]                 # optional provisioning hook; see below
args = ["--fetch-model"]

[settings]              # what this extension accepts; see below
threshold = 0.7
```

`notice` is a warning shown, wrapped, by `klava extensions enable`,
`extensions install` and `extensions show`. Use it for the things a user must
know *after* installing: money, data leaving the machine, a destructive default.

### Declaring settings

A manifest may list the keys the extension accepts, with their defaults:

```toml
[settings]
enabled = false
threshold = 0.7
roles = ["tool"]
```

klava does not interpret these values. It uses the table to make
`klava extensions show <name>` useful — each declared key is printed with its
default and the value currently in effect — and strips `[settings]` before
sending the config, so declaring a key never changes what the extension
receives. Keeping the declared defaults equal to the extension's real defaults
is the extension's job; each shipped extension has a test that compares the
manifest against its `serde` defaults.

Without a `[settings]` table, `show` can only print what is currently set, and
says so. This is the case for every extension installed before the table
existed; `klava extensions refresh-manifest <name>` re-asks the binary for its
manifest and updates it in place (unlike `install`, which never overwrites an
existing manifest).

The easiest way to emit a manifest is `--print-manifest`: klava runs
`<binary> --print-manifest` once at install time and stores valid TOML beside
the artifact as `<name>.toml`.

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
  "nested_calls_used": 0,
  "require_decide": true
}
```

The first run has no `decide`. If the extension asks for one, the host re-runs
it with the answer added:

```json
{
  "protocol": 1,
  "config": { "enabled": true },
  "payload": { "model": "…", "messages": [] },
  "reason": "user",
  "source_api": "openai",
  "model": "…",
  "nested_calls_used": 0,
  "require_decide": true,
  "decide": {
    "answers": { "needs_help": { "type": "noul", "noul": 0.91 } },
    "approve": true,
    "probability": 0.91,
    "cost_usd": 0.000021,
    "model": "typesafe/jev-1.13"
  }
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
| `require_decide` | Whether the host will only accept a `nested_call` after an approved `decide`. |
| `decide` | Present only on a re-run after a `decide`; absent on the first run. |

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
| `decide` | Ask the host's decide model a typed question; the extension is re-run with the answer. | `decide` |
| `nested_call` | Ask klava to run a second model and fold the answer in. | `nested_call` |

An unknown `decision`, or `replace` without a `payload`, is treated as
`continue`. klava never blanks a request because an extension was confused.

`nested_call` shape:

```json
{
  "decision": "nested_call",
  "nested_call": {
    "model": "~anthropic/claude-opus-latest",
    "mode": "append",
    "system": "You are a senior reviewer.",
    "instruction": "Point out what is wrong.",
    "max_tokens": 2048
  }
}
```

`model` names a `[[models]]` entry in the user's config, which carries the
connection (type, URL, key) for the call. A name that is not configured falls
back to the user's default completion model, as does an omitted `model`.

`mode` is `append` (default; the answer is added as a user message and the
original model continues) or `take_over` (the answer is returned to the client
as the assistant turn). The extension never makes the call itself: credentials,
routing, timeouts and the recursion boundary stay in klava, so an extension
cannot recurse.

### Asking the decide model: `decision: "decide"`

klava owns the **decide model** (a System One classifier via `jigor`) and the
credentials it needs. Any extension can ask for a judgment without linking
klava, holding a key, or knowing which backend serves the model. The extension
owns the question and the evidence; klava owns the model, the timeout, the
retry policy and the cost accounting.

```json
{
  "decision": "decide",
  "decide": {
    "state": { "context": "…bounded evidence the classifier should judge…" },
    "questions": [
      { "id": "needs_stronger_model", "type": "noul", "instructions": "Is …?" }
    ],
    "threshold": 0.8,
    "approve_question": "needs_stronger_model"
  }
}
```

The host answers by **re-running the same extension** with `decide` filled in:

```json
{
  "decide": {
    "answers": { "needs_stronger_model": { "type": "noul", "noul": 0.91 } },
    "approve": true,
    "probability": 0.91,
    "cost_usd": 0.000021,
    "model": "typesafe/jev-1.13"
  }
}
```

| Field | Meaning |
|---|---|
| `state` | Any JSON value. The classifier judges it as text. |
| `questions` | Up to 16 typed questions: `noul` (yes/no), `choice` (one of `criteria` keys), `score` (an ordered `criteria` list). |
| `threshold` | `noul` probability at or above which `approve` is true. |
| `approve_question` | Question that decides `approve`. Defaults to the first `noul`. |
| `answers` | Every requested answer, in the System One wire shape. |
| `approve` | Whether the approve question cleared the threshold. |
| `cost_usd` | Billed cost of the classification call, when reported. |

Each extension gets **at most two decide rounds per client request**, so a
classifier loop cannot run away. A failed call (no key, network, timeout,
malformed answer) is logged and the extension is **not** re-run — treat a
missing `decide` as "no".

The request carries `require_decide`, the host's nested-call policy, so an
extension can align with it. The host's switch is authoritative:

```toml
# Require an approved decide before any nested_call (default: true).
require_decide_for_nested_calls = true

# One decision model for web-search tuning and every extension `decide`.
# It names exactly one [[models]] entry; the jigor backend (klava /
# openrouter / cloudflare) is detected from that entry's type and base URL.
decide_model = "typesafe/jev-1.13"
```

When `require_decide_for_nested_calls = true`, a `nested_call` with no approved
`decide` in the same request is refused and the turn stays on the cheap model.
When it is `false`, such calls still run but are logged as `UNVERIFIED`.

The gate fails **closed**: only an explicit `approve: true` allows the spend.
`klava-model-escalation` builds a bounded excerpt (a message window and a
character budget, never the whole payload) and asks one `noul` question, so the
cost of saying "no" is one small classification call — logged next to the
decision.

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
klava extensions show <name>            # status, notice, declared settings, effective config
klava extensions install <source> --trust
klava extensions install <source> --trust --no-setup   # enable, but do not provision
klava extensions update                 # reinstall from each recorded source
klava extensions update <name> --trust  # one extension; latest version
klava extensions update --check         # report what is available, build nothing
klava extensions setup <name>           # run the [setup] hook now (re-runs it)
klava extensions stats <name> [--json]  # run the binary's `--stats`, if it has one
klava extensions refresh-manifest <name>  # re-ask the binary for its manifest
klava extensions enable <name> [--trust|--no-setup]
klava extensions disable <name>
klava extensions set <name> key=value
klava extensions verify
klava extensions remove <name>
```

`update` (alias `upgrade`) skips extensions it does not manage, compares the
recorded version against the registry first, and drops version pins — see
[Update](#update). `set` parses values as JSON when it can, so `threshold=0.7`
and `roles=["tool"]` both work.

`stats` is a pass-through: klava runs `<binary> --stats` and prints whatever it
says. Whether an extension reports anything, and in what shape, is its own
business — klava interprets no outcome and counts nothing, so it has nothing to
invent here. Any extension may implement `--stats`, and one that does not simply
fails with its own usage text.

`install` adopts a manifest the extension prints: after a `cargo:`/`npm:`
install, klava runs `<binary> --print-manifest` once and stores valid TOML as
`<name>.toml` (an existing manifest is never overwritten). That is how a
package-manager install learns about a `[setup]` hook it could not carry.


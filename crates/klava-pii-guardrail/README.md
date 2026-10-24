# klava-pii-guardrail

PII redaction for [klava](https://github.com/Partysun/klava).

A standalone extension that detects personally identifiable information in the
prompt/tool output and replaces it before the request reaches the model. It
uses span-based detection with 21 recognizers and checksum validators (Luhn,
IBAN mod-97, SSN, ITIN, routing, IMEI), so it catches real entities rather than
plain substring matches.

klava launches this binary as a child process and speaks JSON over stdio; it is
never compiled into the klava binary. Installing needs no rebuild of klava.

## Install

```sh
cargo install klava-pii-guardrail
klava extensions install ~/.cargo/bin/klava-pii-guardrail --trust
```

No Rust toolchain? Install the prebuilt binary from npm instead — the package
name carries your platform:

```sh
klava extensions install npm:@zatsepin/klava-pii-guardrail-linux-x64-gnu --trust
klava extensions install npm:@zatsepin/klava-pii-guardrail-linux-arm64-gnu --trust
klava extensions install npm:@zatsepin/klava-pii-guardrail-darwin --trust
klava extensions install npm:@zatsepin/klava-pii-guardrail-win32-x64-msvc --trust
```

## Configuration

All keys live under `[extensions.settings.klava-pii-guardrail]` in the klava
config. `klava extensions show klava-pii-guardrail` prints them with their
defaults:

```toml
[extensions.settings.klava-pii-guardrail]
enabled = true                    # security floor: on by default
entities = ["Email", "CryptoAddress"]  # empty = the crate's full set
operators = {
    Email          = "replace:<EMAIL>",
    CryptoAddress  = "replace:<CryptoAddress>",
    CreditCard     = "mask:4",    # keep last 4 digits
    # Ssn = "hash:<salt>"          # deterministic hash
}
```

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Master switch: this setting is the only way to turn the guardrail off. |
| `entities` | `["Email", "CryptoAddress"]` | Which entity types to scan for. Empty = all 21 recognizers. |
| `operators` | map on right | How each entity is rewritten: `replace:<text>`, `mask:<keep-from-end>`, `hash:<salt>`, or nothing (plain `Redact`). |

Entity names: `Email`, `Phone`, `IpAddress`, `Ipv6`, `CreditCard`, `Iban`,
`Ssn`, `Itin`, `TaxId`, `Passport`, `DriverLicense`, `BankAccount`,
`RoutingNumber`, `CryptoAddress`, `MacAddress`, `Uuid`, `Vin`, `Imei`, `Url`,
`Domain`, `Hostname`.

## Safety

- Redaction walks JSON string **values**, never the serialized request, so
  structural escapes (`\n`, `\t`) can never be corrupted.
- Inline base64 payloads (`data:` URLs, Anthropic image sources) are masked
  before analysis, so a crypto-address-shaped run inside an image is untouched.
- Two failure layers, deliberately different:
  - **Inside a run**, an analysis error leaves the text unchanged rather than
    blanking a request the model could still use.
  - **At the host level**, the manifest declares `fail_closed = true`: if this
    process crashes, times out, or answers with malformed output, klava rejects
    the request instead of sending it unfiltered. A PII filter that silently
    stops filtering is worse than a visible failure.

  Opting out of the second layer (`fail_closed = false` in
  `~/.local/share/klava/extensions/klava-pii-guardrail.toml`) means an internal
  crash would let unredacted text through. Do it only if you would rather have
  the request go out than fail.
- Requests with more than 3 MiB of text are **refused** (`decision: "deny"`)
  rather than analyzed. klava gives an extension 20 s and caps its reply at
  8 MiB, so a multi-megabyte dump could only be half-processed before the host
  killed it. Forwarding it unanalyzed instead would let the payload through
  unchecked — the leak this extension exists to prevent — so the request is
  denied with a reason naming the size. Ordinary prompts and accumulated tool
  output fit comfortably below the limit.
- Redaction is one forward pass over the text. Spans are replaced in a single
  copy rather than by repeated in-place `replace_range` calls, so a payload with
  tens of thousands of matches costs milliseconds instead of minutes.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

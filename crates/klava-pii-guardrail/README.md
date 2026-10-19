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

## Configuration

All keys live under `[extensions.settings.klava-pii-guardrail]` in the klava
config:

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
| `enabled` | `true` | Master switch. klava also injects `disable_pii` when the legacy `enable_pii = false` flag is set. |
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
- Fails **open**: an analysis error leaves the text unchanged; it never blocks
  the request.

See [klava's EXTENSIONS.md](https://github.com/Partysun/klava/blob/master/EXTENSIONS.md)
for the wire protocol.

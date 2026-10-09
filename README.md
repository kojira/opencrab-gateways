# opencrab-gateways

Gateway processes for [OpenCrab](https://github.com/kojira/opencrab). Each gateway is a separate
process that connects to the OpenCrab core over the V3 Unix-socket protocol through
`opencrab-gate-client`. The core owns the protocol; this repository owns the platform runtime,
admission, delivery and credentials of each gateway.

| Crate | Binary | Role |
|---|---|---|
| `crates/discord-gateway` | `discord-gateway` | Discord bot runtime |
| `crates/nostr-gateway` | `nostr-gateway` | Nostr runtime (drives the `nostaro` CLI) |
| `crates/web-gateway` | `web-gateway` | Dashboard conversation HTTP/SSE gateway |
| `crates/cli-gateway` | `opencrab-cli-gateway` | Terminal REPL / JSONL gateway |
| `crates/process-supervisor` | — | Child-process supervision shared by the gateways |
| `crates/gateway-e2e` | — | End-to-end QC that runs real gateways against an in-process core |

`conformance/` holds the V3 process conformance fixtures, and `samples/node/` an independent Node
implementation of the web gateway that runs the same conformance suite.

## Core version

The core crates are git dependencies pinned to one `kojira/opencrab` rev in `Cargo.toml`.
`Cargo.lock` is the source of truth for the core the gateways were tested against:

```sh
python3 scripts/core-rev.py   # prints the locked core rev
```

Deploy gateways only together with a core built from that rev. To move to a newer core, update the
`rev` in `Cargo.toml`, run `cargo update -p opencrab-gate-client`, and let CI pass.

## Build and test

```sh
cargo build --release -p opencrab-discord-gateway -p opencrab-nostr-gateway -p opencrab-web-gateway
cargo clippy --workspace --all-targets --all-features -- -D warnings

# web-gateway's real-process e2e starts the core server built from the locked rev.
OPENCRAB_SERVER_BIN=/path/to/opencrab-server cargo test --workspace --all-features
```

CI checks out the locked core rev, builds `opencrab-server` from it, and runs the tests and the
conformance suite against both the Rust and the Node implementation.

## License

MIT

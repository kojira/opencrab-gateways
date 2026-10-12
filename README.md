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

## Discord placement: attachments

The Discord daemon config and each child placement accept two optional absolute paths:

| Key | Meaning |
|---|---|
| `attachment_spool_root` | Core-provided inbox where the gateway downloads inbound Discord attachments |
| `attachment_outbox_root` | Core-owned outbox of workspace files the agent attaches to a reply. Use the core's `<db_dir>/attachments/outbox` |

When `attachment_outbox_root` is set, `reply` declares an optional `files` argument
(`"format": "workspace-file"`). The core copies each workspace file into the outbox and passes a
descriptor; the gateway re-verifies size and sha256 and attaches the files to the first chunk of
the reply (at most 10, each at most 500 MiB; Discord's own smaller limit is reported as a
rejection). A malformed descriptor, an unreadable or mismatched copy, more than 10 files, a file
over 500 MiB, or `files` without a configured outbox is rejected without sending anything. All
files are read and verified before the reply is sent, because the core deletes the outbox copies
once the gateway answers. Without the key, `reply` does not declare `files`.

## Nostr attachment upload

Nostr attachments use `POST https://share.yabu.me/api/v2/media` with multipart
`no_transform=true` and NIP-98 authorization over the exact request body. The
unversioned `/upload` serves a legacy response; Blossom PUT can transform images.
The NIP-94 original/output hashes and size must match the original file. Before
publishing its URL, the gateway downloads it once and verifies the original bytes'
SHA-256 and size. Pending, unavailable, or converted content is indeterminate;
the gateway does not retry the upload or poll for completion.

## License

MIT

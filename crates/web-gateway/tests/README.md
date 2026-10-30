# tests

| 対象 | コマンド | 算入 |
|---|---|---|
| process conformance | `cargo test -p opencrab-web-gateway --test conformance` | 共通合格数。fixture は `conformance/fixtures/` |
| rust-unit | `cargo test -p opencrab-web-gateway --test rust-unit` | 算入しない。conformance fixture に無いもの（会話の作成・状態、frame の文法）だけ |
| core_process_e2e | `cargo test -p opencrab-web-gateway --test core_process_e2e` | 実 core 結合 |
| web_conversation_create_e2e | `cargo test -p opencrab-web-gateway --test web_conversation_create_e2e` | 実 core 結合 |

conformance harness は `InstanceClient` / `router()` / `parse_frame_bytes` を import しない。SUT は `OPENCRAB_CONFORMANCE_SUT`（未設定時は `CARGO_BIN_EXE_web-gateway`）。fixture は `conformance/fixtures/` のディレクトリ走査1本。言語名分岐は置かない。merge CI は Rust 列と Node 列を同じ fixture で走らせ、`scripts/check-samples-node.sh` を必須にする。

# process conformance

DESIGN-SAMPLES-NODE.md §1 / §5。SUT は `argv[1]=placement.json` の executable。既定は Rust `web-gateway`。差し替えは `OPENCRAB_CONFORMANCE_SUT` だけ。harness は一時 UDS の mock core と実 TCP の HTTP/SSE だけで駆動する。言語名分岐・skip・片側 golden は置かない。

merge CI（`.github/workflows/ci.yml` の `conformance` job）は Rust SUT 列と Node SUT 列を同じ fixture で走らせ、`scripts/check-samples-node.sh` で runtime dependency 0 / Rust import 0 / Bearer 0 / 旧 route 実装 0 を static audit する。Node の exact version は `samples/node/.node-version`。

## 起動 ABI

placement は `{http_bind,core_socket,conversations:[{session,instance_id}],instances:[{instance_id,agent_id,author_id}]}`。harness は fixture の会話（`ids.address`）を `conversations` で渡す。ready は `http_bind` への TCP listen 成立だけ。stdout/stderr は見ない。

## fixture

`fixtures/` の JSON をディレクトリ走査で全件読み、全 SUT に同じ手順・同じ正規化期待値で適用する。SUT 名分岐、実装別 skip、片側 golden は置かない。

共通範囲: gateway 側 framing（1,048,576 成功 / LF 未着超過 / invalid UTF-8 / 非 object / duplicate / LF 込み超過）/ hello / said / dedup / say 3 結果（受理 / external_rejected / indeterminate）/ live queue overflow(32) / activity / disconnect / HTTP / SSE。再接続は production spawn 経路。D-1099 で bind は無くなった（会話の所有は gateway の記録）。

## 合格数

`cargo test -p opencrab-web-gateway --test conformance` の fixture 件数だけが共通 conformance の合格数。`rust-unit` と lib unit は算入しない。

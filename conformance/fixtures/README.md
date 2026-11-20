# fixtures

言語非依存の mock-core 手順と正規化期待値。Rust / Node が同じファイルを読む。

## ファイル

| ファイル | 範囲 |
|---|---|
| `ids.json` | 共有識別子。placement と prelude の源 |
| `hello-said-dedup.json` | hello / said / 同 origin 再送 |
| `say-three-results.json` | say 受理 / empty text の external_rejected / 第3結果（ok/err 捏造なしで SUT close。時間則は見ない） |
| `activity.json` | activity started/ended と completed_no_reply |
| `activity-origin.json` | activity started に additive `origin` を載せても従来どおり started/ended を配送（subset 規約＝追加 field 無視の実証・R2） |
| `turn-failed-keeps-connection.json` | id 無し `turn_failed` 通知を送っても SUT は gate→core write 0・接続維持・後続 said を継続処理（未知/新通知を無視せよの契約・R3） |
| `partial-ignored-keeps-connection.json` | id 無し `partial` 通知（生成途中の本文）を送っても SUT は gate→core write 0・接続維持・後続 said を継続処理（未知/新通知を無視せよの契約） |
| `frame-too-large.json` | LF 込み 1,048,577 byte で close |
| `frame-exact-1mib.json` | LF 込み 1,048,576 byte 丁度の成功 |
| `frame-no-lf-overflow.json` | LF 未着のまま上限超過で close |
| `frame-invalid-utf8.json` | 妥当な JSON 文字列中の invalid UTF-8 で close |
| `frame-non-object.json` | 非 object JSON で close |
| `frame-duplicate.json` | duplicate member で close |
| `live-queue-overflow.json` | live queue 容量 32 超過で external_rejected |
| `http-post-and-routes.json` | `ok.seq=null` の 403 not_admitted / 202 / turn 実行中の別 UUID も 202 / 旧 route 404 |
| `disconnect-and-unknown-conversation.json` | gateway の記録に無い会話は 404、切断後は SSE `gate_error` と POST 503 `disconnect` |
| `reconnect.json` | production spawn 経路の再 hello（D-1099 で bind は無い） |

## スキーマ

```text
{
  name: string,
  prelude: "none" | "hello",   // D-1099: bind は無い
  steps: Step[]
}

Step =
  {op:"http_post_async", id, path, body}
| {op:"http_await", id, status, body?}
| {op:"http_post", path, body, status, body_expect?}
| {op:"http_get", path, status}
| {op:"uds_recv", id?, expect}
| {op:"uds_send", frame}
| {op:"uds_send_raw", utf8}
| {op:"uds_send_hex", hex}
| {op:"uds_send_padded_say", id, binding_id, frame_size}
| {op:"uds_send_says", count, id_prefix, binding_id, expect_ok?}
| {op:"uds_send_oversized", byte, count, nl}
| {op:"uds_idle", ms?}
| {op:"uds_close"}
| {op:"uds_unlisten"}
| {op:"uds_accept"}
| {op:"sse_open", id, path}
| {op:"sse_recv", id, event, data?}
```

文字列 `$name.field` は直前までの capture を参照する。期待値は既知 field の subset。未知 field と member 順は見ない。

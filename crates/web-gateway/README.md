# web-gateway

Web 会話の独立 binary。HTTP/SSE を V3 protocol=3（UDS）へ変換するだけ。判断しない。Bearer を持たない。core crate の wire DTO に依存しない。

## 入口

| method / path | 役割 |
|---|---|
| `POST /api/web-conversations` | `{agent_id,name}`からgatewayがsession key（`extgate-<uuid>`）を採番し、対象instanceへ`open`を送る。core が返した session を gateway の会話記録に残し、`201 ready`。接続前は`503 instance_not_ready` |
| `GET /api/web-conversations/{session_id}` | Web gateway自身の会話記録で所有と状態（`ready` / `unavailable`）を返す。非所有は404 |
| `POST /api/web-conversations/{session_id}/messages` | body exact `{client_message_id,text,attachments}` → V3 `said`。`202` は said ack まで |
| `GET /api/web-conversations/{session_id}/events` | say / activity / `completed_no_reply` の SSE |
| `GET\|POST /rooms/{room}/messages` | 404。alias しない |
| `GET /chat` | 404。redirect しない |

`session_id`はgatewayが作ったsession key（core はこれを session として扱う・D-1099）。どの instance の会話かは gateway の会話記録（`conversations_path`）が決める。記録に無い session は`404`、所有 instance が切断中は`503`。turn 実行中の別 UUID は said を送り、core の session queue 32 が直列化する。キュー満杯は `ok.seq=null` → `403 {state:"not_admitted"}`。`409 conversation_busy` は `PostRefuse::Busy` のときだけ（キュー満杯を client が Busy に写したとき）。`ok.seq=null` は `403 {state:"not_admitted"}`。said 応答は 10 秒で打ち切り、`disconnect` 相当で pending を落とす。wire close の SSE は `event: gate_error`（ブラウザ予約の `error` とぶつからない）。

UDS 切断後も HTTP listen は落とさない。指数 backoff（200ms…8s）で再接続し、hello を送り直す（core への登録は無い）。切断中の message POST は `503 disconnect`、events は SSE `gate_error`（code=`disconnect`）。tracing filter は `opencrab_web_gateway` / `opencrab_gate_client` と binary 名 `web_gateway`。hello / said / say / close を info に残す。V3 client / wire / json は `opencrab-gate-client` を使う。

## 配置

operator が書いた placement JSON を argv で渡す。

```text
web-gateway /path/to/placement.json
```

`http_bind` は loopback のみ。`core_socket` は絶対 path。instanceごとに`agent_id`とUDSを1本ずつ定義し、同じ`agent_id`の重複は拒否する。hello は `instance_id` と `agent_id` を名乗るだけ（core が instance を見つけるか作る）。

`conversations_path`（絶対 path・任意）はこの gateway の会話記録（session key → instance）。無ければメモリだけで、再起動で会話の所有を失う。`conversations: [{session, instance_id}]`（任意）は起動時に記録へ取り込む既存の会話（既に記録済みのものは変えない）。

## 検収

共通 process conformance（SUT は `OPENCRAB_CONFORMANCE_SUT`、未設定時は `web-gateway` binary。`argv[1]=placement.json`。ready は `http_bind` listen のみ）:

`cargo test -p opencrab-web-gateway --test conformance`

fixture はリポジトリ直下 `conformance/fixtures/`。SUT 名分岐・実装別 skip・片側 golden は置かない。

frame parser と in-process HTTP は rust-unit に分離し、共通 conformance の合格数に算入しない:

`cargo test -p opencrab-web-gateway --test rust-unit`

実 core 結合: `cargo test -p opencrab-web-gateway --test core_process_e2e --test web_conversation_create_e2e`

## 関連

- 本設計: `DESIGN-WEBGATE.md`
- 線の契約: `DESIGN-EXTGATE-V3.md`（変更しない）

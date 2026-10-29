# nostr-gateway

Nostr ingress／delivery の独立 binary。`nostaro watch` 子プロセスの JSONL を V3 `said` へ写し、coreからの`reply`／`say`をNostrへ投稿する。Bearer を持たない。core crate の wire DTO に依存しない。

## 起動 ABI

```text
nostr-gateway daemon /path/to/daemon.json
nostr-gateway instance /path/to/placement.json
nostr-gateway settings /path/to/daemon.json 0.0.0.0:8794
```

`daemon` はgateway専用SQLite、ローカルadmin UDS、instance childのreconcileを所有する。`instance` はplacementに従ってcore UDSへ接続し、watch childを起動する。`daemon` と `instance` は HTTP listen と server proxy を作らない（設定画面は別プロセスの `settings` が引数の bind で listen する）。

```json
{
  "database_path": "/var/lib/opencrab-nostr/gateway.db",
  "core_database_path": "/var/lib/opencrab/opencrab.db",
  "legacy_database_path": "/var/lib/opencrab/opencrab.db",
  "admin_socket": "/run/opencrab/nostr-admin.sock",
  "core_socket": "/run/opencrab/gate.sock",
  "nostaro_bin": "/usr/local/bin/nostaro",
  "placement_dir": "/var/lib/opencrab-nostr/placements"
}
```

`legacy_database_path`は一回限りのimport元であり、完了marker後のruntime読取には使わない。admin UDSは0600で、秘密鍵を応答へ含めない。`database_path`と`core_database_path`を同一pathにしてはならない。

`NOSTARO_SECRET_KEY` は起動時に process env から除去し、watch child env にだけ渡す。argv・log・status・config に出さない。

## placement

`http_bind` は置かない。`core_socket` は絶対 path。`nostaro_bin` は nonempty。instance ごとに UDS 1 本。`config_b64` の decode バイトが hello `config_digest` の源。canonical config は relays / filter / self_pubkey / `name` / watches と optional `delivery_mode`。`name` は必須（空なら fail-loud）。秘密を含めない。

default(メンション)車線は常設。watch は追加車線。同じ `address` の binding 1 枚を共有する。
メンション車線の nostaro argv は `--match=any` に加え、instance config の `name` を `--keyword`、`self_pubkey` を `--npub`（p タグ対象）として付ける。kind は 1 と 7。hex pubkey の keyword は付けない（本文 substring にならない）。`--no-mention-only` は付けない。条件ゼロの空網にはしない。この車線の said は `route=immediate`（束ね待ちしない）。`beyond_self` は false。
watch child は bind ack の後にだけ起動する。切断で child を止め、読取済み未送信は破棄して再送しない。
`SaidOutcome` は Accepted / NotAdmitted / Disconnected / WireErr を記録し、`store_error` / `bad_request` はカウンタに残す。turn 実行中の said は core の session queue 32 に積む。`PostRefuse::Busy`（`said refused; binding busy`）はキュー満杯の応答にだけ使う。
watch の有効フィルタは `filter_json`。アンカー `beyond_self` は watch 設定値（`!p_self` の代用はしない）。
タイムライン車線の Bundle は `interval_secs` ごとに flush する。`max_items` は 1 周期の上限件数（省略時 50）。超過分は新しい方から `max_items` 件を残し、古い分は捨てる。捨てた件数は warn と `bundle_discarded` カウンタに残す（会話本文には出さない）。manifest / `bundle_id` / `count` / `[NOSTRBUNDLE/V1 …]` は残した分だけ。

### watch の正本と設定（D-1092）

- watch の正本は instance ごとの保存ファイル `nostaro-watches-<instance_id>.json`（core socket と同じディレクトリ・mode 0600）。core の DB には持たない。
- 保存ファイルが無い初回起動だけ、配置 `config_b64` の `watches` を種にして書き出す（1 度だけの移行。以後は配置の `watches` を読まない）。
- エージェントは宣言能力 `watch_list` / `watch_set` / `watch_delete` で読み書きする（owner のみ。`watch_list` は read_only、ほかは state_change）。範囲（interval 60〜86400 秒・authors 最大 2000・keywords 最大 32・kinds は 1/6/7）を外れた値は状態を変えずに拒否する。
- 変更は保存と同時に watch 車線だけを作り直して即時に効く（say consumer とフォロー反映は止めない）。ファイルが外から書き換えられた場合（設定画面・手編集）は 2 秒ごとの再読込で反映する。
- 設定画面: 下の「設定画面（D-1097）」の watch 欄。検証は能力と同じ。
- 間隔は実行中に変わりうるので、車線をまたぐ dedup の TTL は許される最大間隔（86400 秒）の 2 倍＋余裕で取る。

## 写像

- origin: `nostr:event:v1:{lane}:{event_id}`。`lane` は `default` または `watch:{id}`
- **車線をまたぐ dedup**: 1 event_id は最初に握った車線だけが `said` する（instance 単位の TTL 付き seen セット）。あるイベントがメンション条件と watch 条件の両方に当たると default/watch の両車線が同じ event_id を吐くが、origin は車線名を含むため下流 gate では別メッセージになり会話重複・二重ターンを生む——そこを埋める。即時送出は送る直前に握り、bundle は flush の window 確定後・manifest 構築の前に握って落とす（`bundle_id`/`count`/`origins` を実送信分と一致させる）。gate の per-origin dedup は同一車線の再購読を潰すのでここは車線間だけを見る。TTL は最大 watch `interval_secs` の 2 倍以上（下限 600s）で、即時送出したメンションと interval 後に flush される bundle の窓を跨ぐ。落とした件数は `deduped` カウンタに残す
- 本文先頭: 版付きアンカー `[NOSTRGATE/V1 {…}]`（key 順固定）
- Bundle の次行: `[NOSTRBUNDLE/V1 [origin…]]`（index 順。coordinator が最初の非重複 member で全 origin を照合する）。flush の各 member は `post_said_receipt`（Accepted 後も次 origin を送る）
- その下: 採取 §3 の履歴本文 renderer
- kind 4/1059 が現れたら `route=immediate`（Discard は core）
- inbound originを引き継ぐ`say`はそのeventへの`reply`として投稿する
- reply targetのないheartbeat／bundle由来の`say`はstandalone postとして投稿する

## 検収

`cargo test -p opencrab-nostr-gateway`

## 関連

- 本設計: `DESIGN-NOSTRGATE.md`
- 線の契約: `DESIGN-EXTGATE-V3.md`
- V3 client: `opencrab-gate-client`

## 設定画面（D-1097）

`nostr-gateway settings <daemon.json> <bind>` を別プロセスで起動する（本番は `0.0.0.0:8794`、Tailscale から開く）。状態を持たず、daemon の admin socket（mode 0600）を通して読み書きする。admin scope の全 instance を列挙し、暗黙に 1 つを選ばない。

| HTTP | 内容 | admin op |
|---|---|---|
| `GET /` | 画面 | – |
| `GET /api/instances` | 全 instance の設定とアクセス（秘密なし） | `list` + `get` |
| `POST /api/instances/{id}/config` | `{relays, name, filter}`。`self_pubkey` は保持 | `set_config` |
| `POST /api/instances/{id}/access` | `{owner, trusted_users, followees, co_agents}`。hex / npub / nprofile を hex に正規化 | `set_access` |
| `POST /api/instances/{id}/key` | `{nsec}`（書き込み専用。`self_pubkey` を同じ世代で更新） | `set_key` |
| `POST /api/instances/{id}/generate-key` | `{prefix, overwrite: true}`。`nostaro vanity --json` で生成し、停止状態で保存 | `generate_key` |
| `POST /api/instances/{id}/enabled` | `{enabled}` | `set_enabled` |
| `GET/POST /api/instances/{id}/watches`、`POST …/watches/delete` | instance ごとの `nostaro-watches-<id>.json`。初回は instance の config の watches を種にする（child と同じ） | – |

- 設定の変更は desired generation を上げて `pending` に戻すので、daemon の reconcile（既定 1 秒）が子を作り直す。どのプロセスも手で再起動しない。
- 秘密（nsec）は応答・ログ・URL に出さない。表示は「設定済み／未設定」だけ。
- 範囲外の instance は `scope_denied`（HTTP 404）。不正な値は状態を変えずに 400。

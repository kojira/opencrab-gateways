# src

| ファイル | 役割 |
|---|---|
| `main.rs` | `daemon <daemon.json>` / `instance <placement.json>` / `settings <daemon.json> <bind>`。instance は HTTP listen しない。鍵を env から除去して child へ |
| `config.rs` | placement と instance canonical config。秘密なし。メンション車線は `mention_lane_filter`（keyword=`name`、`--npub`=`self_pubkey`、kind 1+7。hex は keyword にしない。空 name は fail-loud）。watch の `max_items` は Bundle 上限（省略時 50、0 以下は fail-loud） |
| `map.rs` | JSONL → said。origin `nostr:event:v1:{lane}:{event_id}` と版付きアンカー。default lane は `route=immediate` |
| `watch.rs` | nostaro watch spawn。EOF 再購読 5s。鍵は child env のみ。メンション車線 argv は `plan_mention_lane_args`（名前 keyword と `--npub` の両方） |
| `secret.rs` | `NOSTARO_SECRET_KEY` の take + remove_var |
| `run.rs` | UDS client。core 接続が成り立ってから lane 起動（D-1099 で bind は無い）。メンション車線は常設、watch は追加。SaidOutcome を観測。Bundle flush は interval と `max_items` を同居。超過は新しい方を残し `bundle_discarded` を加算 |

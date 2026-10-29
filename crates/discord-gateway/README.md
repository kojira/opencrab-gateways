# discord-gateway

Discord の独立 binary。serenity で受けたメッセージを V3 `said` として core へ渡し、core の `say` と operation を Discord に投稿する。設定はこの gateway が自分の DB に持つ。core が Discord instance について持つのは、固定の中立な config（`{"delivery_mode":"tool_driven"}`）だけ。

## 起動 ABI

```text
discord-gateway daemon   <daemon-config.json>            # 本番: 全 bot を所有し子を起動する
discord-gateway settings <daemon-config.json> <host:port> # 設定画面（本番は 0.0.0.0:8793）
discord-gateway instance <placement.json>                # 子（daemon が起動する。手で使わない）
discord-gateway voice-settings <settings.json> <port>    # 旧 VC 設定画面（settings に統合済み）
```

- `daemon` は gateway の SQLite、admin UDS（mode 0600）、子プロセスの reconcile を持つ。HTTP は listen しない。
- `settings` は daemon の admin UDS だけを通して読み書きする別プロセス。鍵も bot トークンも持たない。
- `instance` は daemon が `placement_dir` に書いた配置で起動する。bot トークンは env `DISCORD_BOT_TOKEN` で渡され、起動直後に env から消える。

## daemon の設定

全パスは絶対パス。知らないキーがあると起動しない。

```json
{
  "database_path": "/srv/opencrab/data/discord-gateway.db",
  "admin_socket": "/srv/opencrab/data/run/discord-admin.sock",
  "core_socket": "/srv/opencrab/data/run/gate.sock",
  "child_binary": "/srv/releases/opencrab-<tag>/bin/discord-gateway",
  "placement_dir": "/tmp/opencrab-prod-discord-<tag>",
  "attachment_spool_root": "/srv/opencrab/data/attachments/inbox",
  "attachment_outbox_root": "/srv/opencrab/data/attachments/outbox",
  "voice_settings": "/srv/opencrab/data/gate/discord/voice/settings.json",
  "core_api": "http://127.0.0.1:8080",
  "legacy_import": {
    "placements": [
      "/srv/opencrab/data/gate/discord/<agent 1>.json",
      "/srv/opencrab/data/gate/discord/<agent 2>.json",
      "/srv/opencrab/data/gate/discord/<agent 3>.json"
    ],
    "token_dir": "/srv/opencrab/data/secrets/discord"
  },
  "reconcile_millis": 1000
}
```

| キー | 必須 | 意味 |
|---|---|---|
| `database_path` | ○ | gateway の DB。instance の設定・暗号化した bot トークン・状態。隣に `.lock` を作る |
| `admin_socket` | ○ | 設定用 UDS（mode 0600）。`settings` プロセスがここに接続する |
| `core_socket` | ○ | core の gate UDS。daemon が instance を provision し、子が接続する |
| `child_binary` | ○ | 子として起動する `discord-gateway` |
| `placement_dir` | ○ | 子の配置・制御ソケット・チャンネル要求ソケットを置く場所。release ごとに分ける |
| `attachment_spool_root` / `attachment_outbox_root` | | 受信添付の置き場 / 送信添付の置き場（core の workspace） |
| `voice_settings` | | VC 設定ファイル。省略時は `database_path` と同じディレクトリの `voice/settings.json` |
| `core_api` | | core の HTTP（エージェント一覧の取得にだけ使う）。無いと設定画面の「新しい bot」が使えない |
| `legacy_import` | | 初回だけ取り込む instance モードの配置とトークン（後述） |
| `reconcile_millis` | | reconcile の間隔。既定 1000 |

env:

- `OPENCRAB_DISCORD_MASTER_KEY`（必須）: DB 内の bot トークンを暗号化する鍵。32 バイトを base64 にしたもの。起動直後に env から消す。

## core とのやりとり

core とのやりとりは `src/core_provisioning.rs` の 1 か所（`ProvisionReconciler`）にまとまっている。それ以外の部分は trait `GateReconciler` しか見ない。core のクレデンシャルは要らない。

1. daemon はその bot の子を止める。
2. core の gate UDS に新しい接続を張り、`hello` の前に `provision` を 1 回送る。中身は instance id、kind `discord`、agent、`enabled`、固定の中立 config、DB にあるチャンネル全部（binding id は `uuid5(instance_id, address)`）。
3. core が返した revision と config digest を DB に記録し、子を起動する。子はその revision で `hello` する。

送ったチャンネルの一覧が core 側の正になる。一覧に無い binding は core が閉じる。

新しい bot（設定画面の「新しい bot」と admin の `create`）は、新しい UUID v4 の instance id で作る。エージェント 1 つにつき bot は 1 つ。

## 初回の取り込み（legacy_import）

DB が空のときだけ、次の手順で取り込む。1 件でも問題があれば何も書かない。

1. `placements` の各配置を検証する。instance は 1 つだけ。`run_env` を持つ配置は取り込まない。
2. 各 bot について、`<token_dir>/<agent_id>.token` を読む。通常ファイル・mode 0600・空でないことが条件。
3. 全行を 1 つのトランザクションで書く。トークンは暗号化する。instance id は配置にあるものをそのまま使う。

core には何も問い合わせない。最初の reconcile がそのままの id で provision する。問題（配置の不備、トークンの不備）があれば daemon は起動しない。

取り込んだあとは、配置ファイルを二度と読まない。設定は DB が正になる。

## 設定画面

```text
discord-gateway settings <daemon-config.json> 0.0.0.0:8793
```

- bot ごとのタブで、次の設定を変えられる。
  - 起動・停止
  - bot トークンの差し替え（Discord に確認して bot ID も合わせる。表示はしない）
  - 表示名・オーナー・信頼済みユーザー・co-agent
  - 配送モード・リアクション
  - チャンネルの追加と一覧
- 全エージェント共通の VC 設定タブがある（`/voice/`）。
- 保存すると、daemon がその bot の子だけを作り直す。数秒で反映され、ほかの bot は止まらない。
- 秘密（bot トークン）は画面にもログにも出さない。
- 認証は無い。Tailscale の内側に置く前提。

## オーナーのメンションでチャンネルを有効にする

bot がまだ binding を持っていないチャンネルで、オーナー（`access.owners`）が bot を @メンションすると、次の順に動く。

1. 子は、自分の core 接続で `create_binding` を送る。binding id は `uuid5(instance_id, address)`。
2. core の bind を待つ。
3. そのチャンネルの say consumer を起動する。返事・typing・リアクション・activity がそのチャンネルに出る。
4. メンションしたそのメッセージを、通常の said として渡す。
5. daemon に自分専用のソケットで知らせる。daemon はアドレスを DB に記録するので、再起動しても残る。

オーナー以外のメンション、メンションの無いオーナーのメッセージでは、何も起きない。チャンネルが 1 つも無い bot（daemon モード）も起動できるので、最初のチャンネルはこの方法で有効にすればよい。

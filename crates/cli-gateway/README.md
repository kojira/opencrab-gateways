# opencrab-cli-gateway

A standalone, text-only terminal gateway. It connects to the core's gate socket (protocol 3) through `opencrab-gate-client` and talks to one agent in one core session per process. It does not read the database and does not handle attachments.

## Placement

```json
{
  "core_socket": "/absolute/path/to/gate.sock",
  "instances": [{
    "instance_id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
    "agent_id": "agent-a",
    "author_id": "local-operator"
  }]
}
```

- `core_socket` must be an absolute path. `instance_id` must be a canonical lowercase UUID. `agent_id` and `author_id` must be nonempty.
- `instance_id` and `agent_id` must each be unique in the file. Unknown members are rejected.
- Nothing is registered in core beforehand: the gateway says `hello` with its instance and agent. The placement contains no credential.

## Run

```text
opencrab-cli-gateway --placement FILE --agent ID (--new NAME | --session KEY) [--mode auto|repl|jsonl] [--connect-timeout-secs N]
```

`--agent` selects the placement entry with exactly that `agent_id`. `--connect-timeout-secs` (default 10) bounds the wait until the session is ready.

Open a new core session. The gateway sends `open` with a fresh `extgate-<uuid>` session key and NAME as the title, and prints the session in the first line (`Connected: <agent> / <session>` in REPL, a `ready` event in JSONL). Reuse that key with `--session`:

```sh
opencrab-cli-gateway --placement cli-placement.json --agent agent-a --new "Terminal chat" --mode repl
```

Continue an existing session key:

```sh
opencrab-cli-gateway --placement cli-placement.json --agent agent-a \
  --session extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb --mode repl
```

`--mode auto` (the default) selects REPL only when stdin and stdout are terminals, JSONL otherwise; `--mode repl` refuses to start without both terminals. REPL commands are `:help`, `:status`, `:quit`, and `::text` (sends `:text`). One nonempty line is one message.

JSONL mode keeps stdout protocol-only; diagnostics go to stderr:

```sh
printf '%s\n' \
  '{"type":"message","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","text":"hello"}' \
  '{"type":"shutdown"}' |
  opencrab-cli-gateway --placement cli-placement.json --agent agent-a \
    --session extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb --mode jsonl
```

Each message origin is `cli:<id>`. Retry an uncertain send with the same UUID; the gateway never replays input by itself after a disconnect. JSONL records are strict objects, reject unknown members and attachments, and are limited to 1,048,576 bytes including LF.

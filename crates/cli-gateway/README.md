# opencrab-cli-gateway

A standalone, text-only terminal gateway over the generic V3 Unix-socket protocol. It owns one exact agent and one core-owned session per process. It does not read the database, provision instances, or handle attachments.

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

Core needs nothing registered first: the gateway says hello with its instance and agent. The placement contains no credential.

## Run

Open a new core session and print its reusable `extgate-<uuid>` session key:

```sh
opencrab-cli-gateway --placement cli-placement.json --agent agent-a --new "Terminal chat" --mode repl
```

Attach to that exact session key:

```sh
opencrab-cli-gateway --placement cli-placement.json --agent agent-a \
  --session extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb --mode repl
```

`--mode auto` selects REPL only when stdin and stdout are terminals. REPL commands are `:help`, `:status`, `:quit`, and `::text` (send `:text`).

JSONL mode keeps stdout protocol-only; diagnostics go to stderr:

```sh
printf '%s\n' \
  '{"type":"message","id":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb","text":"hello"}' \
  '{"type":"shutdown"}' |
  opencrab-cli-gateway --placement cli-placement.json --agent agent-a \
    --session extgate-bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb --mode jsonl
```

Each message origin is deterministically `cli:<id>`. Retry an uncertain send with the same UUID; the gateway never automatically replays input after disconnect. JSONL records are strict objects, reject unknown members and attachments, and are limited to 1,048,576 bytes including LF.

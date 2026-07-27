# Contract: opencode server runtime

OpenCode server use is automatic and loopback only. The fixed candidates are `127.0.0.1:4097`, then `127.0.0.1:4098`. SQLite is read only compatibility enumeration only when no secure server is available. It is not job authority.

## Lifecycle and credentials

The viewer never stops or restarts a server. Spawn is the only operation that may start one. It starts `opencode serve --hostname 127.0.0.1 --port <port>` from the user home directory, preserving the normal environment and overriding only `OPENCODE_SERVER_USERNAME` and `OPENCODE_SERVER_PASSWORD`. Task shells receive neither credential.

Basic authentication uses nonempty `OPENCODE_SERVER_PASSWORD` and optional nonempty `OPENCODE_SERVER_USERNAME` overrides, or a generated stable secret in owner only credential files. SQLite holds only viewer presentation state. The preexisting unused `opencode.server_url` setting remains unchanged.

An occupied candidate is accepted only when it is the exact pinned OpenCode server process and requires authentication. A listener that returns `200` to unauthenticated `GET /global/health` is rejected as insecure. It is never stopped or restarted. Spawn may then use `4098` when that port is free.

Runtime state contains a generation, the pinned identity, health, and the managed session ids. The identity includes pid, start time, listener inode, effective uid, and exact argv. Process shared ownership uses only `flock`; each viewer process serializes its own work locally.

## Request authentication

Before every credential bearing request, the viewer connects without writing and verifies that the listener owner is the exact pinned process. It sends unauthenticated `GET /global/health` with keep alive and requires `401` while retaining a reusable connection. It resolves the reverse accepted connection inode for that exact local ephemeral tuple and verifies the same pinned pid, start time, effective uid, exact argv, and generation. It repeats this validation after the test hook, then sends the authorized request on that same `TcpStream` with `Connection: close`. It never reconnects for authorization.

The client is bounded HTTP/1.1 over `TcpStream`. It accepts strict content framing, including bodyless `204`, has bounded headers, body, and timeouts, and follows no redirects.

## Enumeration and management

Global enumeration is `GET /experimental/session?limit=10000&archived=true`, with `X-Next-Cursor` pagination. Repeated or malformed cursors, or a full page with no cursor, are errors.

The exact managed marker is this permission rule:

```json
{"permission":"agent-viewer.background","pattern":"*","action":"allow"}
```

A metadata marker is invalid. Only exact marked rows are managed. Only they receive `daemon_hosted`, live status, pending input, managed capabilities, and server mutations. The managed id cache includes archived marked rows so archive and unarchive remain available. Archived marked rows are not status polled.

For each unique active managed directory, the viewer fetches status, permission, and question once. A failure marks every row in that directory, including external rows, `Unknown`. Otherwise external server enumerated rows use compatibility `Idle` status.

Server mutations apply only to exact managed rows. Managed attach is refused because it would expose credentials. External rows run `opencode -s <session_id>`. External deletion remains local `opencode session delete <id>`.

## Endpoints used

| Call | Use |
| --- | --- |
| `GET /global/health` | unauthenticated authentication challenge and authenticated health |
| `GET /experimental/session?limit=10000&archived=true` | global enumeration |
| `GET /session/status?directory=...` | one active managed directory status |
| `GET /permission?directory=...` | one active managed directory permissions |
| `GET /question?directory=...` | one active managed directory questions |
| `POST /session?directory=...` | managed spawn |
| `POST /session/{id}/prompt_async?directory=...` | managed prompt, `204` expected |
| `PATCH /session/{id}` | managed rename and archive |
| `POST /session/{id}/abort` | managed stop |
| `DELETE /session/{id}` | managed delete |

PR association is unsupported.

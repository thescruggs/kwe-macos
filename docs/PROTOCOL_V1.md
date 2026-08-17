# Alpha local protocol v1

The manager talks to `kwe-daemon` over a user-owned Unix stream socket. Each
connection carries exactly one UTF-8 JSON request and response, each terminated
by a newline.

Request:

```json
{"version":1,"id":"caller-value","method":"health"}
```

Response:

```json
{"version":1,"id":"caller-value","ok":true,"result":{"status":"ready"}}
```

Methods are `health`, `catalog`, and `rescan`. Requests are capped at 64 KiB,
responses at the manager boundary at 32 MiB, and read/write deadlines are five
seconds. Unknown versions and methods return structured errors. The socket is
created with mode `0600`; the daemon refuses to replace a symlink or regular
file at the requested path.

This is a development transport, not a stable public API. Catalog schema and
transport versions are separate so either can evolve explicitly.


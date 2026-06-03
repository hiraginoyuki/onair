# Security And Privacy

onair's privacy target is backend anonymity from ordinary API-visible server
behavior. It hides simple protocol/configuration leaks while preserving
OpenAI-style compatibility. It is not a full traffic-analysis defense.

Timing, throughput, token rate, model quality, and other behavioral
fingerprints can still reveal information about the backing service.

## Backend Secrecy

- `[[backend]].base_url` must be an absolute `http` or `https` URL without
  embedded credentials, query strings, or fragments. Use `api_key` or
  `api_key_env` for backend credentials.
- `/v1/models` and `/v1/models/{model}` are synthesized from public config;
  backend model IDs are not listed.
- Model-bearing requests are rewritten to backend model IDs only after access
  checks pass.
- Successful JSON and SSE responses rewrite backend model IDs back to public
  model IDs when a model mapping is known.
- Non-success backend responses are converted to generic OpenAI-style errors;
  backend error bodies are discarded.
- Response headers use an allowlist. onair keeps useful API headers such as
  `content-type` and `content-disposition`, sets its own cache policy, and
  echoes only a client-supplied `x-request-id`.
- Backend anonymity covers protocol-visible signs. onair does not try to hide
  timing, throughput, token rate, model quality, or other behavioral
  fingerprints.

## Privacy Boundary

| Surface | Client-visible? | May expose backend details? |
| --- | --- | --- |
| `/v1/models` and `/v1/models/{model}` | yes | no, synthesized from public config |
| Proxied successful JSON/SSE responses | yes | backend model IDs are rewritten when a mapping is known |
| Proxied upstream non-success responses | yes | no, converted to generic OpenAI-style errors |
| Response headers | yes | only allowlisted headers plus client `x-request-id` |
| Debug capture files | local only | yes, may include exact request/upstream bodies |
| Inspector/operator endpoints | operator only | yes, may include backend IDs, URLs, model IDs, paths, and metadata |
| Logs | operator only | sanitized metadata, not prompt or completion bodies |

## Debug Capture Risk

Debug capture is default-off because it writes exact local troubleshooting
artifacts. Captures can include prompts, tool inputs, uploaded file bytes,
personal data, credentials sent in bodies, sensitive query parameters, backend
model IDs, and selected upstream error-response bodies.

Use debug capture only while reproducing a trusted local issue, then disable it
and delete the capture directory.

The default `onair-debug-captures` directory is ignored by this repository, but
custom directories are not automatically protected from commits, backups, or
sharing.

See [observability.md](observability.md#debug-capture) for exact capture file
names and modes.

## Inspector And Operator Endpoint Risk

The inspector does not store prompt or completion bodies. It can still expose
sensitive metadata such as model names, client IDs, source addresses, user
agents, query strings, request sizes, token counts, debug capture IDs, backend
IDs, backend URLs, backend model IDs, model visibility policy, and local
filesystem paths such as `debug_capture.directory`.

With `allow_remote = false`, inspector endpoints are only served when the
effective client address resolves to loopback after trusted-proxy header
processing. This is the default and is appropriate for
`bind = "127.0.0.1:8080"` plus a browser on the same host, while still
rejecting forwarded remote clients that arrive through a trusted proxy.

Set `allow_remote = true` only if the onair bind address is protected by
another access-control layer, such as SSH tunneling, a private VPN, or a
trusted reverse proxy with its own authentication.

Inspector data is memory-only by default. Optional SQLite persistence can
restore the latest retained records after process restart when
`[inspector.persistence].enabled = true`; it is not intended as a long-lived
audit log. Store the database at a private, ignored local path and treat the
database, `-wal`, and `-shm` sidecar files as sensitive local artifacts.

Increase `retention_requests` only as much as needed; config loading rejects
values above `100000` to avoid accidental unbounded memory growth or persisted
metadata growth.

Inspector responses use `Cache-Control: no-store`; avoid putting them behind
shared caches or public reverse proxies.

## Secret Hygiene

- Prefer `api_key_env` over inline `api_key` for backend and client secrets.
- Do not commit `onair.toml`, generated keys, debug captures, `.env` files, or
  local telemetry credentials.
- Do not paste raw debug-capture bodies, private URLs, private hostnames,
  tokens, or credentials into issues, pull requests, public docs, or chat
  transcripts.
- Before public sharing or release, scan tracked files, reachable git history,
  ignored local artifacts, and generated metadata for secrets and private
  environment details.

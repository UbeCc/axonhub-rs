# axonhub-rs

A small, database-free LLM gateway in Rust. It accepts Anthropic Messages and
OpenAI Chat Completions traffic on one local port, routes each request to an
upstream channel by model name, and streams the answer back without buffering.

The design goal is a boring hot path: parse only the top-level `model` field,
forward the original request bytes, stream the response, write nothing to disk.
Routing state is a single YAML file.

## Features

- `POST /anthropic/v1/messages` (and `POST /v1/messages`): Anthropic Messages
  passthrough. The body is forwarded byte-for-byte; only the top-level `model`
  value is rewritten when a channel maps an alias. Streaming responses pass
  through as they arrive.
- `POST /v1/chat/completions`: OpenAI Chat Completions, translated to the
  Anthropic Messages shape and translated back, including tool calls, images,
  reasoning text and SSE streaming.
- Model routing per channel: `models`, alias `mappings`, `hide_mapped_models`,
  and `priority`. On a retryable upstream error (408/409/425/429/5xx/529 or a
  connection failure) the next eligible channel is tried.
- No database. Optional access logging is off by default; health, route and
  Prometheus-style counters are in memory and reset on restart.
- Config reload with `SIGHUP` or `POST /admin/reload`; JWT/GraphQL/admin-console
  state from the previous gateway is not carried over.

## Build

```sh
cargo build --release
```

## Configure

Start from the example and edit it. Keep the file mode `0600` if it contains
literal keys; prefer `${ENV_VAR}` references.

```sh
cp config.example.yml config.yml
chmod 600 config.yml
$EDITOR config.yml
./target/release/axonhub-rs --config config.yml --check
```

`--check` validates the document and prints the channel and model counts
without binding a port.

### Migrating a routing table

If the previous gateway exposed its configuration through a local SQLite seed,
`scripts/import-sqlite.py` can read the channel and API-key tables and emit this
YAML. It only reads configuration tables; request, usage and trace rows are
never read.

```sh
python3 scripts/import-sqlite.py --seed /path/to/config.db \
  --api-key-file /path/to/api-key.json --out config.yml
```

## Run

```sh
./target/release/axonhub-rs --config config.yml
# or with a restart loop in tmux:
bash scripts/start-tmux.sh
```

The listener is loopback-only by default. Put authentication in front of it or
bind it to a private interface only.

## Operations

| Endpoint | Purpose |
|---|---|
| `GET /admin/system/status` | Liveness, version, uptime (same shape the old supervisor probes). |
| `GET /admin/status` | Channel/model counts and in-memory counters. |
| `GET /admin/routes` | Effective channel table, without credentials. |
| `POST /admin/reload` | Re-read `config.yml`; invalid config is rejected and the old one stays active. |
| `GET /metrics` | Prometheus text format. |
| `GET /health` | Plain liveness. |

Send `SIGHUP` for the same reload from a service manager.

The included `scripts/switch-to-rust.sh` stops a legacy gateway, starts this
one and health-checks it; `scripts/rollback-to-axonhub.sh` restores the legacy
service. Both take the legacy start command from `AXONHUB_LEGACY_START`, so no
deployment-specific path is baked into the repository.

## Tests and checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`scripts/smoke.sh` starts the Rust mock upstream and this gateway on loopback
and checks passthrough, alias rewriting, failover, OpenAI translation, auth
and streaming.

## Overhead

`scripts/bench.py` drives a gateway and an instant mock upstream with the same
client; the difference between the two runs is gateway overhead. A representative
loopback run on one machine, 1000 requests at 50 concurrent connections:

| path | p50 | p99 |
|---|---|---|
| direct to mock upstream | ~40 ms | ~256 ms |
| through axonhub-rs | ~41 ms | ~272 ms |

The absolute numbers are dominated by the benchmark client and loopback; the
roughly one millisecond delta is the relevant number. A gateway that writes
per-request rows to a network filesystem adds hundreds of milliseconds to
seconds under concurrency; that is the failure mode this project removes.

## What is intentionally out of scope

- The previous gateway's admin console and GraphQL API. Routing changes are
  YAML edits plus reload; `GET /admin/routes` shows the result.
- Request/usage/trace storage and the analytics built on it.
- Cross-host configuration synchronization. Each host owns its `config.yml`.
- Automatic upstream model discovery and channel probes. Model lists are
  explicit.
- Non-Anthropic upstream protocols. Current channel types are Anthropic-shaped;
  unsupported channel types are rejected instead of being proxied incorrectly.

## License

MIT. See [LICENSE](LICENSE).

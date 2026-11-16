# Changelog

## Unreleased

- Authentication on `/admin/status`, `/admin/routes`, `/admin/reload` and the model
  listings: `admin_api_keys` (falls back to `api_keys`), `x-api-key` or Bearer.
  `/health` and `/admin/system/status` stay open for supervisors and cutover probes.

## 0.1.0

- Initial release.
- Anthropic Messages passthrough with byte-preserving model rewrite.
- OpenAI Chat Completions translation, including streaming and tool calls.
- Per-channel model lists, aliases, hidden mapped models and priorities.
- Retry and failover across eligible channels.
- Config reload by `SIGHUP` or `POST /admin/reload`.
- In-memory health, route inspection and Prometheus-style counters.
- No database and no per-request durable writes.

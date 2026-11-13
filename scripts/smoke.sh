#!/bin/bash
# End-to-end smoke test: mock upstream + gateway on loopback.
set -euo pipefail
DIR=$(cd "$(dirname "$0")/.." && pwd)
MOCK_PORT=${MOCK_PORT:-18083}
GATEWAY_PORT=${GATEWAY_PORT:-18091}
BIN=$DIR/target/release/axonhub-rs
MOCK=$DIR/target/release/mock_upstream
TMP=$(mktemp -d)
MOCK_PID=
GW_PID=
cleanup() {
  [ -n "$GW_PID" ] && kill "$GW_PID" 2>/dev/null || true
  [ -n "$MOCK_PID" ] && kill "$MOCK_PID" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

[ -x "$BIN" ] || { echo "build first: cargo build --release" >&2; exit 1; }
[ -x "$MOCK" ] || { echo "build the mock first: cargo build --release --bin mock_upstream" >&2; exit 1; }

"$MOCK" "127.0.0.1:$MOCK_PORT" >"$TMP/mock.log" 2>&1 &
MOCK_PID=$!

cat >"$TMP/config.yml" <<YAML
listen: 127.0.0.1:$GATEWAY_PORT
api_keys: [test-key]
max_attempts: 3
default_max_tokens: 64
channels:
  - name: primary
    base_url: http://127.0.0.1:$MOCK_PORT
    api_key: mock-fail
    auth: x-api-key
    models: [mock-switch]
    priority: 10
  - name: secondary
    base_url: http://127.0.0.1:$MOCK_PORT
    api_key: mock-ok
    auth: x-api-key
    models: [mock-switch, mock-echo]
    priority: 1
  - name: mapped
    base_url: http://127.0.0.1:$MOCK_PORT
    api_key: mock-ok
    auth: x-api-key
    models: [mock-real]
    mappings:
      - from: mock-alias
        to: mock-real
    hide_mapped_models: true
    priority: 5
YAML

"$BIN" --config "$TMP/config.yml" --check >/dev/null
"$BIN" --config "$TMP/config.yml" >"$TMP/gateway.log" 2>&1 &
GW_PID=$!
for _ in $(seq 1 50); do
  curl -sf --noproxy '*' "http://127.0.0.1:$GATEWAY_PORT/health" >/dev/null && break
  sleep 0.1
done

base="http://127.0.0.1:$GATEWAY_PORT"
auth=(-H 'x-api-key: test-key' -H 'content-type: application/json')
pass=0
check() {
  local label=$1 expected=$2 actual=$3
  if [ "$expected" = "$actual" ]; then
    echo "ok   $label"
    pass=$((pass + 1))
  else
    echo "FAIL $label: expected [$expected] got [$actual]" >&2
    exit 1
  fi
}

check "auth required" 401 "$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' -H 'content-type: application/json' "$base/anthropic/v1/messages" -d '{"model":"mock-echo","max_tokens":8,"messages":[]}')"
check "passthrough" mock-echo "$(curl -s --noproxy '*' "${auth[@]}" "$base/anthropic/v1/messages" -d '{"model":"mock-echo","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["model"])')"
check "alias rewrite" mock-real "$(curl -s --noproxy '*' "${auth[@]}" "$base/anthropic/v1/messages" -d '{"model":"mock-alias","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["model"])')"
check "failover" secondary "$(curl -s --noproxy '*' -D - -o /dev/null "${auth[@]}" "$base/anthropic/v1/messages" -d '{"model":"mock-switch","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}' | tr -d '\r' | awk -F': ' 'tolower($1)=="x-gateway-channel"{print $2}')"
check "openai translation" hello "$(curl -s --noproxy '*' "${auth[@]}" "$base/v1/chat/completions" -d '{"model":"mock-alias","messages":[{"role":"user","content":"hi"}]}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["choices"][0]["message"]["content"])')"
check "stream event" 200 "$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' "${auth[@]}" "$base/anthropic/v1/messages" -d '{"model":"mock-echo","max_tokens":8,"stream":true,"messages":[{"role":"user","content":"hi"}]}')"
check "unknown model" 422 "$(curl -s --noproxy '*' -o /dev/null -w '%{http_code}' "${auth[@]}" "$base/anthropic/v1/messages" -d '{"model":"nope","max_tokens":8,"messages":[]}')"

echo "smoke test passed ($pass checks)"

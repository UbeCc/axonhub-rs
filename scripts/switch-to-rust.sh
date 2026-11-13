#!/bin/bash
# Cut over the local gateway port from a legacy gateway to axonhub-rs.
#
# Environment:
#   AXONHUB_LEGACY_START    shell command that starts the legacy gateway
#                           (used for automatic rollback); optional
#   AXONHUB_LEGACY_ROOTS   space-separated directories whose `axonhub` process
#                           may be stopped (default: empty; only the tmux
#                           session is stopped)
#   AXONHUB_LEGACY_SESSION tmux session of the legacy supervisor (default: axonhub)
set -euo pipefail
DIR=$(cd "$(dirname "$0")/.." && pwd)
URL=${AXONHUB_RS_STATUS_URL:-http://127.0.0.1:8090/admin/system/status}
SESSION=${AXONHUB_RS_SESSION:-axonhub-rs}
LEGACY_SESSION=${AXONHUB_LEGACY_SESSION:-axonhub}
LEGACY_START=${AXONHUB_LEGACY_START:-}
LEGACY_ROOTS=${AXONHUB_LEGACY_ROOTS:-}
PORT=${AXONHUB_RS_PORT:-8090}

probe_rust() {
  curl -s --noproxy '*' -m 3 "$URL" 2>/dev/null | grep -q '"version":"axonhub-rs/'
}
probe_any() {
  curl -s --noproxy '*' -m 3 "$URL" 2>/dev/null | grep -q '"isInitialized":true'
}

rollback() {
  echo "restoring the legacy gateway" >&2
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  if [ -n "$LEGACY_START" ]; then
    bash -c "$LEGACY_START" || true
  fi
}

if probe_rust; then
  echo "axonhub-rs already serves $URL"
  exit 0
fi
if ! probe_any; then
  echo "warning: nothing healthy on the gateway port before cutover" >&2
fi

echo "stopping the legacy supervisor and process"
tmux kill-session -t "$LEGACY_SESSION" 2>/dev/null || true
tmux kill-session -t "${LEGACY_SESSION}-gc" 2>/dev/null || true
for root in $LEGACY_ROOTS; do
  root=$(readlink -f "$root")
  for pid in $(pgrep -x axonhub 2>/dev/null || true); do
    if [ "$(readlink -f "/proc/$pid/cwd" 2>/dev/null)" = "$root" ]; then
      kill "$pid" 2>/dev/null || true
    fi
  done
done
for _ in $(seq 1 15); do
  ss -ltn 2>/dev/null | grep -q "127.0.0.1:${PORT}" || break
  sleep 1
done
if ss -ltn 2>/dev/null | grep -q "127.0.0.1:${PORT}"; then
  echo "the gateway port is still busy; aborting" >&2
  rollback
  exit 1
fi

echo "starting axonhub-rs"
if ! bash "$DIR/scripts/start-tmux.sh"; then
  echo "failed to start axonhub-rs" >&2
  rollback
  exit 1
fi

for _ in $(seq 1 30); do
  if probe_rust; then
    echo "axonhub-rs is serving the gateway port"
    exit 0
  fi
  sleep 1
done

echo "axonhub-rs did not become healthy" >&2
rollback
exit 1

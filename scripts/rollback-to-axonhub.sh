#!/bin/bash
# Stop axonhub-rs and restore the legacy gateway.
# Requires AXONHUB_LEGACY_START (shell command that starts the legacy gateway).
set -euo pipefail
SESSION=${AXONHUB_RS_SESSION:-axonhub-rs}
LEGACY_START=${AXONHUB_LEGACY_START:-}
if [ -z "$LEGACY_START" ]; then
  echo "AXONHUB_LEGACY_START is not set; stop axonhub-rs and start the legacy gateway manually" >&2
  exit 2
fi
tmux kill-session -t "$SESSION" 2>/dev/null || true
for _ in $(seq 1 15); do
  ss -ltn 2>/dev/null | grep -q "${AXONHUB_RS_PORT:-8090}" || break
  sleep 1
done
bash -c "$LEGACY_START"

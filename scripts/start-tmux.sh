#!/bin/bash
# Start axonhub-rs in the tmux session used by the deployment.
set -euo pipefail
DIR=$(cd "$(dirname "$0")/.." && pwd)
CONF=${AXONHUB_RS_CONFIG:-$DIR/config.yml}
SESSION=${AXONHUB_RS_SESSION:-axonhub-rs}
mkdir -p "$DIR/logs"

if [ ! -x "$DIR/target/release/axonhub-rs" ]; then
  echo "missing $DIR/target/release/axonhub-rs; run: cargo build --release" >&2
  exit 1
fi
if [ ! -f "$CONF" ]; then
  echo "missing $CONF; run scripts/export-config.py" >&2
  exit 1
fi
if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "tmux session $SESSION already exists" >&2
  exit 1
fi

tmux new-session -d -s "$SESSION" -c "$DIR" "exec bash '$DIR/scripts/supervise.sh'" 8>&-
echo "started tmux session $SESSION"

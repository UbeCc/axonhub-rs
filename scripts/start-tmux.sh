#!/bin/bash
# Start axonhub-rs in the tmux session used by the deployment.
set -euo pipefail
DIR=$(cd "$(dirname "$0")/.." && pwd)
SESSION=${AXONHUB_RS_SESSION:-axonhub-rs}
mkdir -p "$DIR/logs"

# Per-host config wins over the shared one.  The same rule is repeated in
# supervise.sh so a restart inside the tmux session resolves the same file.
CONF=${AXONHUB_RS_CONFIG:-$DIR/config.$(hostname).yml}
[ -f "$CONF" ] || CONF=$DIR/config.yml

if [ ! -x "$DIR/target/release/axonhub-rs" ]; then
  echo "missing $DIR/target/release/axonhub-rs; run: cargo build --release" >&2
  exit 1
fi
if [ ! -f "$CONF" ]; then
  echo "missing $CONF; run scripts/import-sqlite.py" >&2
  exit 1
fi
if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "tmux session $SESSION already exists" >&2
  exit 1
fi

# tmux starts the pane from the tmux server's environment, not this shell's;
# pass the selected config explicitly so a per-host file is not lost.
tmux new-session -d -s "$SESSION" -c "$DIR" \
  "export AXONHUB_RS_CONFIG='$CONF'; exec bash '$DIR/scripts/supervise.sh'" 8>&-
echo "started tmux session $SESSION with $CONF"

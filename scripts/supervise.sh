#!/bin/bash
# Restart loop for axonhub-rs (run inside tmux by start-tmux.sh).
set -u
DIR=$(cd "$(dirname "$0")/.." && pwd)
CONF=${AXONHUB_RS_CONFIG:-$DIR/config.$(hostname).yml}
[ -f "$CONF" ] || CONF=$DIR/config.yml
BIN=$DIR/target/release/axonhub-rs
mkdir -p "$DIR/logs"
while :; do
  echo "$(date -Is) starting axonhub-rs config=$CONF pid=$$"
  "$BIN" --config "$CONF" >>"$DIR/logs/gateway.log" 2>&1
  rc=$?
  echo "$(date -Is) axonhub-rs exited rc=$rc config=$CONF; restart in 2s" >>"$DIR/logs/supervisor.log"
  sleep 2
done

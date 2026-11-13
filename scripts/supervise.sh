#!/bin/bash
# Restart loop for axonhub-rs (run inside tmux by start-tmux.sh).
set -u
DIR=$(cd "$(dirname "$0")/.." && pwd)
CONF=${AXONHUB_RS_CONFIG:-$DIR/config.$(hostname).yml}
[ -f "$CONF" ] || CONF=$DIR/config.yml
BIN=$DIR/target/release/axonhub-rs
# Internal upstreams (api-gateway.glm.ai etc.) must not go through the
# external egress proxy: from the euler pods that proxy is slow and unstable
# for long streaming responses, and the internal gateway is reachable
# directly.  External upstreams (api.deepseek.com, cc.ixg.be, ...) still use
# the proxy from the environment.
export NO_PROXY="${NO_PROXY:+$NO_PROXY,}127.0.0.1,localhost,.glm.ai"
export no_proxy="$NO_PROXY"
mkdir -p "$DIR/logs"
while :; do
  echo "$(date -Is) starting axonhub-rs config=$CONF pid=$$"
  "$BIN" --config "$CONF" >>"$DIR/logs/gateway.log" 2>&1
  rc=$?
  echo "$(date -Is) axonhub-rs exited rc=$rc config=$CONF; restart in 2s" >>"$DIR/logs/supervisor.log"
  sleep 2
done

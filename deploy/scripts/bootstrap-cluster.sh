#!/usr/bin/env bash
# bootstrap-cluster.sh — one-shot helper to bring up a fresh 3-node
# Oxide-KV cluster on a single host for testing / development.
#
# Production deployments should use the systemd unit template
# (deploy/systemd/oxide-kv@.service) instead. This script is for
# the "I just want to see it work on my laptop" path.
#
# Usage:
#   ./deploy/scripts/bootstrap-cluster.sh start    # start 3 nodes
#   ./deploy/scripts/bootstrap-cluster.sh stop     # stop them
#   ./deploy/scripts/bootstrap-cluster.sh status   # who's leader?
#   ./deploy/scripts/bootstrap-cluster.sh clean    # nuke data dirs
#
# Logs go to /tmp/oxide-kv-node-{1,2,3}.log. Each node gets its own
# data dir under /tmp/oxide-kv-node-{1,2,3}. Override with $BASE.

set -euo pipefail

BASE="${BASE:-/tmp}"
NODES=(node-1 node-2 node-3)
BIN="${BIN:-./target/release/oxide-kv}"

cmd="${1:-}"

is_built() {
    [[ -x "$BIN" ]] || { echo "build first: cargo build --release"; exit 1; }
}

start_one() {
    local id="$1" idx="$2"
    local data_dir="$BASE/oxide-kv-$id"
    local raft_port=$((9000 + idx))
    local client_port=$((9100 + idx))
    local peers=""
    for ((n=0; n<${#NODES[@]}; n++)); do
        [[ "$n" -eq "$idx" ]] && continue
        local p_port=$((9000 + n + 1))
        peers+="127.0.0.1:$p_port,"
    done
    peers="${peers%,}"  # strip trailing comma

    mkdir -p "$data_dir"
    nohup "$BIN" \
        --addr "127.0.0.1:$raft_port" \
        --client-addr "127.0.0.1:$client_port" \
        --peers "$peers" \
        --data-dir "$data_dir" \
        > "$BASE/oxide-kv-$id.log" 2>&1 &
    echo $! > "$BASE/oxide-kv-$id.pid"
    echo "started $id (raft=$raft_port client=$client_port pid=$(cat "$BASE/oxide-kv-$id.pid"))"
}

stop_all() {
    for n in "${NODES[@]}"; do
        local pidfile="$BASE/oxide-kv-$n.pid"
        if [[ -f "$pidfile" ]]; then
            local pid
            pid=$(cat "$pidfile")
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid"
                echo "stopped $n (pid=$pid)"
            fi
            rm -f "$pidfile"
        fi
    done
}

status_all() {
    for n in "${NODES[@]}"; do
        local pidfile="$BASE/oxide-kv-$n.pid"
        if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
            echo "$n: running (pid=$(cat "$pidfile"))"
        else
            echo "$n: not running"
        fi
    done
}

clean_all() {
    stop_all
    for n in "${NODES[@]}"; do
        rm -rf "$BASE/oxide-kv-$n" "$BASE/oxide-kv-$n.log" "$BASE/oxide-kv-$n.pid"
    done
    echo "cleaned all data dirs under $BASE/"
}

case "$cmd" in
    start)
        is_built
        stop_all || true
        for idx in "${!NODES[@]}"; do
            start_one "${NODES[$idx]}" "$((idx + 1))"
        done
        echo "3 nodes started; logs: $BASE/oxide-kv-node-*.log"
        echo "wait ~2s for leader election, then check status"
        ;;
    stop)
        stop_all
        ;;
    status)
        status_all
        ;;
    clean)
        clean_all
        ;;
    *)
        echo "usage: $0 {start|stop|status|clean}" >&2
        exit 2
        ;;
esac
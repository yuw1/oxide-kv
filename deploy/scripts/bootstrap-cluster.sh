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
# Metrics ports: by default, node-N (1-indexed) binds
# 127.0.0.1:(9000 + N*100) (i.e. 9100, 9200, 9300). Override with
# OXIDE_METRICS_PORT_OFFSET or set `--metrics-addr disabled` via the
# wrapper script if you only want one node scraping the leader.

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
    local raft_port=$((9001 + idx))
    local client_port=$((9101 + idx))
    # Metrics port offset: P8 PR #8 ships the `/metrics` endpoint
    # and defaults to `127.0.0.1:9100`. Three nodes would collide on
    # that port, so use `9000 + 100*(idx+1)` (node-1=9100, node-2=9200,
    # node-3=9300). Set `OXIDE_METRICS_PORT_OFFSET=0` (or set
    # `--metrics-addr disabled` via env override) to revert to a
    # single-port binding for debugging.
    local metrics_offset="${OXIDE_METRICS_PORT_OFFSET:-100}"
    local metrics_port=$((9000 + metrics_offset * (idx + 1)))
    local peers=""
    for ((n=0; n<${#NODES[@]}; n++)); do
        [[ "$n" -eq "$idx" ]] && continue
        local p_port=$((9001 + n))
        peers+="127.0.0.1:$p_port,"
    done
    peers="${peers%,}"  # strip trailing comma

    mkdir -p "$data_dir"
    nohup "$BIN" \
        --addr "127.0.0.1:$raft_port" \
        --client-addr "127.0.0.1:$client_port" \
        --peers "$peers" \
        --data-dir "$data_dir" \
        --metrics-addr "127.0.0.1:$metrics_port" \
        > "$BASE/oxide-kv-$id.log" 2>&1 &
    local pid=$!
    echo $pid > "$BASE/oxide-kv-$id.pid"
    echo "started $id (raft=$raft_port client=$client_port metrics=$metrics_port pid=$pid)"

    # Append a JSON record so test / ops scripts can discover
    # each node's full port triplet without scraping logs. P8 PR #9
    # uses this to find the leader's client port after
    # `bootstrap-cluster.sh status` reports the cluster is up.
    # Format: one JSON object per line, newline-delimited (NDJSON).
    cat >> "$BASE/cluster.jsonl" <<EOF
{"node":"$id","raft":$raft_port,"client":$client_port,"metrics":$metrics_port,"pid":$pid,"data_dir":"$data_dir","log":"$BASE/oxide-kv-$id.log"}
EOF
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
    rm -f "$BASE/cluster.jsonl"
    echo "cleaned all data dirs under $BASE/"
}

case "$cmd" in
    start)
        is_built
        stop_all || true
        rm -f "$BASE/cluster.jsonl"
        for idx in "${!NODES[@]}"; do
            start_one "${NODES[$idx]}" "$idx"
        done
        echo "3 nodes started; logs: $BASE/oxide-kv-node-*.log"
        echo "wait ~2s for leader election, then check status"
        echo "node discovery: $BASE/cluster.jsonl (NDJSON, one record per node)"
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
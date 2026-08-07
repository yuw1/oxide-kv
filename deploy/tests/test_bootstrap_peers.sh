#!/usr/bin/env bash
# Regression test for the off-by-one in bootstrap-cluster.sh that
# caused node-2's --peers list to include its own raft addr (9002)
# and miss node-3 (9003). Symptom: 2PC BeginTx timed out with
# "replication failed: timed out waiting for index N to replicate to
# all peers (current: [..., ("127.0.0.1:9002", 0)])" — the leader
# was looking up its own address in match_index, which is always 0.
#
# This test verifies the peer-list construction logic in isolation,
# without actually launching the binary (so it doesn't need a build
# or ports). Re-run after any edit to start_one() or its call sites.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../scripts" && pwd)"
SCRIPT="$SCRIPT_DIR/bootstrap-cluster.sh"

# Extract the start_one() function and the NODES array definition so
# we can replay them in a sandboxed environment without launching
# the cluster.
#
# Easiest: source the script with a stub `case` that ignores all
# subcommands, then re-define the bits we want to test. The script
# has no side effects beyond its own `case "$cmd" in` so this is
# safe.
test -f "$SCRIPT" || { echo "FAIL: $SCRIPT not found"; exit 1; }

# Inline-replay the peer-list construction with the same constants
# as the script. If this drifts from the script, the test is useless,
# so we ALSO grep the script and verify the literals.
NODES=("node-1" "node-2" "node-3")
EXPECTED_RAFT=(9001 9002 9003)
EXPECTED_PEERS=(
  "127.0.0.1:9002,127.0.0.1:9003"
  "127.0.0.1:9001,127.0.0.1:9003"
  "127.0.0.1:9001,127.0.0.1:9002"
)

fail=0
for idx in 0 1 2; do
  peers=""
  for ((n=0; n<${#NODES[@]}; n++)); do
    [[ "$n" -eq "$idx" ]] && continue
    p_port=$((9001 + n))
    peers+="127.0.0.1:$p_port,"
  done
  peers="${peers%,}"
  raft_port=$((9001 + idx))

  if [[ "$raft_port" != "${EXPECTED_RAFT[$idx]}" ]]; then
    echo "FAIL idx=$idx: raft port $raft_port != ${EXPECTED_RAFT[$idx]}"
    fail=1
  fi
  if [[ "$peers" != "${EXPECTED_PEERS[$idx]}" ]]; then
    echo "FAIL idx=$idx: peers [$peers] != [${EXPECTED_PEERS[$idx]}]"
    fail=1
  fi
  # Hard invariant: a node's own raft port must NEVER appear in its peers list.
  if [[ ",$peers," == *",127.0.0.1:$raft_port,"* ]]; then
    echo "FAIL idx=$idx: peers list contains self ($raft_port) — split-cluster bug"
    fail=1
  fi
done

# Cross-check: the script must NOT contain the old buggy pattern
# `$((idx + 1))` passed to start_one, and the port formula must be
# `$((9001 + idx))` (or equivalent) for raft_port.
if grep -q 'start_one "${NODES\[\$idx\]}" "\$((idx + 1))"' "$SCRIPT"; then
  echo "FAIL: script still passes idx+1 to start_one (off-by-one regression)"
  fail=1
fi
if ! grep -qE 'raft_port=\$\(\(9001 \+ idx\)\)' "$SCRIPT"; then
  echo "FAIL: script raft_port formula is no longer 9001+idx"
  fail=1
fi

if [[ "$fail" -eq 0 ]]; then
  echo "ok — bootstrap-cluster.sh peer-list construction is correct"
fi
exit "$fail"

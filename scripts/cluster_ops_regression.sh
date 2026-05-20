#!/usr/bin/env bash
set -euo pipefail

remote_node="${1:-${JARVIS_REMOTE_NODE:-archiebald}}"
run_id="ops-smoke-$(date +%s)-$$"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

require jarvisctl
require jq

echo "heartbeat: local"
jarvisctl node heartbeat --output json | jq -e '.heartbeat_epoch_ms > 0' >/dev/null

if ssh -o BatchMode=yes -o ConnectTimeout=8 "${remote_node}" 'command -v jarvisctl >/dev/null' >/dev/null 2>&1; then
  echo "heartbeat: ${remote_node}"
  jarvisctl node heartbeat "${remote_node}" --output json | jq -e '.heartbeat_epoch_ms > 0' >/dev/null
else
  echo "heartbeat: skipped remote (${remote_node} is not reachable with batch SSH)"
fi

echo "doctor: heartbeat facts"
jarvisctl node doctor --output json |
  jq -e '.[] | select(.available == true) | .facts.heartbeat_age_seconds != null' >/dev/null

echo "relay: local and remote"
"$(dirname "$0")/cluster_relay_smoke.sh" "${remote_node}"

echo "operator-request: create and resolve"
request_id="$(
  jarvisctl operator-request create \
    --title "${run_id} approval" \
    --kind smoke \
    --severity low \
    --reason "Regression smoke for durable operator request lifecycle." \
    --requested-by cluster_ops_regression \
    --ttl-seconds 600 \
    --output json |
    jq -r '.id'
)"
jarvisctl operator-request resolve "${request_id}" \
  --status approved \
  --decision "Approved regression smoke." \
  --decided-by cluster_ops_regression \
  --response-json '"accept"' \
  --output json |
  jq -e '.status == "approved"' >/dev/null

echo "relay retention: dry-run"
jarvisctl message prune --max-age-days 0 --output json |
  jq -e '.[0].scanned >= 0' >/dev/null

echo "cluster ops regression: ok (${run_id})"

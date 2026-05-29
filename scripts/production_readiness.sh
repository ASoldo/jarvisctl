#!/usr/bin/env bash
set -euo pipefail

remote_node="${1:-${JARVIS_REMOTE_NODE:-archiebald}}"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

require jarvisctl
require jq

echo "== local heartbeat =="
jarvisctl node heartbeat --output json | jq -e '.heartbeat_epoch_ms > 0' >/dev/null

echo "== heartbeat timer =="
jarvisctl node heartbeat-service-status --output json |
  jq -r '[.timer_active, .timer_enabled] | @tsv'

echo "== local codex doctor =="
codex doctor --json |
  jq -r '[.overallStatus, .codexVersion, .checks["auth.credentials"].status, .checks["runtime.search"].status, .checks["state.paths"].status, .checks["state.rollout_db_parity"].status, .checks["network.websocket_reachability"].status, .checks["updates.status"].status] | @tsv'

if ssh -o BatchMode=yes -o ConnectTimeout=8 "${remote_node}" 'command -v jarvisctl >/dev/null' >/dev/null 2>&1; then
  echo "== remote heartbeat: ${remote_node} =="
  jarvisctl node heartbeat "${remote_node}" --output json | jq -e '.heartbeat_epoch_ms > 0' >/dev/null
  ssh -o BatchMode=yes -o ConnectTimeout=8 "${remote_node}" \
    'jarvisctl node heartbeat-service-status --output json | jq -r '\''[.timer_active, .timer_enabled] | @tsv'\'''
  echo "== remote codex doctor: ${remote_node} =="
  ssh -o BatchMode=yes -o ConnectTimeout=8 "${remote_node}" \
    'codex doctor --json | jq -r '\''[.overallStatus, .codexVersion, .checks["auth.credentials"].status, .checks["runtime.search"].status, .checks["state.paths"].status, .checks["state.rollout_db_parity"].status, .checks["network.websocket_reachability"].status, .checks["updates.status"].status] | @tsv'\'''
else
  echo "== remote heartbeat: skipped (${remote_node} is not reachable with batch SSH) =="
fi

echo "== node doctor =="
jarvisctl node doctor --output json |
  jq -r '.[] | [.node, .available, .schedulable, ((.issues // []) | join(";"))] | @tsv'

echo "== node preflight =="
set +e
preflight_json="$(jarvisctl node preflight --output json 2>&1)"
preflight_status=$?
set -e
if [ "${preflight_status}" -eq 0 ]; then
  printf '%s\n' "${preflight_json}" | jq -r '[.ok, ((.issues // []) | join(";"))] | @tsv'
else
  printf '%s\n' "${preflight_json}" >&2
  echo "preflight_failed=true"
fi

echo "== relay retention dry-run =="
jarvisctl message prune --max-age-days 14 --cluster --output json |
  jq -r '(if type == "array" then .[] else . end) | [.scanned, .removed, .kept_recent, .kept_pending, .dry_run] | @tsv'

echo "== autonomy service =="
jarvisctl autonomy service-status --output json |
  jq -r '[.timer_active, .timer_enabled, .linger] | @tsv'

echo "== capability validation =="
jarvisctl capability validate --output json |
  jq -r 'if type == "array" then [all(.[]; .status == "passed"), ([.[] | .failed] | add // 0)] else [(.ok // (.status == "passed")), ((.issues // []) | length)] end | @tsv'

echo "production readiness: checked"

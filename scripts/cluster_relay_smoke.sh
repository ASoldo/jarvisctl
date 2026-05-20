#!/usr/bin/env bash
set -euo pipefail

remote_node="${1:-${JARVIS_REMOTE_NODE:-archiebald}}"
namespace="relay-smoke-$(date +%s)-$$"

require() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 2
  fi
}

require jarvisctl
require jq

echo "local relay queue: namespace=${namespace}"
local_id="$(
  jarvisctl message send \
    --to-namespace "${namespace}" \
    --text "local relay smoke ${namespace}" \
    --output json |
    jq -r '.id'
)"
jarvisctl message list --namespace "${namespace}" --all --output json |
  jq -e --arg id "${local_id}" '.[] | select(.id == $id and .status == "pending")' >/dev/null
jarvisctl message ack "${local_id}" --output json |
  jq -e '.status == "acked"' >/dev/null
echo "local relay queue: ok (${local_id})"

if ! ssh -o BatchMode=yes -o ConnectTimeout=8 "${remote_node}" 'command -v jarvisctl >/dev/null' >/dev/null 2>&1; then
  echo "remote relay queue: skipped (${remote_node} is not reachable with batch SSH)"
  exit 0
fi

remote_namespace="${namespace}-${remote_node}"
echo "remote relay queue: node=${remote_node} namespace=${remote_namespace}"
remote_id="$(
  ssh "${remote_node}" \
    "jarvisctl message send --to-namespace '${remote_namespace}' --text 'remote relay smoke ${remote_namespace}' --output json" |
    jq -r '.id'
)"
jarvisctl message list --namespace "${remote_namespace}" --all --cluster --output json |
  jq -e --arg id "${remote_id}" --arg node "${remote_node}" \
    '.[] | select(.id == $id and .source_node == $node and .status == "pending")' >/dev/null
jarvisctl message ack "${remote_id}" --node "${remote_node}" --output json |
  jq -e '.status == "acked"' >/dev/null
echo "remote relay queue: ok (${remote_id})"
